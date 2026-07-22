use super::*;
use codex_http_client::OutboundProxyPolicy;
use codex_http_client::cache_system_proxy_route_for_test;
use pretty_assertions::assert_eq;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::any;
use wiremock::matchers::header;
use wiremock::matchers::path;

fn request(url: String, redirect_policy: HttpRedirectPolicy) -> HttpRequestParams {
    HttpRequestParams {
        method: "GET".to_string(),
        url,
        headers: vec![HttpHeader {
            name: "Authorization".to_string(),
            value: "Bearer fixture-mcp-token".to_string(),
        }],
        body: None,
        timeout_ms: Some(2_000),
        redirect_policy,
        request_id: "loopback-regression".to_string(),
        stream_response: false,
    }
}

fn client() -> RouteAwareHttpClient {
    RouteAwareHttpClient::new(HttpClientFactory::new(
        OutboundProxyPolicy::RespectSystemProxy,
    ))
}

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
        .and(header("authorization", "Bearer fixture-mcp-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("target"))
        .expect(2)
        .mount(&target)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(502))
        .expect(0)
        .mount(&poison_proxy)
        .await;
    let url = format!("{}/mcp", target.uri());
    cache_system_proxy_route_for_test(&url, poison_proxy.uri());
    for policy in [HttpRedirectPolicy::Follow, HttpRedirectPolicy::Stop] {
        let response = client()
            .http_request(request(url.clone(), policy))
            .await
            .expect("loopback request succeeds directly");
        assert_eq!(response.status, 200);
        assert_eq!(response.body.0, b"target");
    }
    assert!(poison_proxy.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn loopback_cross_origin_redirect_reaches_neither_target_nor_poison_proxy() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;
    let poison_proxy = MockServer::start().await;
    let url = format!("{}/start", source.uri());
    let redirected_url = format!("{}/must-not-be-reached", target.uri());
    cache_system_proxy_route_for_test(&url, poison_proxy.uri());
    cache_system_proxy_route_for_test(&redirected_url, poison_proxy.uri());
    Mock::given(path("/start"))
        .and(header("authorization", "Bearer fixture-mcp-token"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", redirected_url))
        .expect(1)
        .mount(&source)
        .await;
    for server in [&target, &poison_proxy] {
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(server)
            .await;
    }
    let error = client()
        .http_request(request(url, HttpRedirectPolicy::Follow))
        .await
        .expect_err("cross-origin loopback redirect must fail closed");
    assert!(error.to_string().contains("http/request failed"));
    assert!(target.received_requests().await.unwrap().is_empty());
    assert!(poison_proxy.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn loopback_same_origin_redirect_is_followed_directly_with_bearer() {
    let target = MockServer::start().await;
    let poison_proxy = MockServer::start().await;
    let url = format!("{}/start", target.uri());
    let redirected_url = format!("{}/done", target.uri());
    cache_system_proxy_route_for_test(&url, poison_proxy.uri());
    cache_system_proxy_route_for_test(&redirected_url, poison_proxy.uri());
    Mock::given(path("/start"))
        .and(header("authorization", "Bearer fixture-mcp-token"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/done"))
        .expect(1)
        .mount(&target)
        .await;
    Mock::given(path("/done"))
        .and(header("authorization", "Bearer fixture-mcp-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("done"))
        .expect(1)
        .mount(&target)
        .await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(502))
        .expect(0)
        .mount(&poison_proxy)
        .await;
    let response = client()
        .http_request(request(url, HttpRedirectPolicy::Follow))
        .await
        .expect("same-origin redirect succeeds");
    assert_eq!(response.status, 200);
    assert_eq!(response.body.0, b"done");
    assert!(poison_proxy.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn non_loopback_request_keeps_using_the_configured_proxy() {
    let proxy = MockServer::start().await;
    let url = format!(
        "http://mcp.example.test/{}/resource",
        proxy.address().port()
    );
    cache_system_proxy_route_for_test(&url, proxy.uri());
    Mock::given(any())
        .and(header("authorization", "Bearer fixture-mcp-token"))
        .respond_with(ResponseTemplate::new(200).set_body_string("proxy"))
        .expect(1)
        .mount(&proxy)
        .await;
    let response = client()
        .http_request(request(url, HttpRedirectPolicy::Follow))
        .await
        .expect("remote request follows configured proxy");
    assert_eq!(response.status, 200);
    assert_eq!(response.body.0, b"proxy");
    assert_eq!(proxy.received_requests().await.unwrap().len(), 1);
}
