use super::*;

struct FutureDropMarker(Arc<std::sync::atomic::AtomicBool>);

impl Drop for FutureDropMarker {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

struct DropObservedTask {
    future_dropped: Arc<std::sync::atomic::AtomicBool>,
    started: Arc<tokio::sync::Notify>,
}

struct CancellationObservedTask {
    future_dropped: Arc<std::sync::atomic::AtomicBool>,
    started: Arc<tokio::sync::Notify>,
    cancellation_observed: Arc<tokio::sync::Notify>,
}

impl SessionTask for DropObservedTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.drop_observed"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let _drop_marker = FutureDropMarker(Arc::clone(&self.future_dropped));
        self.started.notify_waiters();
        std::future::pending::<SessionTaskResult>().await
    }
}

impl SessionTask for CancellationObservedTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.cancellation_observed"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<Session>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let _drop_marker = FutureDropMarker(Arc::clone(&self.future_dropped));
        self.started.notify_waiters();
        cancellation_token.cancelled().await;
        self.cancellation_observed.notify_waiters();
        std::future::pending::<SessionTaskResult>().await
    }
}

#[tokio::test]
async fn abort_all_tasks_waits_until_the_turn_future_is_dropped() {
    let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
    let future_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let started_wait = started.notified();
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            DropObservedTask {
                future_dropped: Arc::clone(&future_dropped),
                started: Arc::clone(&started),
            },
        )
        .await;
    timeout(Duration::from_secs(1), started_wait)
        .await
        .expect("turn task should start");

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;

    assert!(future_dropped.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test]
async fn abort_all_tasks_joins_an_already_cancelled_turn() {
    let (session, turn_context, _rx) = make_session_and_context_with_rx().await;
    let future_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let started_wait = started.notified();
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            DropObservedTask {
                future_dropped: Arc::clone(&future_dropped),
                started: Arc::clone(&started),
            },
        )
        .await;
    timeout(Duration::from_secs(1), started_wait)
        .await
        .expect("turn task should start");
    session
        .active_turn
        .lock()
        .await
        .as_ref()
        .and_then(|turn| turn.task.as_ref())
        .expect("running task")
        .cancellation_token
        .cancel();

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;

    assert!(future_dropped.load(std::sync::atomic::Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_does_not_restart_pending_trigger_work_before_shutdown_complete() {
    let (session, turn_context, rx) = make_session_and_context_with_rx().await;
    let future_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let started_wait = started.notified();
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            DropObservedTask {
                future_dropped: Arc::clone(&future_dropped),
                started: Arc::clone(&started),
            },
        )
        .await;
    timeout(Duration::from_secs(1), started_wait)
        .await
        .expect("active turn should start");
    let active_turn_id = turn_context.sub_id.clone();

    session
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("worker path should parse"),
                AgentPath::root(),
                Vec::new(),
                "pending trigger during shutdown".to_string(),
                /*trigger_turn*/ true,
            ),
            Default::default(),
        )
        .await;

    let shutdown_session = Arc::clone(&session);
    let shutdown_task = tokio::spawn(async move {
        handlers::shutdown(&shutdown_session, "shutdown-sub-id".to_string()).await
    });

    timeout(Duration::from_secs(5), async {
        loop {
            let event = rx.recv().await.expect("shutdown event channel open");
            match event.msg {
                EventMsg::TurnStarted(event) if event.turn_id != active_turn_id => {
                    panic!("shutdown started queued work as turn {}", event.turn_id);
                }
                EventMsg::ShutdownComplete => {
                    assert!(
                        session.active_turn.lock().await.is_none(),
                        "ShutdownComplete must not be emitted with an active turn"
                    );
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("shutdown should reach ShutdownComplete");

    assert!(shutdown_task.await.expect("shutdown task should join"));
    assert!(future_dropped.load(std::sync::atomic::Ordering::SeqCst));
    assert!(
        session.input_queue.has_trigger_turn_mailbox_items().await,
        "shutdown must leave queued trigger mail unstarted"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_a_concurrent_guardian_abort_to_join_the_turn() {
    let (session, turn_context, rx) = make_session_and_context_with_rx().await;
    let future_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let started = Arc::new(tokio::sync::Notify::new());
    let cancellation_observed = Arc::new(tokio::sync::Notify::new());
    let started_wait = started.notified();
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            CancellationObservedTask {
                future_dropped: Arc::clone(&future_dropped),
                started: Arc::clone(&started),
                cancellation_observed: Arc::clone(&cancellation_observed),
            },
        )
        .await;
    timeout(Duration::from_secs(1), started_wait)
        .await
        .expect("active turn should start");

    let cancellation_wait = cancellation_observed.notified();
    let guardian_session = Arc::clone(&session);
    let guardian_turn_id = turn_context.sub_id.clone();
    let guardian_abort = tokio::spawn(async move {
        guardian_session
            .abort_turn_if_active(&guardian_turn_id, TurnAbortReason::Interrupted)
            .await
    });
    timeout(Duration::from_secs(1), cancellation_wait)
        .await
        .expect("guardian abort should cancel the active task");

    let shutdown_session = Arc::clone(&session);
    let shutdown_task = tokio::spawn(async move {
        handlers::shutdown(&shutdown_session, "shutdown-sub-id".to_string()).await
    });

    timeout(Duration::from_secs(5), async {
        loop {
            let event = rx.recv().await.expect("shutdown event channel open");
            if matches!(event.msg, EventMsg::ShutdownComplete) {
                assert!(
                    future_dropped.load(std::sync::atomic::Ordering::SeqCst),
                    "ShutdownComplete must wait until the concurrently aborted future is dropped"
                );
                assert!(session.active_turn.lock().await.is_none());
                break;
            }
        }
    })
    .await
    .expect("shutdown should reach ShutdownComplete");

    assert!(guardian_abort.await.expect("guardian abort should join"));
    assert!(shutdown_task.await.expect("shutdown task should join"));
}
