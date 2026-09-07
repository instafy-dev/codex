use super::SessionIo;
use super::handlers::submission_loop;
use super::session_loop_termination_from_handle;
use super::tests::make_session_and_context;
use crate::agent::AgentStatus;
use crate::tools::code_mode::CodeModeService;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeSessionProviderFuture;
use codex_code_mode::CodeModeSessionResultFuture;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::StartedCell;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Submission;
use std::sync::Arc;
use tokio::sync::watch;

struct FailingShutdownSession;

impl CodeModeSession for FailingShutdownSession {
    fn execute<'a>(
        &'a self,
        _request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell> {
        Box::pin(async { Err("inert fixture does not execute code".to_string()) })
    }

    fn wait<'a>(&'a self, _request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { unreachable!("fixture has no live cells") })
    }

    fn terminate<'a>(&'a self, _cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome> {
        Box::pin(async { unreachable!("fixture has no live cells") })
    }

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()> {
        Box::pin(async { Err("fixture host did not confirm shutdown".to_string()) })
    }
}

struct FailingShutdownProvider;

impl CodeModeSessionProvider for FailingShutdownProvider {
    fn create_session<'a>(
        &'a self,
        _delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a> {
        Box::pin(async { Ok(Arc::new(FailingShutdownSession) as Arc<dyn CodeModeSession>) })
    }
}

#[tokio::test]
async fn failed_cleanup_is_reported_to_every_shutdown_waiter_without_consuming_events() {
    let (mut session, turn) = make_session_and_context().await;
    session.services.code_mode_service = CodeModeService::new(
        Arc::new(FailingShutdownProvider),
        &turn.config.code_mode,
        /*executed_tool_calls*/ None,
    );
    // Initialize the synthetic session without executing code or starting a process.
    session
        .services
        .code_mode_service
        .execute(ExecuteRequest {
            tool_call_id: "inert-shutdown-fixture".to_string(),
            enabled_tools: Vec::new(),
            source: String::new(),
            yield_time_ms: None,
            max_output_tokens: None,
        })
        .await
        .expect_err("the inert fixture never executes code");
    let (tx_event, rx_event) = async_channel::unbounded();
    session.tx_event = tx_event;
    let (tx_sub, rx_sub) = async_channel::bounded::<Submission>(4);
    let loop_handle = tokio::spawn(submission_loop(Arc::new(session), turn.config, rx_sub));
    let io = SessionIo {
        tx_sub,
        rx_event,
        agent_status: watch::channel(AgentStatus::PendingInit).1,
        session_loop_termination: session_loop_termination_from_handle(loop_handle),
    };
    let (first, second) = tokio::join!(io.shutdown_and_wait(), io.shutdown_and_wait());
    let first = first.expect_err("cleanup failure must reach first waiter");
    let second = second.expect_err("cleanup failure must reach second waiter");
    assert_eq!(first.to_string(), second.to_string());
    assert!(
        first
            .to_string()
            .contains("fixture host did not confirm shutdown")
    );
    assert_eq!(
        io.shutdown_and_wait()
            .await
            .expect_err("later waiters must retain the failure")
            .to_string(),
        first.to_string()
    );
    let mut cleanup_error_seen = false;
    while let Ok(event) = io.rx_event.try_recv() {
        match event.msg {
            EventMsg::Error(error) => {
                cleanup_error_seen |=
                    error.message == "Codex shutdown cleanup could not be confirmed";
            }
            EventMsg::ShutdownComplete => panic!("failed cleanup was announced as complete"),
            _ => {}
        }
    }
    assert!(
        cleanup_error_seen,
        "normal event consumers must retain the cleanup error"
    );
}

#[tokio::test]
async fn failed_cleanup_on_submission_channel_close_reaches_termination_waiter() {
    let (mut session, turn) = make_session_and_context().await;
    session.services.code_mode_service = CodeModeService::new(
        Arc::new(FailingShutdownProvider),
        &turn.config.code_mode,
        /*executed_tool_calls*/ None,
    );
    session
        .services
        .code_mode_service
        .execute(ExecuteRequest {
            tool_call_id: "inert-channel-close-fixture".to_string(),
            enabled_tools: Vec::new(),
            source: String::new(),
            yield_time_ms: None,
            max_output_tokens: None,
        })
        .await
        .expect_err("the inert fixture never executes code");
    let (tx_sub, rx_sub) = async_channel::bounded::<Submission>(1);
    let loop_handle = tokio::spawn(submission_loop(Arc::new(session), turn.config, rx_sub));
    drop(tx_sub);
    let error = session_loop_termination_from_handle(loop_handle)
        .await
        .expect_err("channel-close cleanup failure must remain observable");
    assert!(error.contains("fixture host did not confirm shutdown"));
}

#[tokio::test]
async fn canceled_session_loop_does_not_confirm_cleanup() {
    let handle = tokio::spawn(std::future::pending::<anyhow::Result<()>>());
    handle.abort();
    let error = session_loop_termination_from_handle(handle)
        .await
        .expect_err("aborting the loop does not prove resource cleanup");
    assert!(error.contains("ended without confirmed cleanup"));
}
