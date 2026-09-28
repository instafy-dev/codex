use crate::TransportError;
use crate::error::ApiError;
use crate::rate_limits::parse_promo_message;
use crate::rate_limits::parse_rate_limit_for_limit;
use crate::rate_limits::parse_rate_limit_reached_type;
use base64::Engine;
use chrono::DateTime;
use chrono::Utc;
use codex_protocol::auth::PlanType;
use codex_protocol::error::CodexErr;
use codex_protocol::error::RetryLimitReachedError;
use codex_protocol::error::UnexpectedResponseError;
use codex_protocol::error::UsageLimitReachedError;
use http::HeaderMap;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

pub fn map_api_error(err: ApiError) -> CodexErr {
    match err {
        ApiError::ContextWindowExceeded => CodexErr::ContextWindowExceeded,
        ApiError::QuotaExceeded => CodexErr::QuotaExceeded,
        ApiError::UsageNotIncluded => CodexErr::UsageNotIncluded,
        ApiError::Retryable { message, delay } => CodexErr::Stream(message, delay),
        ApiError::Stream(msg) => CodexErr::Stream(msg, None),
        ApiError::ServerOverloaded => CodexErr::ServerOverloaded,
        ApiError::Api { status, message } => {
            let user_message = api_error_user_message(status, &message);
            CodexErr::UnexpectedStatus(UnexpectedResponseError {
                status,
                body: message,
                user_message,
                url: None,
                cf_ray: None,
                request_id: None,
                identity_authorization_error: None,
                identity_error_code: None,
            })
        }
        ApiError::InvalidRequest { message } => CodexErr::InvalidRequest(message),
        ApiError::CyberPolicy { message } => CodexErr::CyberPolicy { message },
        ApiError::Transport(transport) => match transport {
            TransportError::Http {
                status,
                url,
                headers,
                body,
            } => {
                let body_text = body.unwrap_or_default();

                if status == http::StatusCode::SERVICE_UNAVAILABLE
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&body_text)
                    && matches!(
                        value
                            .get("error")
                            .and_then(|error| error.get("code"))
                            .and_then(serde_json::Value::as_str),
                        Some("server_is_overloaded" | "slow_down")
                    )
                {
                    return CodexErr::ServerOverloaded;
                }

                if status == http::StatusCode::BAD_REQUEST {
                    if let Ok(parsed) = serde_json::from_str::<Value>(&body_text)
                        && let Some(error) = parsed.get("error")
                        && error.get("code").and_then(Value::as_str)
                            == Some(CYBER_POLICY_ERROR_CODE)
                    {
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .filter(|message| !message.trim().is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| CYBER_POLICY_FALLBACK_MESSAGE.to_string());
                        CodexErr::CyberPolicy { message }
                    } else if body_text
                        .contains("The image data you provided does not represent a valid image")
                    {
                        CodexErr::InvalidImageRequest()
                    } else {
                        CodexErr::InvalidRequest(body_text)
                    }
                } else if status == http::StatusCode::INTERNAL_SERVER_ERROR {
                    CodexErr::InternalServerError
                } else if status == http::StatusCode::TOO_MANY_REQUESTS {
                    if let Ok(err) = serde_json::from_str::<UsageErrorResponse>(&body_text) {
                        if err.error.error_type.as_deref() == Some("usage_limit_reached") {
                            let limit_id = extract_header(headers.as_ref(), ACTIVE_LIMIT_HEADER);
                            let promo_message = headers.as_ref().and_then(parse_promo_message);
                            let rate_limit_reached_type =
                                headers.as_ref().and_then(parse_rate_limit_reached_type);
                            let rate_limits = headers
                                .as_ref()
                                .and_then(|map| {
                                    parse_rate_limit_for_limit(map, limit_id.as_deref())
                                })
                                .map(|mut snapshot| {
                                    snapshot.rate_limit_reached_type = rate_limit_reached_type;
                                    snapshot
                                });
                            let resets_at = err
                                .error
                                .resets_at
                                .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0));
                            return CodexErr::UsageLimitReached(UsageLimitReachedError {
                                plan_type: err.error.plan_type,
                                resets_at,
                                rate_limits: rate_limits.map(Box::new),
                                promo_message,
                                rate_limit_reached_type,
                            });
                        } else if err.error.error_type.as_deref() == Some("usage_not_included") {
                            return CodexErr::UsageNotIncluded;
                        }
                    }

                    // Instafy's model proxy does not retry an upstream rate limit itself. It
                    // answers with a 429 whose body is marked retryable and leaves the retry to
                    // this client. A stream error sends the turn through the stream retry budget
                    // after the proxy's delay, instead of ending it on a RetryLimit that claims
                    // retries were made when none were. A stream error carries no status code,
                    // so the status leads the message: once the budget runs out, whatever reads
                    // the final error can still tell it was a 429.
                    if let Some(message) = instafy_retryable_error_message(&body_text) {
                        let delay = instafy_retryable_429_delay(
                            headers.as_ref(),
                            Utc::now(),
                            retry_jitter_sample(),
                        );
                        return CodexErr::Stream(format!("{status}: {message}"), Some(delay));
                    }

                    CodexErr::RetryLimit(RetryLimitReachedError {
                        status,
                        request_id: extract_request_tracking_id(headers.as_ref()),
                    })
                } else {
                    CodexErr::UnexpectedStatus(UnexpectedResponseError {
                        status,
                        user_message: api_error_user_message(status, &body_text),
                        body: body_text,
                        url,
                        cf_ray: extract_header(headers.as_ref(), CF_RAY_HEADER),
                        request_id: extract_request_id(headers.as_ref()),
                        identity_authorization_error: extract_header(
                            headers.as_ref(),
                            X_OPENAI_AUTHORIZATION_ERROR_HEADER,
                        ),
                        identity_error_code: extract_x_error_json_code(headers.as_ref()),
                    })
                }
            }
            TransportError::RetryLimit => CodexErr::RetryLimit(RetryLimitReachedError {
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
                request_id: None,
            }),
            TransportError::Timeout => CodexErr::RequestTimeout,
            TransportError::Network(msg) | TransportError::Build(msg) => {
                CodexErr::Stream(msg, None)
            }
        },
        ApiError::RateLimit(msg) => CodexErr::Stream(msg, None),
    }
}

const ACTIVE_LIMIT_HEADER: &str = "x-codex-active-limit";
const REQUEST_ID_HEADER: &str = "x-request-id";
const OAI_REQUEST_ID_HEADER: &str = "x-oai-request-id";
const CF_RAY_HEADER: &str = "cf-ray";
const X_OPENAI_AUTHORIZATION_ERROR_HEADER: &str = "x-openai-authorization-error";
const X_ERROR_JSON_HEADER: &str = "x-error-json";
const CYBER_POLICY_ERROR_CODE: &str = "cyber_policy";
const CYBER_POLICY_FALLBACK_MESSAGE: &str =
    "This request has been flagged for possible cybersecurity risk.";
const CLOUDFLARE_BLOCKED_MESSAGE: &str =
    "Access blocked by Cloudflare. This usually happens when connecting from a restricted region";
const RETRY_AFTER_HEADER: &str = "retry-after";
const INSTAFY_UPSTREAM_ERROR_TYPE: &str = "upstream_error";
const INSTAFY_RETRYABLE_429_FALLBACK_MESSAGE: &str = "The AI provider is rate limiting requests.";
// Bounded Instafy proxy runs allow one stream retry per sampling request, and other runs
// keep the provider's default stream retry count. Either way the count starts over for
// each sampling request, so a single wait should outlast a short rate-limit window
// without spending much of a bounded run's time budget. A missing or unreadable
// Retry-After gets a few seconds rather than the sub-second backoff.
const INSTAFY_RETRYABLE_429_DEFAULT_DELAY: Duration = Duration::from_secs(5);
const INSTAFY_RETRYABLE_429_MIN_DELAY: Duration = Duration::from_secs(1);
const INSTAFY_RETRYABLE_429_MAX_DELAY: Duration = Duration::from_secs(30);
// Managed runtimes share one upstream key, so turns that hit the same limit get the same
// Retry-After and would all retry at the same instant. Waiting up to this much longer,
// never shorter, spreads them out while still honoring the proxy's floor.
const INSTAFY_RETRYABLE_429_MAX_JITTER_PERCENT: u32 = 20;

#[cfg(test)]
#[path = "api_bridge_tests.rs"]
mod tests;

fn extract_request_tracking_id(headers: Option<&HeaderMap>) -> Option<String> {
    extract_request_id(headers).or_else(|| extract_header(headers, CF_RAY_HEADER))
}

fn api_error_user_message(status: http::StatusCode, body: &str) -> Option<String> {
    if status == http::StatusCode::FORBIDDEN
        && body.contains("Cloudflare")
        && body.contains("blocked")
    {
        Some(format!("{CLOUDFLARE_BLOCKED_MESSAGE} (status {status})"))
    } else {
        None
    }
}

fn extract_request_id(headers: Option<&HeaderMap>) -> Option<String> {
    extract_header(headers, REQUEST_ID_HEADER)
        .or_else(|| extract_header(headers, OAI_REQUEST_ID_HEADER))
}

fn extract_header(headers: Option<&HeaderMap>, name: &str) -> Option<String> {
    headers.and_then(|map| {
        map.get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    })
}

fn extract_x_error_json_code(headers: Option<&HeaderMap>) -> Option<String> {
    let encoded = extract_header(headers, X_ERROR_JSON_HEADER)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let parsed = serde_json::from_slice::<Value>(&decoded).ok()?;
    parsed
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Returns the error message when a 429 body has the Instafy proxy shape
/// `{"error":{"type":"upstream_error","message":...,"retryable":true}}`, and `None`
/// for any other body, so another provider's retryable 429 keeps its old handling.
fn instafy_retryable_error_message(body: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(body).ok()?;
    let error = parsed.get("error")?;
    if error.get("type").and_then(Value::as_str) != Some(INSTAFY_UPSTREAM_ERROR_TYPE)
        || error.get("retryable").and_then(Value::as_bool) != Some(true)
    {
        return None;
    }
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map_or_else(
            || INSTAFY_RETRYABLE_429_FALLBACK_MESSAGE.to_string(),
            str::to_string,
        );
    Some(message)
}

/// Reads Retry-After as delta-seconds or an HTTP date and clamps it, so a
/// hostile or mistaken header cannot stall a turn for minutes, then spreads the
/// retry by up to `INSTAFY_RETRYABLE_429_MAX_JITTER_PERCENT` so runtimes sharing
/// one key do not all retry at the same instant. Below the cap the spread only
/// lengthens the wait and uses the room left under the cap; when the server asked
/// for the cap or longer, the wait is already shorter than asked, so the spread
/// goes below the cap instead of pinning every retry to exactly the cap.
/// `jitter` is a sample from [0, 1] that picks where in that spread the retry lands;
/// it is a parameter so tests can pin it (0 means no spread).
fn instafy_retryable_429_delay(
    headers: Option<&HeaderMap>,
    now: DateTime<Utc>,
    jitter: f64,
) -> Duration {
    let asked = extract_header(headers, RETRY_AFTER_HEADER)
        .and_then(|value| parse_retry_after(value.trim(), now))
        .unwrap_or(INSTAFY_RETRYABLE_429_DEFAULT_DELAY);
    let requested = asked.clamp(
        INSTAFY_RETRYABLE_429_MIN_DELAY,
        INSTAFY_RETRYABLE_429_MAX_DELAY,
    );
    let jitter = jitter.clamp(0.0, 1.0);
    let max_spread = requested * INSTAFY_RETRYABLE_429_MAX_JITTER_PERCENT / 100;
    if asked >= INSTAFY_RETRYABLE_429_MAX_DELAY {
        return INSTAFY_RETRYABLE_429_MAX_DELAY - max_spread.mul_f64(jitter);
    }
    let room = max_spread.min(INSTAFY_RETRYABLE_429_MAX_DELAY - requested);
    requested + room.mul_f64(jitter)
}

/// A uniform sample from [0, 1] for spreading retries. The leading bytes of a v4
/// UUID are random, and this crate already makes v4 UUIDs for request ids, so the
/// spread needs no new dependency.
fn retry_jitter_sample() -> f64 {
    let bytes = Uuid::new_v4().into_bytes();
    let sample = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    f64::from(sample) / f64::from(u32::MAX)
}

fn parse_retry_after(value: &str, now: DateTime<Utc>) -> Option<Duration> {
    if let Some(delay) = parse_retry_after_seconds(value) {
        return Some(delay);
    }
    // HTTP dates use the RFC 1123 form, which RFC 2822 parsing accepts. A date
    // that has already passed means the caller may retry right away.
    let retry_at = DateTime::parse_from_rfc2822(value).ok()?;
    Some(
        (retry_at.with_timezone(&Utc) - now)
            .to_std()
            .unwrap_or(Duration::ZERO),
    )
}

/// RFC 9110 delta-seconds is a whole number, but proxies and SDKs also send
/// fractions and the odd negative value. A fraction rounds up so the retry never
/// lands inside the window, a negative value means the window has already passed,
/// and a value too large for a `Duration` saturates. The caller clamps all three.
/// Anything other than an optional minus sign, digits and one decimal point (for
/// example `inf`, `1e3` or `+5`) is not delta-seconds and falls through to date
/// parsing.
fn parse_retry_after_seconds(value: &str) -> Option<Duration> {
    let (negative, magnitude) = match value.strip_prefix('-') {
        Some(magnitude) => (true, magnitude),
        None => (false, value),
    };
    if !magnitude
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    // Rejects an empty string, a lone point and more than one point.
    let seconds = magnitude.parse::<f64>().ok()?;
    if negative {
        return Some(Duration::ZERO);
    }
    Some(Duration::try_from_secs_f64(seconds.ceil()).unwrap_or(Duration::MAX))
}

#[derive(Debug, Deserialize)]
struct UsageErrorResponse {
    error: UsageErrorBody,
}

#[derive(Debug, Deserialize)]
struct UsageErrorBody {
    #[serde(rename = "type")]
    error_type: Option<String>,
    plan_type: Option<PlanType>,
    resets_at: Option<i64>,
}
