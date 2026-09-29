//! Instafy bills every response the upstream produces, including one that stops early.
//! When no output item that the turn records in its history completed before the stop, a
//! retry sends the same request again, and a response cut off by max_output_tokens or
//! stopped by content_filter would most likely end the same way. These tests run whole turns and compactions with a stream
//! retry budget to check that such a response is requested once and ends the turn with
//! its reason.
//!
//! Once an output item has completed, the turn has recorded it and run its tool call, and
//! its retry rebuilds the request from that history: it continues with the tool output, as
//! any follow-up does, and must still be sent. Compaction retries send the same request
//! either way, so a compaction is requested once in both cases.

use anyhow::Result;
use codex_login::CodexAuth;
use codex_models_manager::bundled_models_response;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::AskForApproval;
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
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::ev_shell_command_call;
use core_test_support::responses::mount_models_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::start_websocket_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

/// The `response.incomplete` event that stops a response early for `reason`.
fn ev_incomplete(reason: &str) -> Value {
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
    })
}

/// Serves the same incomplete response to every request, so a retry would reach the
/// mock and be counted rather than fail for lack of a response. The response completes the
/// output items in `finished`, then stops for `reason` in the middle of a message.
async fn mount_incomplete_response(server: &MockServer, finished: Vec<Value>, reason: &str) {
    let mut events = vec![ev_response_created("resp_incomplete")];
    events.extend(finished);
    events.extend([
        ev_message_item_added("msg_incomplete", "partial content"),
        ev_output_text_delta("continued chunk"),
        ev_incomplete(reason),
    ]);
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(sse_response(sse(events)))
        .mount(server)
        .await;
}

/// Finishes the first request, a normal turn, with `completed`, and answers every later
/// one, the compaction and anything that would send it again, with an incomplete response
/// that completes `finished` first.
async fn mount_completed_then_incomplete_responses(
    server: &MockServer,
    completed: Value,
    finished: Vec<Value>,
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
    mount_incomplete_response(server, finished, reason).await;
}

/// A reasoning item, which a compaction response can complete before its compaction item.
fn finished_reasoning_item() -> Vec<Value> {
    vec![ev_reasoning_item("rs_finished", &["summarizing"], &[])]
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

fn user_input(text: &str, thread_settings: ThreadSettingsOverrides) -> Op {
    Op::UserInput {
        items: vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings,
    }
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
    user_input(
        text,
        ThreadSettingsOverrides {
            collaboration_mode,
            ..Default::default()
        },
    )
}

/// A turn that runs the model's tool calls without approval or a sandbox, as
/// `TestCodex::submit_turn` does.
fn tool_turn(test: &TestCodex, text: &str) -> Op {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    user_input(
        text,
        ThreadSettingsOverrides {
            approval_policy: Some(AskForApproval::Never),
            sandbox_policy: Some(sandbox_policy),
            permission_profile,
            ..Default::default()
        },
    )
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

/// Runs a turn whose response completes `finished`, which core does not record, and then
/// stops for `reason`, and checks that it is requested once and ends the turn with its reason.
async fn assert_incomplete_response_is_requested_once(
    finished: Vec<Value>,
    reason: &str,
) -> Result<()> {
    let server = start_mock_server().await;
    mount_incomplete_response(&server, finished, reason).await;
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
    assert_incomplete_response_is_requested_once(/*finished*/ Vec::new(), "max_output_tokens").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_content_filter_is_requested_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_incomplete_response_is_requested_once(/*finished*/ Vec::new(), "content_filter").await
}

/// A completed item of a type this client does not know parses as `Other`, which core drops
/// instead of recording in the conversation history. The turn's retry rebuilds its request
/// from that unchanged history, so it would send the same request again and must not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_after_only_an_unrecorded_item_is_requested_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_incomplete_response_is_requested_once(
        vec![json!({
            "type": "response.output_item.done",
            "item": {
                "type": "mcp_list_tools",
                "id": "mcpl_finished",
                "server_label": "docs",
                "tools": []
            }
        })],
        "max_output_tokens",
    )
    .await
}

/// Parallel tool calls where the first call completes and max_output_tokens cuts the
/// response off during the second. The turn records the first call and runs it before the
/// incomplete response ends the request, so its retry rebuilds the request from that
/// history: the follow-up that hands the model the tool output, not the same request again.
/// The turn must send it rather than end with a command run and its output never shown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_after_a_finished_tool_call_continues_with_its_output() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "call_finished";
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp_capped"),
                ev_shell_command_call(call_id, "echo ran before the cap"),
                json!({
                    "type": "response.output_item.added",
                    "item": {
                        "type": "function_call",
                        "call_id": "call_cut_off",
                        "name": "shell_command",
                        "arguments": ""
                    }
                }),
                ev_incomplete("max_output_tokens"),
            ]),
            sse(vec![
                ev_response_created("resp_continued"),
                ev_assistant_message("msg_continued", "done"),
                ev_completed("resp_continued"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(5);
        })
        .build(&server)
        .await?;

    test.codex
        .submit(tool_turn(&test, "run two commands"))
        .await?;
    let (stream_errors, errors, completed) = turn_errors(&test.codex).await;

    let requests = responses.requests();
    assert_eq!(
        requests.len(),
        2,
        "the turn must continue after the incomplete response"
    );
    assert_eq!(requests[0].function_call_output_text(call_id), None);
    let continuation = &requests[1];
    let output = continuation
        .function_call_output_text(call_id)
        .expect("the continuation must carry the output of the call that ran");
    assert!(
        output.contains("ran before the cap"),
        "tool output: {output}"
    );
    let call_ids = continuation
        .input()
        .iter()
        .filter_map(|item| item.get("call_id").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(
        call_ids,
        vec![call_id, call_id],
        "the continuation carries the finished call and its output, not the cut-off call"
    );
    assert_eq!(
        stream_errors
            .iter()
            .map(|event| event.additional_details.as_deref())
            .collect::<Vec<_>>(),
        vec![Some(
            "stream disconnected before completion: Incomplete response returned, reason: max_output_tokens"
        )]
    );
    assert_eq!(errors, Vec::new());
    assert_eq!(completed.error, None);

    Ok(())
}

/// The same continuation over the Responses websocket, which parses its events with its own
/// per-request state and opens a new connection for the retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_after_a_finished_tool_call_continues_over_websocket() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let call_id = "call_finished";
    let server = start_websocket_server(vec![
        vec![
            // The session's startup prewarm comes first.
            vec![ev_response_created("warm-1"), ev_completed("warm-1")],
            vec![
                ev_response_created("resp_capped"),
                ev_shell_command_call(call_id, "echo ran before the cap"),
                ev_incomplete("max_output_tokens"),
            ],
        ],
        vec![vec![
            ev_response_created("resp_continued"),
            ev_assistant_message("msg_continued", "done"),
            ev_completed("resp_continued"),
        ]],
    ])
    .await;
    let mut builder = test_codex().with_config(|config| {
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(5);
    });
    let test = builder.build_with_websocket_server(&server).await?;

    test.submit_turn("run a command").await?;

    let connections = server.connections();
    assert_eq!(
        connections.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 1],
        "the turn must continue, on a new connection, after the incomplete response"
    );
    let continuation = connections[1][0].body_json();
    let input = &continuation["input"];
    let output = input
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["type"] == "function_call_output" && item["call_id"] == call_id)
        .and_then(|item| item["output"].as_str())
        .unwrap_or_else(|| {
            panic!("the continuation must carry the output of the call that ran: {input}")
        });
    assert!(
        output.contains("ran before the cap"),
        "tool output: {output}"
    );

    server.shutdown().await;
    Ok(())
}

/// A switch to a model with a smaller context window compacts on the previous model
/// first, and with ChatGPT auth an InvalidRequest from that model runs the compaction
/// again on the current one. An incomplete response is an InvalidRequest, but the previous
/// model did produce it, so the compaction must end after that one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_compaction_is_not_sent_again_on_the_current_model() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_incomplete_compaction_is_requested_once(
        /*finished*/ Vec::new(),
        "Error running remote compact task: Incomplete response returned, reason: max_output_tokens",
    )
    .await
}

/// After a completed output item the incomplete response is a retryable stream error, but
/// the compaction stream retry sends the same prompt again, so it must not retry it either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_compaction_after_a_finished_item_is_requested_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_incomplete_compaction_is_requested_once(
        finished_reasoning_item(),
        "Error running remote compact task: stream disconnected before completion: Incomplete response returned, reason: max_output_tokens",
    )
    .await
}

/// Switches to a model with a smaller context window, which runs a remote compaction on the
/// previous model that ends incomplete after completing `finished`, and checks that it is
/// requested once and ends the turn with `expected_error`.
async fn assert_incomplete_compaction_is_requested_once(
    finished: Vec<Value>,
    expected_error: &str,
) -> Result<()> {
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
        finished,
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
        vec![expected_error]
    );

    Ok(())
}

/// A provider without remote compaction compacts locally, and local compaction retries
/// every error except a few terminal ones. An incomplete response must still end it after
/// one request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_local_compaction_is_requested_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_incomplete_local_compaction_is_requested_once(
        /*finished*/ Vec::new(),
        "Incomplete response returned, reason: content_filter",
    )
    .await
}

/// Every local compaction attempt sends the history taken before the first one, so a retry
/// after a completed output item would send the same request again too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_local_compaction_after_a_finished_item_is_requested_once() -> Result<()> {
    skip_if_no_network!(Ok(()));
    assert_incomplete_local_compaction_is_requested_once(
        finished_reasoning_item(),
        "stream disconnected before completion: Incomplete response returned, reason: content_filter",
    )
    .await
}

/// Runs a turn, then a local compaction that ends incomplete after completing `finished`,
/// and checks that the compaction is requested once and ends with `expected_error`.
async fn assert_incomplete_local_compaction_is_requested_once(
    finished: Vec<Value>,
    expected_error: &str,
) -> Result<()> {
    let server = start_mock_server().await;
    mount_completed_then_incomplete_responses(
        &server,
        ev_completed("resp_first"),
        finished,
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
        vec![expected_error]
    );

    Ok(())
}
