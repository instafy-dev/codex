use super::*;

#[derive(Default)]
struct BlockingStartup {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    block: AtomicBool,
}

impl codex_extension_api::ThreadLifecycleContributor<Config> for BlockingStartup {
    fn on_thread_start<'a>(
        &'a self,
        _input: codex_extension_api::ThreadStartInput<'a, Config>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if self.block.load(Ordering::Acquire) {
                self.entered.notify_one();
                self.release.notified().await;
            }
        })
    }
}

async fn manager_with_blocking_startup() -> (
    tempfile::TempDir,
    Config,
    Arc<ThreadManager>,
    Arc<BlockingStartup>,
) {
    let temp_dir = tempdir().expect("tempdir");
    let mut config = test_config().await;
    config.codex_home = temp_dir.path().join("codex-home").abs();
    config.cwd = config.codex_home.abs();
    std::fs::create_dir_all(&config.codex_home).expect("create codex home");
    let observer = Arc::new(BlockingStartup::default());
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(observer.clone());
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = Arc::new(ThreadManager::new(
        &config,
        Arc::clone(&auth_manager),
        build_models_manager(&config, auth_manager),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(extensions.build()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        passthrough_image_store(),
        thread_store_from_config(&config, /*state_db*/ None),
        /*agent_graph_store*/ None,
        TEST_INSTALLATION_ID.to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    ));
    (temp_dir, config, manager, observer)
}

async fn wait_for_admission_to_close(manager: &ThreadManager) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !manager.state.shutdown_started.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown must close admission");
}

#[tokio::test]
async fn shutdown_drains_threads_registered_after_its_initial_snapshot() {
    let (_home, config, manager, observer) = manager_with_blocking_startup().await;
    observer.block.store(true, Ordering::Release);
    let starting = tokio::spawn({
        let manager = Arc::clone(&manager);
        let config = config.clone();
        async move { manager.start_thread(StartThreadOptions::new(config)).await }
    });
    tokio::time::timeout(Duration::from_secs(5), observer.entered.notified())
        .await
        .expect("startup must reach the lifecycle callback");
    assert!(manager.list_thread_ids().await.is_empty());
    let shutting_down = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move {
            manager
                .shutdown_all_threads_bounded(Duration::from_secs(10))
                .await
        }
    });
    wait_for_admission_to_close(&manager).await;
    assert!(
        manager
            .start_thread(StartThreadOptions::new(config))
            .await
            .is_err()
    );
    observer.release.notify_one();
    let started = starting
        .await
        .expect("join startup")
        .expect("admitted thread starts");
    let report = shutting_down.await.expect("join shutdown");
    assert!(report.is_complete());
    assert_eq!(report.completed, vec![started.thread_id]);
    assert!(manager.list_thread_ids().await.is_empty());
}

#[tokio::test]
async fn shutdown_reports_pending_startup_and_still_stops_registered_threads() {
    let (_home, config, manager, observer) = manager_with_blocking_startup().await;
    let existing = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start existing thread");
    observer.block.store(true, Ordering::Release);
    let starting = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.start_thread(StartThreadOptions::new(config)).await }
    });
    tokio::time::timeout(Duration::from_secs(5), observer.entered.notified())
        .await
        .expect("startup must block");
    let report = tokio::time::timeout(
        Duration::from_secs(5),
        manager.shutdown_all_threads_bounded(Duration::from_secs(1)),
    )
    .await
    .expect("shutdown must stay bounded");
    assert!(report.admission_failed);
    assert!(!report.is_complete());
    assert_eq!(report.completed, vec![existing.thread_id]);
    observer.release.notify_one();
    let started = starting
        .await
        .expect("join startup")
        .expect("finish admitted startup");
    let report = manager
        .shutdown_all_threads_bounded(Duration::from_secs(10))
        .await;
    assert!(report.is_complete());
    assert_eq!(report.completed, vec![started.thread_id]);
}

#[tokio::test]
async fn canceling_shutdown_keeps_admission_closed_for_retry() {
    let (_home, config, manager, observer) = manager_with_blocking_startup().await;
    observer.block.store(true, Ordering::Release);
    let starting = tokio::spawn({
        let manager = Arc::clone(&manager);
        let config = config.clone();
        async move { manager.start_thread(StartThreadOptions::new(config)).await }
    });
    tokio::time::timeout(Duration::from_secs(5), observer.entered.notified())
        .await
        .expect("startup must block");
    let shutting_down = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move {
            manager
                .shutdown_all_threads_bounded(Duration::from_secs(10))
                .await
        }
    });
    wait_for_admission_to_close(&manager).await;
    shutting_down.abort();
    assert!(
        shutting_down
            .await
            .expect_err("shutdown task canceled")
            .is_cancelled()
    );
    observer.release.notify_one();
    starting
        .await
        .expect("join startup")
        .expect("finish admitted startup");
    assert!(
        manager
            .start_thread(StartThreadOptions::new(config))
            .await
            .is_err()
    );
    assert!(
        manager
            .shutdown_all_threads_bounded(Duration::from_secs(10))
            .await
            .is_complete()
    );
    assert!(manager.list_thread_ids().await.is_empty());
}

#[tokio::test]
async fn canceled_partial_startup_cannot_be_reported_as_clean() {
    let (_home, config, manager, observer) = manager_with_blocking_startup().await;
    observer.block.store(true, Ordering::Release);
    let starting = tokio::spawn({
        let manager = Arc::clone(&manager);
        async move { manager.start_thread(StartThreadOptions::new(config)).await }
    });
    tokio::time::timeout(Duration::from_secs(5), observer.entered.notified())
        .await
        .expect("startup must block");
    starting.abort();
    assert!(matches!(starting.await, Err(error) if error.is_cancelled()));
    assert!(manager.list_thread_ids().await.is_empty());
    for _ in 0..2 {
        let report = manager
            .shutdown_all_threads_bounded(Duration::from_secs(1))
            .await;
        assert!(report.admission_failed);
        assert!(!report.is_complete());
    }
}
