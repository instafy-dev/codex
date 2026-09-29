use super::*;
use base64::Engine;
use codex_protocol::protocol::RateLimitReachedType;
use pretty_assertions::assert_eq;
use std::time::Duration;

#[test]
fn map_api_error_maps_server_overloaded() {
    let err = map_api_error(ApiError::ServerOverloaded);
    assert!(matches!(err, CodexErr::ServerOverloaded));
}

#[test]
fn map_api_error_maps_server_overloaded_from_503_body() {
    let body = serde_json::json!({
        "error": {
            "code": "server_is_overloaded"
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::SERVICE_UNAVAILABLE,
        url: Some("http://example.com/v1/responses".to_string()),
        headers: None,
        body: Some(body),
    }));

    assert!(matches!(err, CodexErr::ServerOverloaded));
}

#[test]
fn map_api_error_maps_cloudflare_blocked_response_to_user_message() {
    let mut headers = HeaderMap::new();
    headers.insert(CF_RAY_HEADER, http::HeaderValue::from_static("ray-id"));
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::FORBIDDEN,
        url: Some("http://example.com/blocked".to_string()),
        headers: Some(headers),
        body: Some(
            "<html><body>Cloudflare error: Sorry, you have been blocked</body></html>".to_string(),
        ),
    }));

    let CodexErr::UnexpectedStatus(err) = err else {
        panic!("expected CodexErr::UnexpectedStatus, got {err:?}");
    };
    assert_eq!(
        err.user_message.as_deref(),
        Some(
            "Access blocked by Cloudflare. This usually happens when connecting from a restricted region (status 403 Forbidden)"
        )
    );
    assert_eq!(
        err.to_string(),
        "Access blocked by Cloudflare. This usually happens when connecting from a restricted region (status 403 Forbidden), url: http://example.com/blocked, cf-ray: ray-id"
    );
}

#[test]
fn map_api_error_maps_cyber_policy_from_400_body() {
    let body = serde_json::json!({
        "error": {
            "message": "This request has been flagged for potentially high-risk cyber activity.",
            "type": "invalid_request",
            "param": null,
            "code": "cyber_policy"
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::BAD_REQUEST,
        url: Some("http://example.com/v1/responses".to_string()),
        headers: None,
        body: Some(body),
    }));

    let CodexErr::CyberPolicy { message } = err else {
        panic!("expected CodexErr::CyberPolicy, got {err:?}");
    };
    assert_eq!(
        message,
        "This request has been flagged for potentially high-risk cyber activity."
    );
}

#[test]
fn map_api_error_maps_wrapped_websocket_cyber_policy_from_400_body() {
    let body = serde_json::json!({
        "type": "error",
        "status": 400,
        "error": {
            "message": "This websocket request was flagged.",
            "type": "invalid_request",
            "code": "cyber_policy"
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::BAD_REQUEST,
        url: Some("ws://example.com/v1/responses".to_string()),
        headers: None,
        body: Some(body),
    }));

    let CodexErr::CyberPolicy { message } = err else {
        panic!("expected CodexErr::CyberPolicy, got {err:?}");
    };
    assert_eq!(message, "This websocket request was flagged.");
}

#[test]
fn map_api_error_uses_cyber_policy_fallback_for_missing_message() {
    let body = serde_json::json!({
        "error": {
            "code": "cyber_policy"
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::BAD_REQUEST,
        url: Some("http://example.com/v1/responses".to_string()),
        headers: None,
        body: Some(body),
    }));

    let CodexErr::CyberPolicy { message } = err else {
        panic!("expected CodexErr::CyberPolicy, got {err:?}");
    };
    assert_eq!(
        message,
        "This request has been flagged for possible cybersecurity risk."
    );
}

#[test]
fn map_api_error_keeps_unknown_400_errors_generic() {
    let body = serde_json::json!({
        "error": {
            "message": "Some other bad request.",
            "code": "some_other_policy"
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::BAD_REQUEST,
        url: Some("http://example.com/v1/responses".to_string()),
        headers: None,
        body: Some(body.clone()),
    }));

    let CodexErr::InvalidRequest(message) = err else {
        panic!("expected CodexErr::InvalidRequest, got {err:?}");
    };
    assert_eq!(message, body);
}

#[test]
fn map_api_error_maps_usage_limit_limit_name_header() {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACTIVE_LIMIT_HEADER,
        http::HeaderValue::from_static("codex_other"),
    );
    headers.insert(
        "x-codex-other-limit-name",
        http::HeaderValue::from_static("codex_other"),
    );
    let body = serde_json::json!({
        "error": {
            "type": "usage_limit_reached",
            "plan_type": "pro",
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::TOO_MANY_REQUESTS,
        url: Some("http://example.com/v1/responses".to_string()),
        headers: Some(headers),
        body: Some(body),
    }));

    let CodexErr::UsageLimitReached(usage_limit) = err else {
        panic!("expected CodexErr::UsageLimitReached, got {err:?}");
    };
    assert_eq!(
        usage_limit
            .rate_limits
            .as_ref()
            .and_then(|snapshot| snapshot.limit_name.as_deref()),
        Some("codex_other")
    );
}

#[test]
fn map_api_error_does_not_fallback_limit_name_to_limit_id() {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACTIVE_LIMIT_HEADER,
        http::HeaderValue::from_static("codex_other"),
    );
    let body = serde_json::json!({
        "error": {
            "type": "usage_limit_reached",
            "plan_type": "pro",
        }
    })
    .to_string();
    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::TOO_MANY_REQUESTS,
        url: Some("http://example.com/v1/responses".to_string()),
        headers: Some(headers),
        body: Some(body),
    }));

    let CodexErr::UsageLimitReached(usage_limit) = err else {
        panic!("expected CodexErr::UsageLimitReached, got {err:?}");
    };
    assert_eq!(
        usage_limit
            .rate_limits
            .as_ref()
            .and_then(|snapshot| snapshot.limit_name.as_deref()),
        None
    );
}

#[test]
fn map_api_error_copies_rate_limit_reached_type_to_usage_limit_snapshot() {
    for (active_limit, expected_limit_id) in [(None, "codex"), (Some("codex_other"), "codex_other")]
    {
        let mut headers = HeaderMap::new();
        if let Some(active_limit) = active_limit {
            headers.insert(
                ACTIVE_LIMIT_HEADER,
                http::HeaderValue::from_static(active_limit),
            );
        }
        for (name, value) in [
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "false"),
            ("x-codex-credits-balance", ""),
            (
                "x-codex-rate-limit-reached-type",
                "workspace_member_usage_limit_reached",
            ),
        ] {
            headers.insert(name, http::HeaderValue::from_static(value));
        }
        let body = serde_json::json!({
            "error": {
                "type": "usage_limit_reached",
                "plan_type": "pro",
            }
        })
        .to_string();

        let err = map_api_error(ApiError::Transport(TransportError::Http {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            url: Some("http://example.com/v1/responses".to_string()),
            headers: Some(headers),
            body: Some(body),
        }));

        let CodexErr::UsageLimitReached(usage_limit) = err else {
            panic!("expected CodexErr::UsageLimitReached, got {err:?}");
        };
        assert_eq!(
            usage_limit.rate_limit_reached_type,
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached)
        );
        let snapshot = usage_limit
            .rate_limits
            .as_ref()
            .expect("usage limit snapshot");
        assert_eq!(snapshot.limit_id.as_deref(), Some(expected_limit_id));
        assert_eq!(
            snapshot.rate_limit_reached_type,
            Some(RateLimitReachedType::WorkspaceMemberUsageLimitReached)
        );
        assert_eq!(
            snapshot.credits.as_ref().map(|credits| (
                credits.has_credits,
                credits.unlimited,
                credits.balance.as_deref()
            )),
            Some((true, false, None))
        );
    }
}

#[test]
fn map_api_error_ignores_unparseable_rate_limit_reached_type_headers() {
    let values = [
        http::HeaderValue::from_static("future_rate_limit_reached_type"),
        http::HeaderValue::from_bytes(&[0xff]).expect("valid opaque header value"),
    ];

    for value in values {
        let mut headers = HeaderMap::new();
        headers.insert("x-codex-rate-limit-reached-type", value);
        let body = serde_json::json!({
            "error": {
                "type": "usage_limit_reached",
                "plan_type": "pro",
            }
        })
        .to_string();
        let err = map_api_error(ApiError::Transport(TransportError::Http {
            status: http::StatusCode::TOO_MANY_REQUESTS,
            url: Some("http://example.com/v1/responses".to_string()),
            headers: Some(headers),
            body: Some(body),
        }));

        let CodexErr::UsageLimitReached(usage_limit) = err else {
            panic!("expected CodexErr::UsageLimitReached, got {err:?}");
        };
        assert_eq!(usage_limit.rate_limit_reached_type, None);
    }
}

#[test]
fn map_api_error_extracts_identity_auth_details_from_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(REQUEST_ID_HEADER, http::HeaderValue::from_static("req-401"));
    headers.insert(CF_RAY_HEADER, http::HeaderValue::from_static("ray-401"));
    headers.insert(
        X_OPENAI_AUTHORIZATION_ERROR_HEADER,
        http::HeaderValue::from_static("missing_authorization_header"),
    );
    let x_error_json =
        base64::engine::general_purpose::STANDARD.encode(r#"{"error":{"code":"token_expired"}}"#);
    headers.insert(
        X_ERROR_JSON_HEADER,
        http::HeaderValue::from_str(&x_error_json).expect("valid x-error-json header"),
    );

    let err = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::UNAUTHORIZED,
        url: Some("https://chatgpt.com/backend-api/codex/models".to_string()),
        headers: Some(headers),
        body: Some(r#"{"detail":"Unauthorized"}"#.to_string()),
    }));

    let CodexErr::UnexpectedStatus(err) = err else {
        panic!("expected CodexErr::UnexpectedStatus, got {err:?}");
    };
    assert_eq!(err.request_id.as_deref(), Some("req-401"));
    assert_eq!(err.cf_ray.as_deref(), Some("ray-401"));
    assert_eq!(
        err.identity_authorization_error.as_deref(),
        Some("missing_authorization_header")
    );
    assert_eq!(err.identity_error_code.as_deref(), Some("token_expired"));
}

fn instafy_proxy_rate_limit_body() -> String {
    serde_json::json!({
        "error": {
            "message": "The upstream provider rate limit was reached.",
            "type": "upstream_error",
            "code": "upstream_rate_limit",
            "retryable": true
        }
    })
    .to_string()
}

fn map_429(headers: Option<HeaderMap>, body: String) -> CodexErr {
    map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::TOO_MANY_REQUESTS,
        url: Some("http://proxy:8789/v1/responses".to_string()),
        headers,
        body: Some(body),
    }))
}

fn retry_after_headers(value: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_str(value).expect("valid Retry-After header"),
    );
    headers
}

fn http_date(at: DateTime<Utc>) -> String {
    at.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn fixed_now() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(1_790_000_000, 0).expect("valid timestamp")
}

/// The delay with the jitter pinned to zero, so tests can compare exact values.
fn unjittered_delay(retry_after: &str, now: DateTime<Utc>) -> Duration {
    instafy_retryable_429_delay(
        Some(&retry_after_headers(retry_after)),
        now,
        /*jitter*/ 0.0,
    )
}

/// `map_api_error` draws a real jitter sample, so its delay can land anywhere from
/// the requested wait to 20% above it, never past the 30 second cap.
fn assert_jittered_delay(delay: Duration, requested: Duration) {
    let max = (requested + requested / 5).min(Duration::from_secs(30));
    assert!(
        (requested..=max).contains(&delay),
        "delay {delay:?} outside {requested:?}..={max:?}"
    );
}

#[test]
fn map_api_error_maps_instafy_retryable_429_to_stream_retry_with_retry_after() {
    let err = map_429(
        Some(retry_after_headers("2")),
        instafy_proxy_rate_limit_body(),
    );

    assert!(
        err.is_retryable(),
        "expected a retryable error, got {err:?}"
    );
    let CodexErr::Stream(message, Some(delay)) = &err else {
        panic!("expected CodexErr::Stream with a delay, got {err:?}");
    };
    assert_eq!(
        message,
        "429 Too Many Requests: The upstream provider rate limit was reached."
    );
    assert_jittered_delay(*delay, Duration::from_secs(2));
    // This is the text a person sees if every stream retry hits the limit too, so
    // the status has to survive into it.
    assert_eq!(
        err.to_string(),
        "stream disconnected before completion: 429 Too Many Requests: The upstream provider rate limit was reached."
    );
}

#[test]
fn map_api_error_maps_instafy_retryable_429_without_retry_after_to_default_delay() {
    let err = map_429(/*headers*/ None, instafy_proxy_rate_limit_body());

    let CodexErr::Stream(message, Some(delay)) = err else {
        panic!("expected CodexErr::Stream with a delay, got {err:?}");
    };
    assert_eq!(
        message,
        "429 Too Many Requests: The upstream provider rate limit was reached."
    );
    assert_jittered_delay(delay, Duration::from_secs(5));
}

#[test]
fn instafy_retryable_429_delay_reads_http_date_retry_after() {
    let now = fixed_now();
    let retry_at = now + chrono::TimeDelta::seconds(20);

    assert_eq!(
        unjittered_delay(&http_date(retry_at), now),
        Duration::from_secs(20)
    );
}

#[test]
fn map_api_error_spreads_retryable_429_delays() {
    // Runtimes that share one upstream key get the same Retry-After, so the delay
    // map_api_error hands back must differ between calls or they retry together.
    let delays: Vec<Duration> = (0..16)
        .map(|_| {
            let err = map_429(
                Some(retry_after_headers("2")),
                instafy_proxy_rate_limit_body(),
            );
            let CodexErr::Stream(_, Some(delay)) = err else {
                panic!("expected CodexErr::Stream with a delay, got {err:?}");
            };
            assert_jittered_delay(delay, Duration::from_secs(2));
            delay
        })
        .collect();
    assert!(
        delays.iter().any(|delay| *delay != delays[0]),
        "every mapped delay was {:?}; jitter is not applied",
        delays[0]
    );
}

#[test]
fn instafy_retryable_429_delay_parses_and_clamps_retry_after() {
    let now = fixed_now();
    let past = http_date(now - chrono::TimeDelta::seconds(60));
    let far_future = http_date(now + chrono::TimeDelta::seconds(3_600));
    let cases = [
        ("2", Duration::from_secs(2)),
        (" 7 ", Duration::from_secs(7)),
        ("0", Duration::from_secs(1)),
        ("3600", Duration::from_secs(30)),
        // A fraction rounds up so the retry cannot land inside the window.
        ("1.5", Duration::from_secs(2)),
        ("2.0", Duration::from_secs(2)),
        ("0.2", Duration::from_secs(1)),
        // A negative value or a past date means the window has passed.
        ("-5", Duration::from_secs(1)),
        ("-1.5", Duration::from_secs(1)),
        (past.as_str(), Duration::from_secs(1)),
        // Values beyond u64 seconds or a Duration saturate instead of being ignored.
        ("18446744073709551616", Duration::from_secs(30)),
        ("99999999999999999999999999", Duration::from_secs(30)),
        (far_future.as_str(), Duration::from_secs(30)),
        // Anything that is neither delta-seconds nor a date gets the default.
        ("soon", Duration::from_secs(5)),
        ("inf", Duration::from_secs(5)),
        ("NaN", Duration::from_secs(5)),
        ("1e3", Duration::from_secs(5)),
        ("+5", Duration::from_secs(5)),
        ("1.2.3", Duration::from_secs(5)),
        (".", Duration::from_secs(5)),
        ("-", Duration::from_secs(5)),
        ("", Duration::from_secs(5)),
    ];

    for (retry_after, expected) in cases {
        assert_eq!(
            unjittered_delay(retry_after, now),
            expected,
            "Retry-After {retry_after:?}"
        );
    }
}

#[test]
fn instafy_retryable_429_delay_adds_bounded_positive_jitter() {
    let now = fixed_now();
    let cases = [
        (Some("2"), 0.0, Duration::from_millis(2_000)),
        (Some("2"), 0.5, Duration::from_millis(2_200)),
        (Some("2"), 1.0, Duration::from_millis(2_400)),
        (None, 1.0, Duration::from_secs(6)),
        // Below the cap the spread uses only the room left under it, so a long
        // wait still spreads instead of piling up at exactly the cap.
        (Some("28"), 0.5, Duration::from_secs(29)),
        (Some("28"), 1.0, Duration::from_secs(30)),
        // At or past the cap the spread goes below it: 30 s down to 24 s.
        (Some("30"), 0.0, Duration::from_secs(30)),
        (Some("3600"), 0.0, Duration::from_secs(30)),
        (Some("3600"), 0.5, Duration::from_secs(27)),
        (Some("3600"), 1.0, Duration::from_secs(24)),
        // A sample outside [0, 1] is pinned to that range.
        (Some("2"), 2.0, Duration::from_millis(2_400)),
        (Some("2"), -1.0, Duration::from_millis(2_000)),
    ];

    for (retry_after, jitter, expected) in cases {
        let headers = retry_after.map(retry_after_headers);
        assert_eq!(
            instafy_retryable_429_delay(headers.as_ref(), now, jitter),
            expected,
            "Retry-After {retry_after:?} with jitter {jitter}"
        );
    }
}

#[test]
fn retry_jitter_sample_stays_in_unit_interval_and_varies() {
    let samples: Vec<f64> = (0..64).map(|_| retry_jitter_sample()).collect();

    assert!(
        samples.iter().all(|sample| (0.0..=1.0).contains(sample)),
        "samples outside [0, 1]: {samples:?}"
    );
    // 64 draws from 2^32 values landing on a single one would mean the source is
    // not random and every runtime would still retry at the same instant.
    assert!(
        samples
            .iter()
            .any(|sample| sample.to_bits() != samples[0].to_bits()),
        "samples never varied: {samples:?}"
    );
}

#[test]
fn map_api_error_uses_fallback_message_for_instafy_retryable_429_without_message() {
    let body = serde_json::json!({
        "error": { "type": "upstream_error", "retryable": true }
    })
    .to_string();
    let err = map_429(/*headers*/ None, body);

    let CodexErr::Stream(message, _) = err else {
        panic!("expected CodexErr::Stream, got {err:?}");
    };
    assert_eq!(
        message,
        "429 Too Many Requests: The AI provider is rate limiting requests."
    );
}

#[test]
fn map_api_error_keeps_non_retryable_429_as_retry_limit() {
    let bodies = [
        serde_json::json!({
            "error": {
                "message": "Rate limit reached for requests",
                "type": "requests",
                "code": "rate_limit_exceeded"
            }
        })
        .to_string(),
        serde_json::json!({
            "error": {
                "message": "The upstream provider rate limit was reached.",
                "type": "upstream_error",
                "code": "upstream_rate_limit",
                "retryable": false
            }
        })
        .to_string(),
        // Another provider's retryable body keeps the old handling.
        serde_json::json!({
            "error": {
                "message": "Rate limit reached for requests",
                "type": "rate_limit_exceeded",
                "retryable": true
            }
        })
        .to_string(),
        serde_json::json!({
            "error": {
                "message": "The upstream provider rate limit was reached.",
                "code": "upstream_rate_limit",
                "retryable": true
            }
        })
        .to_string(),
        serde_json::json!({ "error": { "type": "upstream_error", "retryable": "true" } })
            .to_string(),
        serde_json::json!({ "type": "upstream_error", "retryable": true }).to_string(),
        "Too Many Requests".to_string(),
    ];

    for body in bodies {
        let mut headers = retry_after_headers("2");
        headers.insert(REQUEST_ID_HEADER, http::HeaderValue::from_static("req-429"));
        let err = map_429(Some(headers), body.clone());

        assert!(!err.is_retryable(), "body {body}");
        let CodexErr::RetryLimit(retry_limit) = err else {
            panic!("expected CodexErr::RetryLimit for body {body}, got {err:?}");
        };
        assert_eq!(retry_limit.status, http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry_limit.request_id.as_deref(), Some("req-429"));
    }
}

#[test]
fn map_api_error_keeps_usage_limit_429_terminal_even_when_marked_retryable() {
    let body = serde_json::json!({
        "error": {
            "type": "usage_limit_reached",
            "plan_type": "pro",
            "retryable": true
        }
    })
    .to_string();
    let err = map_429(Some(retry_after_headers("2")), body);
    assert!(matches!(err, CodexErr::UsageLimitReached(_)), "got {err:?}");

    let body = serde_json::json!({
        "error": {
            "type": "usage_not_included",
            "retryable": true
        }
    })
    .to_string();
    let err = map_429(Some(retry_after_headers("2")), body);
    assert!(matches!(err, CodexErr::UsageNotIncluded), "got {err:?}");
}

#[test]
fn is_instafy_incomplete_response_matches_only_the_incomplete_invalid_request() {
    let message = "Incomplete response returned, reason: content_filter";
    assert!(is_instafy_incomplete_response(&CodexErr::InvalidRequest(
        message.to_string()
    )));

    // A rejected request, and any other error that carries the same text, keep their
    // handling, including the compaction fallback to the current model.
    let rejected = map_api_error(ApiError::Transport(TransportError::Http {
        status: http::StatusCode::BAD_REQUEST,
        url: Some("http://proxy:8789/v1/responses".to_string()),
        headers: None,
        body: Some(serde_json::json!({ "detail": message }).to_string()),
    }));
    assert!(
        matches!(rejected, CodexErr::InvalidRequest(_)),
        "got {rejected:?}"
    );
    assert!(!is_instafy_incomplete_response(&rejected));
    assert!(!is_instafy_incomplete_response(&CodexErr::Stream(
        message.to_string(),
        /*delay*/ None,
    )));
}
