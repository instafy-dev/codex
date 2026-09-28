//! Instafy's model proxy answers an upstream rate limit with a retryable 429 and a
//! Retry-After, and leaves the retry to the client. These tests run whole turns
//! against that answer to check the wait before the retry and the text a person
//! sees when the retries run out.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const RATE_LIMITED_TURN_ERROR: &str = "stream disconnected before completion: 429 Too Many Requests: The upstream provider rate limit was reached.";

fn proxy_rate_limit_response() -> ResponseTemplate {
    // The smallest Retry-After the client honors keeps the test to about a second.
    ResponseTemplate::new(429)
        .insert_header("retry-after", "1")
        .set_body_json(serde_json::json!({
            "error": {
                "message": "The upstream provider rate limit was reached.",
                "type": "upstream_error",
                "code": "upstream_rate_limit",
                "retryable": true
            }
        }))
}

/// Serves `responses` in order and records when each request arrived, so a test
/// can measure the wait between an attempt and its retry.
struct TimedSequence {
    responses: Vec<ResponseTemplate>,
    arrivals: Arc<Mutex<Vec<Instant>>>,
}

impl Respond for TimedSequence {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let mut arrivals = self.arrivals.lock().expect("arrivals lock");
        arrivals.push(Instant::now());
        let call = arrivals.len();
        self.responses
            .get(call - 1)
            .cloned()
            .unwrap_or_else(|| panic!("no response for request {call}"))
    }
}

async fn mount_timed_sequence(
    server: &MockServer,
    responses: Vec<ResponseTemplate>,
) -> Arc<Mutex<Vec<Instant>>> {
    let arrivals = Arc::new(Mutex::new(Vec::new()));
    let expected_requests = responses.len() as u64;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(TimedSequence {
            responses,
            arrivals: Arc::clone(&arrivals),
        })
        .up_to_n_times(expected_requests)
        .expect(expected_requests)
        .mount(server)
        .await;
    arrivals
}

async fn build_codex_with_one_stream_retry(server: &MockServer) -> Result<TestCodex> {
    // One stream retry matches a bounded Instafy proxy run, and no request-level
    // retries keeps every attempt visible to the mock.
    test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
        })
        .build(server)
        .await
}

async fn submit_user_message(test: &TestCodex, text: &str) -> Result<()> {
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: text.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retryable_proxy_rate_limit_waits_for_retry_after_then_completes_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let arrivals = mount_timed_sequence(
        &server,
        vec![
            proxy_rate_limit_response(),
            sse_response(sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-1"),
            ])),
        ],
    )
    .await;
    let test = build_codex_with_one_stream_retry(&server).await?;

    submit_user_message(&test, "hello").await?;

    let mut stream_errors = Vec::new();
    let completed = loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::StreamError(event) => stream_errors.push(event),
            EventMsg::Error(event) => panic!("the retry should have completed the turn: {event:?}"),
            EventMsg::TurnComplete(event) => break event,
            _ => {}
        }
    };

    assert_eq!(completed.error, None);
    assert_eq!(completed.last_agent_message.as_deref(), Some("done"));
    assert_eq!(stream_errors.len(), 1, "stream errors: {stream_errors:?}");
    assert_eq!(
        stream_errors[0].additional_details.as_deref(),
        Some(RATE_LIMITED_TURN_ERROR)
    );

    let arrivals = arrivals.lock().expect("arrivals lock").clone();
    assert_eq!(arrivals.len(), 2);
    let wait = arrivals[1].duration_since(arrivals[0]);
    // Retry-After asks for one second and jitter adds at most a fifth of that. The
    // generic backoff would retry in about 200 ms and the default for a missing
    // header waits five seconds, so this window shows the header was honored.
    assert!(
        (Duration::from_secs(1)..Duration::from_secs(4)).contains(&wait),
        "retried after {wait:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retryable_proxy_rate_limit_reports_429_when_stream_retries_run_out() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let arrivals = mount_timed_sequence(
        &server,
        vec![proxy_rate_limit_response(), proxy_rate_limit_response()],
    )
    .await;
    let test = build_codex_with_one_stream_retry(&server).await?;

    submit_user_message(&test, "hello").await?;

    let mut errors = Vec::new();
    let completed = loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::Error(event) => errors.push(event),
            EventMsg::TurnComplete(event) => break event,
            _ => {}
        }
    };

    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    let error = errors.remove(0);
    assert_eq!(error.message, RATE_LIMITED_TURN_ERROR);
    // A stream error has no status code, so the structured info stays generic and
    // the text is what still says 429.
    assert_eq!(error.codex_error_info, Some(CodexErrorInfo::Other));
    assert_eq!(completed.error, Some(error));
    assert_eq!(arrivals.lock().expect("arrivals lock").len(), 2);

    Ok(())
}
