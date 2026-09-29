//! Instafy bills every response the upstream produces, including one that stops early.
//! A retry sends the whole request again, and a response cut off by max_output_tokens
//! or stopped by content_filter would most likely end the same way. These tests run
//! whole turns and compactions with a stream retry budget to check that such a response
//! is requested once and ends the turn with its reason.

use anyhow::Result;
use codex_login::CodexAuth;
use codex_models_manager::bundled_models_response;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::StreamErrorEvent;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_message_item_added;
use core_test_support::responses::ev_output_text_delta;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_models_once;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

/// Serves the same incomplete response to every request, so a retry would reach the
/// mock and be counted rather than fail for lack of a response.
async fn mount_incomplete_response(server: &MockServer, reason: &str) {
    let body = sse(vec![
        ev_response_created("resp_incomplete"),
        ev_message_item_added("msg_incomplete", "partial content"),
        ev_output_text_delta("continued chunk"),
        json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_incomplete",
                "object": "response",
                "status": "incomplete",
                "error": null,
                "incomplete_details": { "reason": reason },
                "usage": {
                    "input_tokens": 12,
                    "input_tokens_details": null,
                    "output_tokens": 34,
                    "output_tokens_details": null,
                    "total_tokens": 46
                }
            }
        }),
    ]);
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(sse_response(body))
        .mount(server)
        .await;
}

/// Finishes the first request, a normal turn, with `completed`, and answers every later
/// one, the compaction and anything that would send it again, with an incomplete response.
async fn mount_completed_then_incomplete_responses(
    server: &MockServer,
    completed: Value,
    reason: &str,
) {
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(sse_response(sse(vec![
            ev_response_created("resp_first"),
            ev_assistant_message("msg_first", "first reply"),
            completed,
        ])))
        .up_to_n_times(1)
        .mount(server)
        .await;
    mount_incomplete_response(server, reason).await;
}

/// The JSON body of every request that reached the Responses endpoint, in order.
async fn responses_request_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.method.as_str() == "POST")
        .filter(|request| request.url.path().ends_with("/responses"))
        .map(|request| {
            let zstd = request
                .headers
                .get("content-encoding")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.eq_ignore_ascii_case("zstd"));
            let body = if zstd {
                zstd::stream::decode_all(request.body.as_slice()).expect("zstd request body")
            } else {
                request.body.clone()
            };
            serde_json::from_slice(&body).expect("JSON request body")
        })
        .collect()
}

fn is_compaction_request(body: &Value) -> bool {
    body["input"]
        .as_array()
        .and_then(|input| input.last())
        .is_some_and(|item| item == &json!({ "type": "compaction_trigger" }))
}

fn user_turn(text: &str, model: Option<&str>) -> Op {
    let collaboration_mode = model.map(|model| CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model: model.to_string(),
            reasoning_effort: None,
            developer_instructions: None,
        },
    });
    Op::UserInput {
        items: vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings: ThreadSettingsOverrides {
            collaboration_mode,
            ..Default::default()
        },
    }
}

/// Collects the retry notices and errors of the running turn until it completes.
async fn turn_errors(
    codex: &codex_core::CodexThread,
) -> (Vec<StreamErrorEvent>, Vec<ErrorEvent>, TurnCompleteEvent) {
    let mut stream_errors = Vec::new();
    let mut errors = Vec::new();
    loop {
        match wait_for_event(codex, |_| true).await {
            EventMsg::StreamError(event) => stream_errors.push(event),
            EventMsg::Error(event) => errors.push(event),
            EventMsg::TurnComplete(event) => return (stream_errors, errors, event),
            _ => {}
        }
    }
}

/// A copy of the bundled gpt-5.4 entry under another slug and context window.
fn model_info_with_context_window(slug: &str, context_window: i64) -> ModelInfo {
    let mut model_info = bundled_models_response()
        .expect("bundled models.json should parse")
        .models
        .into_iter()
        .find(|model| model.slug == "gpt-5.4")
        .expect("gpt-5.4 missing from models.json");
    model_info.slug = slug.to_string();
    model_info.context_window = Some(context_window);
    model_info
}

async fn assert_incomplete_response_is_requested_once(reason: &str) -> Result<()> {
    let server = start_mock_server().await;
    mount_incomplete_response(&server, reason).await;
    // Five stream retries is the provider default that browser sessions keep, and no
    // request-level retries keeps every attempt visible to the mock.
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(5);
        })
        .build(&server)
        .await?;

    test.codex
        .submit(user_turn("trigger incomplete", /*model*/ None))
        .await?;
    let (stream_errors, mut errors, completed) = turn_errors(&test.codex).await;

    assert_eq!(
        responses_request_bodies(&server).await.len(),
        1,
        "an incomplete response must be requested once"
    );
    assert!(
        stream_errors.is_empty(),
        "an incomplete response must not be retried: {stream_errors:?}"
    );
    assert_eq!(errors.len(), 1, "errors: {errors:?}");
    let error = errors.remove(0);
    assert_eq!(
        error.message,
        format!("Incomplete response returned, reason: {reason}")
    );
    assert_eq!(error.codex_error_info, Some(CodexErrorInfo::Other));
    assert_eq!(completed.error, Some(error));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_max_output_tokens_is_requested_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_incomplete_response_is_requested_once("max_output_tokens").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_content_filter_is_requested_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_incomplete_response_is_requested_once("content_filter").await
}

/// A switch to a model with a smaller context window compacts on the previous model
/// first, and with ChatGPT auth an InvalidRequest from that model runs the compaction
/// again on the current one. An incomplete response is an InvalidRequest, but the previous
/// model did produce it, so the compaction must end after that one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_compaction_is_not_sent_again_on_the_current_model() -> Result<()> {
    skip_if_no_network!(Ok(()));

    // `start_mock_server` mounts an empty model list first, which would shadow this one.
    let server = MockServer::start().await;
    let previous_model = "gpt-5.6";
    let next_model = "gpt-5.5";
    mount_models_once(
        &server,
        ModelsResponse {
            models: vec![
                model_info_with_context_window(previous_model, /*context_window*/ 273_000),
                model_info_with_context_window(next_model, /*context_window*/ 125_000),
            ],
        },
    )
    .await;
    // The first turn fills more of the context than the next model holds.
    mount_completed_then_incomplete_responses(
        &server,
        ev_completed_with_tokens("resp_first", /*total_tokens*/ 120_000),
        "max_output_tokens",
    )
    .await;
    let test = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model(previous_model)
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(5);
        })
        .build(&server)
        .await?;

    test.codex
        .submit(user_turn("before switch", Some(previous_model)))
        .await?;
    let (_, errors, _) = turn_errors(&test.codex).await;
    assert_eq!(errors, Vec::new(), "the first turn should finish");

    test.codex
        .submit(user_turn("after switch", Some(next_model)))
        .await?;
    let (stream_errors, errors, _) = turn_errors(&test.codex).await;

    let bodies = responses_request_bodies(&server).await;
    let compactions = bodies
        .iter()
        .filter(|body| is_compaction_request(body))
        .map(|body| body["model"].as_str().unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        compactions,
        vec![previous_model],
        "an incomplete compaction must be requested once, on the previous model only"
    );
    assert_eq!(bodies.len(), 2, "the turn after the switch must not sample");
    assert!(
        stream_errors.is_empty(),
        "an incomplete compaction must not be retried: {stream_errors:?}"
    );
    assert_eq!(
        errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Error running remote compact task: Incomplete response returned, reason: max_output_tokens"
        ]
    );

    Ok(())
}

/// A provider without remote compaction compacts locally, and local compaction retries
/// every error except a few terminal ones. An incomplete response must still end it after
/// one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_local_compaction_is_requested_once() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    mount_completed_then_incomplete_responses(
        &server,
        ev_completed("resp_first"),
        "content_filter",
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.name = "OpenAI (test)".to_string();
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(5);
        })
        .build(&server)
        .await?;

    test.codex
        .submit(user_turn("before compact", /*model*/ None))
        .await?;
    let (_, errors, _) = turn_errors(&test.codex).await;
    assert_eq!(errors, Vec::new(), "the first turn should finish");

    test.codex.submit(Op::Compact).await?;
    let (stream_errors, errors, _) = turn_errors(&test.codex).await;

    assert_eq!(
        responses_request_bodies(&server).await.len(),
        2,
        "an incomplete local compaction must be requested once"
    );
    assert!(
        stream_errors.is_empty(),
        "an incomplete local compaction must not be retried: {stream_errors:?}"
    );
    assert_eq!(
        errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>(),
        vec!["Incomplete response returned, reason: content_filter"]
    );

    Ok(())
}
