//! `reqwest`-backed `HttpClient` implementation.
//!
//! This code runs wherever the real network request should originate:
//! - in a local environment, that means the orchestrator process
//! - in a remote environment, that means the remote runtime after the
//!   orchestrator has forwarded `http/request` over JSON-RPC

use std::error::Error as StdError;
use std::net::IpAddr;
use std::time::Duration;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_http_client::build_reqwest_client_with_custom_ca;
use codex_http_client::with_chatgpt_cloudflare_cookie_store;
use futures::FutureExt;
use futures::StreamExt;
use futures::future::BoxFuture;
use reqwest::Method;
use reqwest::Url;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use tracing::Instrument;

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

/// `HttpClient` implementation that performs the actual HTTP request with
/// `reqwest`.
#[derive(Clone, Default)]
pub struct ReqwestHttpClient;

/// Streaming response state held between the initial HTTP response and
/// downstream body-delta forwarding.
pub(crate) struct PendingReqwestHttpBodyStream {
    pub(crate) request_id: String,
    pub(crate) response: reqwest::Response,
}

/// Validates `http/request` parameters and runs the actual `reqwest` call used
/// by the exec-server route and the local [`HttpClient`] backend.
pub(crate) struct ReqwestHttpRequestRunner {
    client: reqwest::Client,
    loopback_client: reqwest::Client,
}

impl ReqwestHttpClient {
    fn build_client(
        timeout_ms: Option<u64>,
        redirect_policy: HttpRedirectPolicy,
        disable_proxy: bool,
    ) -> Result<reqwest::Client, ExecServerError> {
        let builder = match timeout_ms {
            None => reqwest::Client::builder(),
            Some(timeout_ms) => {
                reqwest::Client::builder().timeout(Duration::from_millis(timeout_ms))
            }
        };
        let builder = match redirect_policy {
            HttpRedirectPolicy::Follow => builder,
            HttpRedirectPolicy::Stop => builder.redirect(reqwest::redirect::Policy::none()),
        };
        let mut builder = with_chatgpt_cloudflare_cookie_store(builder);
        if disable_proxy {
            // Local MCP capabilities can carry bearer credentials. Never let an ambient
            // HTTP(S)_PROXY observe those headers before the destination Host policy runs.
            builder = builder.no_proxy();
        }
        build_reqwest_client_with_custom_ca(builder)
            .map_err(|error| ExecServerError::HttpRequest(error.to_string()))
    }
}

impl HttpClient for ReqwestHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        async move {
            let runner = ReqwestHttpRequestRunner::new(params.timeout_ms, params.redirect_policy)
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
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
            let runner = ReqwestHttpRequestRunner::new(params.timeout_ms, params.redirect_policy)
                .map_err(|error| ExecServerError::HttpRequest(error.message))?;
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

impl ReqwestHttpRequestRunner {
    pub(crate) fn new(
        timeout_ms: Option<u64>,
        redirect_policy: HttpRedirectPolicy,
    ) -> Result<Self, JSONRPCErrorError> {
        let client = ReqwestHttpClient::build_client(timeout_ms, redirect_policy, false)
            .map_err(|error| internal_error(error.to_string()))?;
        let loopback_client = ReqwestHttpClient::build_client(timeout_ms, redirect_policy, true)
            .map_err(|error| internal_error(error.to_string()))?;
        Ok(Self {
            client,
            loopback_client,
        })
    }

    pub(crate) async fn run(
        &self,
        params: HttpRequestParams,
    ) -> Result<(HttpRequestResponse, Option<PendingReqwestHttpBodyStream>), JSONRPCErrorError>
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
        let client = if url_is_loopback(&url) {
            &self.loopback_client
        } else {
            &self.client
        };
        let mut request = client.request(method.clone(), url).headers(headers);
        if let Some(body) = params.body {
            request = request.body(body.into_inner());
        }

        let response = match request.send().instrument(request_span.clone()).await {
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
                Some(PendingReqwestHttpBodyStream {
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
        pending_stream: PendingReqwestHttpBodyStream,
        notifications: RpcNotificationSender,
    ) {
        let PendingReqwestHttpBodyStream {
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
            let value = HeaderValue::from_str(&header.value).map_err(|error| {
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

fn log_send_error(method: &Method, error: reqwest::Error) {
    let error = error.without_url();
    let source_chain = error_source_chain(&error);
    tracing::warn!(
        http_method = method.as_str(),
        error_is_timeout = error.is_timeout(),
        error_is_connect = error.is_connect(),
        error = %error,
        error_sources = ?source_chain,
        "http/request send failed"
    );
}

fn error_source_chain(error: &reqwest::Error) -> Option<String> {
    let mut sources = Vec::new();
    let mut source = error.source();
    while let Some(error) = source {
        sources.push(error.to_string());
        source = error.source();
    }
    (!sources.is_empty()).then(|| sources.join(": "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::any;
    use wiremock::matchers::header;

    #[test]
    fn loopback_urls_are_selected_for_direct_proxy_bypass() {
        for value in [
            "http://127.0.0.1:4321/mcp",
            "http://127.9.8.7/mcp",
            "http://[::1]:4321/mcp",
            "https://localhost/mcp",
        ] {
            assert!(url_is_loopback(&Url::parse(value).expect("valid test URL")));
        }
        for value in [
            "https://example.com/mcp",
            "http://192.168.1.5/mcp",
            "http://localhost.example/mcp",
        ] {
            assert!(!url_is_loopback(
                &Url::parse(value).expect("valid test URL")
            ));
        }
    }

    #[tokio::test]
    async fn loopback_request_and_bearer_never_reach_a_configured_poison_proxy() {
        let target = MockServer::start().await;
        let poison_proxy = MockServer::start().await;
        Mock::given(any())
            .and(header("authorization", "Bearer personal-browser-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_string("target"))
            .expect(1)
            .mount(&target)
            .await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(502).set_body_string("proxy observed request"))
            .mount(&poison_proxy)
            .await;

        let proxied_builder = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(poison_proxy.uri()).expect("valid poison proxy URL"));
        let direct_builder = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(poison_proxy.uri()).expect("valid poison proxy URL"))
            .no_proxy();
        let runner = ReqwestHttpRequestRunner {
            client: build_reqwest_client_with_custom_ca(with_chatgpt_cloudflare_cookie_store(
                proxied_builder,
            ))
            .expect("proxied client"),
            loopback_client: build_reqwest_client_with_custom_ca(
                with_chatgpt_cloudflare_cookie_store(direct_builder),
            )
            .expect("direct loopback client"),
        };

        let (response, pending) = runner
            .run(HttpRequestParams {
                method: "GET".to_string(),
                url: format!("{}/mcp", target.uri()),
                headers: vec![HttpHeader {
                    name: "Authorization".to_string(),
                    value: "Bearer personal-browser-secret".to_string(),
                }],
                body: None,
                timeout_ms: Some(2_000),
                redirect_policy: HttpRedirectPolicy::Follow,
                request_id: "loopback-no-proxy".to_string(),
                stream_response: false,
            })
            .await
            .expect("loopback request succeeds directly");

        assert_eq!(response.status, 200);
        assert!(pending.is_none());
        assert!(
            poison_proxy
                .received_requests()
                .await
                .expect("proxy requests")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn non_loopback_request_keeps_using_the_configured_proxy() {
        let proxy = MockServer::start().await;
        Mock::given(any())
            .and(header("authorization", "Bearer remote-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_string("proxy"))
            .expect(1)
            .mount(&proxy)
            .await;

        let proxied_builder = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(proxy.uri()).expect("valid proxy URL"));
        let runner = ReqwestHttpRequestRunner {
            client: build_reqwest_client_with_custom_ca(with_chatgpt_cloudflare_cookie_store(
                proxied_builder,
            ))
            .expect("proxied client"),
            loopback_client: build_reqwest_client_with_custom_ca(
                with_chatgpt_cloudflare_cookie_store(reqwest::Client::builder().no_proxy()),
            )
            .expect("direct loopback client"),
        };

        let (response, pending) = runner
            .run(HttpRequestParams {
                method: "GET".to_string(),
                url: "http://mcp.example.test/resource".to_string(),
                headers: vec![HttpHeader {
                    name: "Authorization".to_string(),
                    value: "Bearer remote-secret".to_string(),
                }],
                body: None,
                timeout_ms: Some(2_000),
                redirect_policy: HttpRedirectPolicy::Follow,
                request_id: "remote-through-proxy".to_string(),
                stream_response: false,
            })
            .await
            .expect("non-loopback request succeeds through proxy");

        assert_eq!(response.status, 200);
        assert!(pending.is_none());
        assert_eq!(
            proxy
                .received_requests()
                .await
                .expect("proxy requests")
                .len(),
            1
        );
    }
}
