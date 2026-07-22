//! Route-aware local HTTP capability implementation.
//!
//! This code runs wherever the real network request should originate:
//! - in a local environment, that means the orchestrator process
//! - in a remote environment, that means the remote runtime after the
//!   orchestrator has forwarded `http/request` over JSON-RPC

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::RouteAwareClientPool;
use codex_http_client::RouteAwareRequestError;
use codex_http_client::build_reqwest_client_with_custom_ca;
use codex_http_client::with_chatgpt_cloudflare_cookie_store;
use codex_protocol::shell_environment::CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR;
use codex_protocol::shell_environment::OPENAI_FEDERATION_RULE_ID_ENV_VAR;
use codex_protocol::shell_environment::OPENAI_IDENTITY_TOKEN_FILE_ENV_VAR;
use codex_protocol::shell_environment::OPENAI_WORKLOAD_IDENTITY_CONTEXT_ENV_VAR;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use tracing::Instrument;
use url::Url;

use super::HttpResponseBodyStream;
use super::response_body_stream::send_body_delta;
use crate::HttpClient;
use crate::client::ExecServerError;
use crate::protocol::HttpHeader;
use crate::protocol::HttpRedirectPolicy;
use crate::protocol::HttpRequestBodyDeltaNotification;
use crate::protocol::HttpRequestParams;
use crate::protocol::HttpRequestResponse;
use crate::protocol::MAX_HTTP_BODY_DELTA_BYTES;
use crate::rpc::RpcNotificationSender;
use crate::rpc::internal_error;
use crate::rpc::invalid_params;

const HTTP_HEADER_ENV_DENYLIST: &[&str] = &[
    CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN_ENV_VAR,
    OPENAI_FEDERATION_RULE_ID_ENV_VAR,
    OPENAI_IDENTITY_TOKEN_FILE_ENV_VAR,
    OPENAI_WORKLOAD_IDENTITY_CONTEXT_ENV_VAR,
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    "CODEX_CONNECTORS_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AZURE_CLIENT_SECRET",
    "AZURE_FEDERATED_TOKEN_FILE",
    "GOOGLE_APPLICATION_CREDENTIALS",
];

/// HTTP capability implementation backed by the shared route-aware transport.
#[derive(Clone)]
pub struct RouteAwareHttpClient {
    follow_redirects: RouteAwareClientPool,
    stop_redirects: RouteAwareClientPool,
    loopback_follow_redirects: Arc<OnceLock<Result<reqwest::Client, String>>>,
    loopback_stop_redirects: Arc<OnceLock<Result<reqwest::Client, String>>>,
}

/// Streaming response state held between the initial HTTP response and
/// downstream body-delta forwarding.
pub(crate) struct PendingRouteAwareHttpBodyStream {
    pub(crate) request_id: String,
    pub(crate) response: codex_http_client::HttpResponse,
}

/// Validates `http/request` parameters and runs the actual HTTP call used
/// by the exec-server route and the local [`HttpClient`] backend.
pub(crate) struct RouteAwareHttpRequestRunner {
    client: RouteAwareClientPool,
    loopback_client: Arc<OnceLock<Result<reqwest::Client, String>>>,
    redirect_policy: HttpRedirectPolicy,
}

impl RouteAwareHttpClient {
    pub fn new(http_client_factory: HttpClientFactory) -> Self {
        Self {
            loopback_follow_redirects: Arc::new(OnceLock::new()),
            loopback_stop_redirects: Arc::new(OnceLock::new()),
            follow_redirects: RouteAwareClientPool::with_chatgpt_cloudflare_cookies_without_request_logging(
                http_client_factory.clone(),
                // Delegated HTTP targets arbitrary endpoints; route class only labels diagnostics.
                ClientRouteClass::Other,
            ),
            stop_redirects:
                RouteAwareClientPool::with_chatgpt_cloudflare_cookies_without_redirects_or_request_logging(
                    http_client_factory,
                    // Proxy routing comes from the factory, not this diagnostic-only route class.
                    ClientRouteClass::Other,
                ),
        }
    }

    /// Enables narrowly scoped TLS-backend fallback for both redirect policies.
    pub fn with_tls_backend_fallback(mut self) -> Self {
        self.follow_redirects = self.follow_redirects.with_tls_backend_fallback();
        self.stop_redirects = self.stop_redirects.with_tls_backend_fallback();
        self
    }

    pub(crate) fn runner(
        &self,
        redirect_policy: HttpRedirectPolicy,
    ) -> RouteAwareHttpRequestRunner {
        let client = match redirect_policy {
            HttpRedirectPolicy::Follow => self.follow_redirects.clone(),
            HttpRedirectPolicy::Stop => self.stop_redirects.clone(),
        };
        let loopback_client = match redirect_policy {
            HttpRedirectPolicy::Follow => Arc::clone(&self.loopback_follow_redirects),
            HttpRedirectPolicy::Stop => Arc::clone(&self.loopback_stop_redirects),
        };
        RouteAwareHttpRequestRunner {
            client,
            loopback_client,
            redirect_policy,
        }
    }
}

impl HttpClient for RouteAwareHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        async move {
            let runner = self.runner(params.redirect_policy);
            let (response, _) = runner
                .run(HttpRequestParams {
                    stream_response: false,
                    ..params
                })
                .await
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            Ok(response)
        }
        .boxed()
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        async move {
            let runner = self.runner(params.redirect_policy);
            let (response, pending_stream) = runner
                .run(HttpRequestParams {
                    stream_response: true,
                    ..params
                })
                .await
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
            let pending_stream = pending_stream.ok_or_else(|| {
                ExecServerError::Protocol(
                    "http request stream did not return a response body stream".to_string(),
                )
            })?;
            Ok((
                response,
                HttpResponseBodyStream::local(pending_stream.response),
            ))
        }
        .boxed()
    }
}

impl RouteAwareHttpRequestRunner {
    pub(crate) async fn run(
        &self,
        params: HttpRequestParams,
    ) -> Result<(HttpRequestResponse, Option<PendingRouteAwareHttpBodyStream>), JSONRPCErrorError>
    {
        let method = Method::from_bytes(params.method.as_bytes())
            .map_err(|error| invalid_params(format!("http/request method is invalid: {error}")))?;
        let url = Url::parse(&params.url)
            .map_err(|error| invalid_params(format!("http/request url is invalid: {error}")))?;
        match url.scheme() {
            "http" | "https" => {}
            scheme => {
                return Err(invalid_params(format!(
                    "http/request only supports http and https URLs, got {scheme}"
                )));
            }
        }

        let request_span = tracing::info_span!(
            "codex.exec_server.http_request",
            otel.kind = "client",
            http.request.method = method.as_str(),
            server.address = url.host_str().unwrap_or_default(),
            server.port = u64::from(url.port_or_known_default().unwrap_or_default()),
            http.response.status_code = tracing::field::Empty,
            error.type = tracing::field::Empty,
        );
        let mut headers = Self::build_headers(params.headers)?;
        codex_otel::inject_span_w3c_trace_headers(&request_span, &mut headers);
        let send = async {
            if url_is_loopback(&url) {
                // Local MCP bearer credentials must not reach ambient or system proxies.
                // Keep redirects on this same origin so this direct route cannot escape.
                let client = self.loopback_client.get_or_init(|| {
                    let redirects = match self.redirect_policy {
                        HttpRedirectPolicy::Follow => loopback_same_origin_redirect_policy(),
                        HttpRedirectPolicy::Stop => reqwest::redirect::Policy::none(),
                    };
                    build_reqwest_client_with_custom_ca(with_chatgpt_cloudflare_cookie_store(
                        reqwest::Client::builder().no_proxy().redirect(redirects),
                    ))
                    .map_err(|error| error.to_string())
                });
                let client = client
                    .as_ref()
                    .map_err(|error| RouteAwareRequestError::Build(error.clone()))?;
                let mut request = client.request(method.clone(), url).headers(headers);
                if let Some(body) = params.body {
                    request = request.body(body.into_inner());
                }
                if let Some(timeout_ms) = params.timeout_ms {
                    request = request.timeout(Duration::from_millis(timeout_ms));
                }
                request.send().await.map_err(RouteAwareRequestError::from)
            } else {
                let mut request = self.client.request(method.clone(), url).headers(headers);
                if let Some(body) = params.body {
                    request = request.body(body.into_inner());
                }
                if let Some(timeout_ms) = params.timeout_ms {
                    request = request.timeout(Duration::from_millis(timeout_ms));
                }
                request.send().await
            }
        };

        let response = match send.instrument(request_span.clone()).await {
            Ok(response) => response,
            Err(error) => {
                request_span.record("error.type", "request");
                let error_message = error.to_string();
                log_send_error(&method, error);
                return Err(internal_error(format!(
                    "http/request failed: {error_message}"
                )));
            }
        };
        let status = response.status().as_u16();
        request_span.record("http.response.status_code", u64::from(status));
        let headers = Self::response_headers(response.headers());

        if params.stream_response {
            return Ok((
                HttpRequestResponse {
                    status,
                    headers,
                    body: Vec::new().into(),
                },
                Some(PendingRouteAwareHttpBodyStream {
                    request_id: params.request_id,
                    response,
                }),
            ));
        }

        let body = response.bytes().await.map_err(|error| {
            internal_error(format!(
                "failed to read http/request response body: {error}"
            ))
        })?;

        Ok((
            HttpRequestResponse {
                status,
                headers,
                body: body.to_vec().into(),
            },
            None,
        ))
    }

    pub(crate) async fn stream_body(
        pending_stream: PendingRouteAwareHttpBodyStream,
        notifications: RpcNotificationSender,
    ) {
        let PendingRouteAwareHttpBodyStream {
            request_id,
            response,
        } = pending_stream;
        let mut seq = 1;
        let mut body = response.bytes_stream();
        while let Some(chunk) = body.next().await {
            match chunk {
                Ok(bytes) => {
                    for chunk in bytes.chunks(MAX_HTTP_BODY_DELTA_BYTES) {
                        if !send_body_delta(
                            &notifications,
                            HttpRequestBodyDeltaNotification {
                                request_id: request_id.clone(),
                                seq,
                                delta: chunk.to_vec().into(),
                                done: false,
                                error: None,
                            },
                        )
                        .await
                        {
                            return;
                        }
                        seq += 1;
                    }
                }
                Err(error) => {
                    let _ = send_body_delta(
                        &notifications,
                        HttpRequestBodyDeltaNotification {
                            request_id,
                            seq,
                            delta: Vec::new().into(),
                            done: true,
                            error: Some(error.to_string()),
                        },
                    )
                    .await;
                    return;
                }
            }
        }

        let _ = send_body_delta(
            &notifications,
            HttpRequestBodyDeltaNotification {
                request_id,
                seq,
                delta: Vec::new().into(),
                done: true,
                error: None,
            },
        )
        .await;
    }

    fn build_headers(headers: Vec<HttpHeader>) -> Result<HeaderMap, JSONRPCErrorError> {
        let mut header_map = HeaderMap::new();
        for header in headers {
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|error| {
                invalid_params(format!("http/request header name is invalid: {error}"))
            })?;
            let value = match header.value_env_var {
                Some(env_var) => {
                    if HTTP_HEADER_ENV_DENYLIST
                        .iter()
                        .any(|denied| denied.eq_ignore_ascii_case(&env_var))
                    {
                        return Err(invalid_params(format!(
                            "http/request header {} cannot use executor environment variable {env_var}",
                            header.name
                        )));
                    }
                    let env_value = std::env::var(&env_var).map_err(|_| {
                        invalid_params(format!(
                            "http/request header {} requires executor environment variable {env_var}",
                            header.name
                        ))
                    })?;
                    if env_value.is_empty() {
                        return Err(invalid_params(format!(
                            "http/request header {} requires a non-empty executor environment variable {env_var}",
                            header.name
                        )));
                    }
                    format!("{}{env_value}", header.value)
                }
                None => header.value,
            };
            let value = HeaderValue::from_str(&value).map_err(|error| {
                invalid_params(format!(
                    "http/request header value is invalid for {}: {error}",
                    header.name
                ))
            })?;
            header_map.append(name, value);
        }
        Ok(header_map)
    }

    fn response_headers(headers: &HeaderMap) -> Vec<HttpHeader> {
        headers
            .iter()
            .filter_map(|(name, value)| {
                Some(HttpHeader {
                    name: name.as_str().to_string(),
                    value: value.to_str().ok()?.to_string(),
                    value_env_var: None,
                })
            })
            .collect()
    }
}

fn url_is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn loopback_same_origin_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > 10 {
            return attempt.error("too many redirects");
        }
        let Some(original_url) = attempt.previous().first() else {
            return attempt.error("redirect is missing its original URL");
        };
        if original_url.origin() == attempt.url().origin() {
            attempt.follow()
        } else {
            attempt.error("loopback http/request redirect changed origin")
        }
    })
}

fn log_send_error(method: &Method, error: RouteAwareRequestError) {
    let error_is_timeout = error.is_timeout();
    let error_is_connect = error.is_connect();
    let error = match error {
        RouteAwareRequestError::Request(error) => error.without_url().to_string(),
        error => error.to_string(),
    };
    tracing::warn!(
        http_method = method.as_str(),
        error_is_timeout,
        error_is_connect,
        error = %error,
        "http/request send failed"
    );
}

#[cfg(test)]
#[path = "route_aware_http_client_tests.rs"]
mod tests;
