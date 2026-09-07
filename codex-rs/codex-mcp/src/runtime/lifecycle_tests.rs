use std::sync::Arc;

use super::McpRuntime;
use super::PublishedMcpRuntime;

fn empty_publication() -> Arc<PublishedMcpRuntime> {
    McpRuntime::empty(/*prefix_mcp_tool_names*/ false)
        .current
        .load_full()
}

#[tokio::test]
async fn interrupted_refresh_prevents_confirmed_shutdown() {
    let runtime = McpRuntime::empty(/*prefix_mcp_tool_names*/ false);
    drop(runtime.begin_refresh().expect("refresh starts"));
    assert!(runtime.ensure_refresh_confirmed().is_err());
    let error = runtime
        .shutdown_confirmed()
        .await
        .expect_err("interrupted refresh must fail closed");
    assert!(
        error
            .to_string()
            .contains("interrupted MCP refresh left a process lifecycle unconfirmed")
    );
}

#[tokio::test]
async fn published_generations_are_retained_until_confirmed_shutdown() {
    let runtime = McpRuntime::empty(/*prefix_mcp_tool_names*/ false);
    let original = runtime.latest_connections();
    let refresh = runtime.begin_refresh().expect("refresh starts");
    let current = empty_publication();
    let previous = refresh
        .publish(Arc::clone(&current))
        .expect("refresh publishes");
    assert!(Arc::ptr_eq(&previous, &original));
    assert!(
        runtime
            .lifecycle
            .lock()
            .expect("lifecycle lock")
            .retired
            .iter()
            .any(|connections| Arc::ptr_eq(connections, &original))
    );
    refresh
        .confirm(previous, Arc::clone(&current.connections))
        .await
        .expect("predecessor shutdown confirms");
    runtime
        .ensure_refresh_confirmed()
        .expect("refresh confirmed");
    runtime
        .shutdown_confirmed()
        .await
        .expect("empty generations confirm shutdown");
}

#[tokio::test]
async fn interrupted_post_publication_cleanup_retains_predecessor_and_fails_closed() {
    let runtime = McpRuntime::empty(/*prefix_mcp_tool_names*/ false);
    let original = runtime.latest_connections();
    let refresh = runtime.begin_refresh().expect("refresh starts");
    refresh
        .publish(empty_publication())
        .expect("refresh publishes");
    drop(refresh);
    runtime
        .shutdown_confirmed()
        .await
        .expect_err("abandoned predecessor cleanup must fail closed");
    let lifecycle = runtime.lifecycle.lock().expect("lifecycle lock");
    assert!(lifecycle.refresh_tainted);
    assert!(!lifecycle.refresh_in_progress);
    assert!(
        lifecycle
            .retired
            .iter()
            .any(|connections| Arc::ptr_eq(connections, &original))
    );
}

#[tokio::test]
async fn successful_refresh_does_not_clear_earlier_lifecycle_uncertainty() {
    let runtime = McpRuntime::empty(/*prefix_mcp_tool_names*/ false);
    drop(runtime.begin_refresh().expect("first refresh starts"));
    let refresh = runtime
        .begin_refresh()
        .expect("later refresh allowed for cleanup");
    let current = empty_publication();
    let previous = refresh
        .publish(Arc::clone(&current))
        .expect("refresh publishes");
    refresh
        .confirm(previous, Arc::clone(&current.connections))
        .await
        .expect("predecessor shutdown confirms");
    assert!(runtime.ensure_refresh_confirmed().is_err());
    runtime
        .shutdown_confirmed()
        .await
        .expect_err("later success must not erase earlier uncertainty");
}

#[tokio::test]
async fn shutdown_blocks_publication_and_retains_an_already_constructed_generation() {
    let runtime = McpRuntime::empty(/*prefix_mcp_tool_names*/ false);
    let refresh = runtime.begin_refresh().expect("refresh starts");
    runtime
        .shutdown_confirmed()
        .await
        .expect_err("in-progress construction must fail closed");
    let current = empty_publication();
    assert!(refresh.publish(Arc::clone(&current)).is_err());
    assert!(
        runtime
            .lifecycle
            .lock()
            .expect("lifecycle lock")
            .retired
            .iter()
            .any(|connections| Arc::ptr_eq(connections, &current.connections))
    );
    drop(refresh);
    assert!(runtime.begin_refresh().is_err());
}
