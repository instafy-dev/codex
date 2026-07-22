//! Runtime support for Model Context Protocol (MCP) servers.
//!
//! This module contains the thread-owned MCP runtime and data that describes the
//! environment in which MCP servers execute. Transport startup lives in
//! [`crate::rmcp_client`] and connection-set behavior lives in
//! [`crate::connection_manager`].

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use anyhow::anyhow;
use arc_swap::ArcSwap;
use codex_exec_server::Environment;
use codex_exec_server::EnvironmentManager;
use codex_exec_server::HttpClient;
use codex_exec_server::ReqwestHttpClient;
use codex_protocol::models::PermissionProfile;
use codex_utils_path_uri::PathUri;
use serde::Deserialize;
use serde::Serialize;

use crate::McpConnectionSet;

/// Owns the currently published MCP connection set for one Codex thread.
///
/// Replacements are published atomically. Callers that already hold a snapshot
/// keep the previous connection set alive until their work completes.
pub struct McpRuntime {
    connections: ArcSwap<McpConnectionSet>,
    lifecycle: Mutex<McpRuntimeLifecycle>,
}

#[derive(Default)]
struct McpRuntimeLifecycle {
    retired: Vec<Arc<McpConnectionSet>>,
    refresh_in_progress: bool,
    refresh_tainted: bool,
    shutdown_started: bool,
}

/// Marks the interval in which a replacement connection set may launch processes.
///
/// Dropping this guard before publication permanently taints the runtime. A partly
/// constructed stdio client may otherwise become unreachable before its process group
/// can be confirmed gone, so terminal shutdown must fail closed in that case.
pub struct McpRuntimeRefresh<'a> {
    runtime: &'a McpRuntime,
    completed: bool,
}

/// A newly published generation whose predecessor is still being confirmed shut down.
/// Dropping this value before [`Self::confirm`] taints the runtime and leaves the
/// predecessor registered for terminal cleanup.
pub struct McpRuntimePublication<'a> {
    runtime: &'a McpRuntime,
    connections: Arc<McpConnectionSet>,
    previous: Arc<McpConnectionSet>,
    completed: bool,
}

impl McpRuntime {
    pub fn new(connections: Arc<McpConnectionSet>) -> Self {
        Self {
            connections: ArcSwap::from(connections),
            lifecycle: Mutex::new(McpRuntimeLifecycle::default()),
        }
    }

    pub fn snapshot(&self) -> Arc<McpConnectionSet> {
        self.connections.load_full()
    }

    pub fn begin_refresh(&self) -> Result<McpRuntimeRefresh<'_>> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.shutdown_started {
            return Err(anyhow!("MCP runtime shutdown has already started"));
        }
        if lifecycle.refresh_in_progress {
            lifecycle.refresh_tainted = true;
            return Err(anyhow!(
                "another MCP runtime refresh is already in progress"
            ));
        }
        lifecycle.refresh_in_progress = true;
        Ok(McpRuntimeRefresh {
            runtime: self,
            completed: false,
        })
    }

    pub async fn shutdown(&self) {
        if let Err(error) = self.shutdown_confirmed().await {
            tracing::warn!("MCP runtime shutdown was not fully confirmed: {error:#}");
        }
    }

    /// Stop every connection generation and return only when all owned MCP processes
    /// are confirmed gone. Any interrupted refresh makes confirmation impossible and
    /// is reported permanently so authority-sensitive callers fail closed.
    pub async fn shutdown_confirmed(&self) -> Result<()> {
        let (current, retired, refresh_in_progress, refresh_tainted) = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lifecycle.shutdown_started = true;
            (
                self.snapshot(),
                lifecycle.retired.clone(),
                lifecycle.refresh_in_progress,
                lifecycle.refresh_tainted,
            )
        };

        let mut failures = Vec::new();
        if refresh_in_progress {
            failures.push(
                "an MCP refresh remains in progress, so its process lifecycle is unconfirmed"
                    .to_string(),
            );
        }
        if refresh_tainted {
            failures.push(
                "an interrupted MCP refresh left a process lifecycle unconfirmed".to_string(),
            );
        }
        if let Err(error) = current.shutdown_confirmed().await {
            failures.push(format!("current generation: {error:#}"));
        }
        for (index, connections) in retired.into_iter().enumerate() {
            if Arc::ptr_eq(&connections, &current) {
                continue;
            }
            if let Err(error) = connections.shutdown_confirmed().await {
                failures.push(format!("retired generation {index}: {error:#}"));
            }
        }

        if failures.is_empty() {
            self.lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retired
                .clear();
            Ok(())
        } else {
            Err(anyhow!(
                "MCP runtime shutdown could not be confirmed: {}",
                failures.join("; ")
            ))
        }
    }
}

impl<'a> McpRuntimeRefresh<'a> {
    pub fn publish(mut self, connections: McpConnectionSet) -> Result<McpRuntimePublication<'a>> {
        let mut lifecycle = self
            .runtime
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.shutdown_started {
            lifecycle.refresh_tainted = true;
            return Err(anyhow!(
                "MCP runtime shutdown started before refresh publication"
            ));
        }
        if !lifecycle.refresh_in_progress {
            lifecycle.refresh_tainted = true;
            return Err(anyhow!(
                "MCP runtime refresh publication lost its lifecycle"
            ));
        }

        let connections = Arc::new(connections);
        let previous = self.runtime.connections.swap(Arc::clone(&connections));
        lifecycle.retired.push(Arc::clone(&previous));
        self.completed = true;
        Ok(McpRuntimePublication {
            runtime: self.runtime,
            connections,
            previous,
            completed: false,
        })
    }
}

impl Drop for McpRuntimeRefresh<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut lifecycle = self
            .runtime
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.refresh_in_progress = false;
        lifecycle.refresh_tainted = true;
    }
}

impl McpRuntimePublication<'_> {
    pub fn connections(&self) -> Arc<McpConnectionSet> {
        Arc::clone(&self.connections)
    }

    /// Confirm the previous generation is gone before completing the refresh.
    pub async fn confirm(mut self) -> Result<()> {
        self.previous.shutdown_confirmed().await.map_err(|error| {
            anyhow!("previous MCP generation shutdown could not be confirmed: {error:#}")
        })?;

        let mut lifecycle = self
            .runtime
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle
            .retired
            .retain(|connections| !Arc::ptr_eq(connections, &self.previous));
        lifecycle.refresh_in_progress = false;
        self.completed = true;
        Ok(())
    }
}

impl Drop for McpRuntimePublication<'_> {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let mut lifecycle = self
            .runtime
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle.refresh_in_progress = false;
        lifecycle.refresh_tainted = true;
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxState {
    pub permission_profile: PermissionProfile,
    pub codex_linux_sandbox_exe: Option<PathBuf>,
    pub sandbox_cwd: PathUri,
    #[serde(default)]
    pub use_legacy_landlock: bool,
}

/// Runtime context used when resolving per-server MCP environments.
///
/// `McpConfig` describes what servers exist. This value carries the canonical
/// environment registry plus the local stdio fallback cwd used when a local
/// stdio server omits its own working directory.
#[derive(Clone)]
pub struct McpRuntimeContext {
    environment_manager: Arc<EnvironmentManager>,
    local_stdio_fallback_cwd: PathBuf,
}

impl McpRuntimeContext {
    pub fn new(
        environment_manager: Arc<EnvironmentManager>,
        local_stdio_fallback_cwd: PathBuf,
    ) -> Self {
        Self {
            environment_manager,
            local_stdio_fallback_cwd,
        }
    }

    pub(crate) fn local_stdio_fallback_cwd(&self) -> PathBuf {
        self.local_stdio_fallback_cwd.clone()
    }

    pub(crate) fn resolve_server_environment(
        &self,
        server_name: &str,
        config: &codex_config::McpServerConfig,
    ) -> Result<Option<Arc<Environment>>, String> {
        // Resolve `"local"` through the shared registry when available. Local
        // HTTP is the one current exception: it can use the ambient HTTP client
        // even when no local Environment is configured.
        if let Some(environment) = self
            .environment_manager
            .get_environment(&config.environment_id)
        {
            return Ok(Some(environment));
        }

        if config.is_local_environment() {
            return match config.transport {
                codex_config::McpServerTransportConfig::Stdio { .. } => Err(format!(
                    "local stdio MCP server `{server_name}` requires a local environment"
                )),
                codex_config::McpServerTransportConfig::StreamableHttp { .. } => Ok(None),
            };
        }

        Err(format!(
            "MCP server `{server_name}` references unknown environment id `{}`",
            config.environment_id
        ))
    }

    /// Resolves the HTTP capability owned by the server's configured environment.
    pub fn resolve_http_client(
        &self,
        server_name: &str,
        config: &codex_config::McpServerConfig,
    ) -> Result<Arc<dyn HttpClient>, String> {
        Ok(self
            .resolve_server_environment(server_name, config)?
            .map_or_else(
                || Arc::new(ReqwestHttpClient) as Arc<dyn HttpClient>,
                |environment| environment.get_http_client(),
            ))
    }
}

pub(crate) fn emit_duration(metric: &str, duration: Duration, tags: &[(&str, &str)]) {
    if let Some(metrics) = codex_otel::global() {
        let _ = metrics.record_duration(metric, duration, tags);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID;
    use codex_config::McpServerConfig;
    use codex_config::McpServerTransportConfig;
    use codex_exec_server::EnvironmentManager;
    use codex_utils_path_uri::LegacyAppPathString;
    use pretty_assertions::assert_eq;
    use serde_json::Value;

    use super::*;

    fn stdio_server(environment_id: &str) -> McpServerConfig {
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "echo".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: environment_id.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        }
    }

    fn http_server(environment_id: &str) -> McpServerConfig {
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: "http://127.0.0.1:1".to_string(),
                bearer_token_env_var: None,
                http_headers: None,
                env_http_headers: None,
            },
            environment_id: environment_id.to_string(),
            ..stdio_server(environment_id)
        }
    }

    #[test]
    fn sandbox_state_serializes_skip_missing_entries_as_missing_path_behavior() {
        let sandbox_cwd = PathUri::from_host_native_path(
            std::env::current_dir().expect("current directory should be available"),
        )
        .expect("current directory should convert to a URI");
        let sandbox_state = SandboxState {
            permission_profile: PermissionProfile::workspace_write(),
            codex_linux_sandbox_exe: None,
            sandbox_cwd,
            use_legacy_landlock: false,
        };

        let serialized = serde_json::to_value(&sandbox_state).expect("serialize sandbox state");
        let serialized_text = serde_json::to_string(&serialized).expect("serialize JSON text");
        assert!(
            !serialized_text.contains("generated_default_path"),
            "MCP sandbox metadata must preserve FileSystemPath's stable wire variants"
        );
        assert!(
            !serialized_text.contains("generated_default_special"),
            "MCP sandbox metadata must preserve FileSystemPath's stable wire variants"
        );

        let entries = serialized
            .pointer("/permissionProfile/file_system/entries")
            .and_then(Value::as_array)
            .expect("workspace-write profile should contain filesystem entries");
        let skip_missing_entries = entries
            .iter()
            .filter(|entry| {
                entry.get("missing_path_behavior").and_then(Value::as_str) == Some("skip")
            })
            .collect::<Vec<_>>();
        assert!(
            !skip_missing_entries.is_empty(),
            "skip-missing entries should be represented as optional missing_path_behavior"
        );
        assert!(
            skip_missing_entries.iter().all(|entry| {
                matches!(
                    entry.pointer("/path/type").and_then(Value::as_str),
                    Some("path" | "special")
                )
            }),
            "skip-missing entries should use the stable path/special variants"
        );

        let deserialized: SandboxState =
            serde_json::from_value(serialized).expect("deserialize sandbox state");
        assert_eq!(
            deserialized.permission_profile,
            sandbox_state.permission_profile
        );
    }

    #[test]
    fn local_stdio_requires_local_stdio_availability() {
        let runtime_context = McpRuntimeContext::new(
            Arc::new(EnvironmentManager::without_environments()),
            PathBuf::from("/tmp"),
        );

        let error = match runtime_context
            .resolve_server_environment("stdio", &stdio_server(DEFAULT_MCP_SERVER_ENVIRONMENT_ID))
        {
            Ok(_) => panic!("local stdio MCP should require a local environment"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            "local stdio MCP server `stdio` requires a local environment"
        );
    }

    #[test]
    fn local_http_does_not_require_local_stdio_availability() {
        let runtime_context = McpRuntimeContext::new(
            Arc::new(EnvironmentManager::without_environments()),
            PathBuf::from("/tmp"),
        );

        let resolved_runtime = match runtime_context
            .resolve_server_environment("http", &http_server(DEFAULT_MCP_SERVER_ENVIRONMENT_ID))
        {
            Ok(resolved_runtime) => resolved_runtime,
            Err(error) => panic!("local HTTP MCP should resolve: {error}"),
        };
        assert!(resolved_runtime.is_none());
    }

    #[test]
    fn unknown_explicit_environment_is_rejected() {
        let runtime_context = McpRuntimeContext::new(
            Arc::new(EnvironmentManager::without_environments()),
            PathBuf::from("/tmp"),
        );

        let error =
            match runtime_context.resolve_server_environment("stdio", &stdio_server("remote")) {
                Ok(_) => panic!("unknown MCP environment should fail"),
                Err(error) => error,
            };
        assert_eq!(
            error,
            "MCP server `stdio` references unknown environment id `remote`"
        );
    }

    #[tokio::test]
    async fn explicit_remote_stdio_and_http_accept_named_environment() {
        let runtime_context = McpRuntimeContext::new(
            Arc::new(
                EnvironmentManager::create_for_tests(
                    Some("ws://127.0.0.1:8765".to_string()),
                    /*local_runtime_paths*/ None,
                )
                .await,
            ),
            PathBuf::from("/tmp"),
        );

        let mut remote_stdio = stdio_server("remote");
        let McpServerTransportConfig::Stdio { cwd, .. } = &mut remote_stdio.transport else {
            unreachable!("stdio helper should build stdio transport");
        };
        *cwd = Some(LegacyAppPathString::from_path(&std::env::temp_dir()));
        for resolved_runtime in [
            runtime_context.resolve_server_environment("stdio", &remote_stdio),
            runtime_context.resolve_server_environment("http", &http_server("remote")),
        ] {
            let resolved_runtime = match resolved_runtime {
                Ok(resolved_runtime) => resolved_runtime,
                Err(error) => panic!("remote MCP should resolve: {error}"),
            };
            assert!(resolved_runtime.is_some());
        }
    }

    #[tokio::test]
    async fn remote_stdio_accepts_foreign_absolute_cwd() {
        let runtime_context = McpRuntimeContext::new(
            Arc::new(
                EnvironmentManager::create_for_tests(
                    Some("ws://127.0.0.1:8765".to_string()),
                    /*local_runtime_paths*/ None,
                )
                .await,
            ),
            PathBuf::from("/tmp"),
        );
        let mut remote_stdio = stdio_server("remote");
        let McpServerTransportConfig::Stdio { cwd, .. } = &mut remote_stdio.transport else {
            unreachable!("stdio helper should build stdio transport");
        };
        *cwd = Some(
            PathUri::parse("file:///C:/plugins/demo")
                .expect("foreign cwd URI")
                .into(),
        );

        let resolved_runtime =
            match runtime_context.resolve_server_environment("stdio", &remote_stdio) {
                Ok(resolved_runtime) => resolved_runtime,
                Err(error) => panic!("foreign cwd should resolve: {error}"),
            };
        assert!(resolved_runtime.is_some());
    }

    #[tokio::test]
    async fn local_stdio_accepts_local_environment_when_available() {
        let runtime_context = McpRuntimeContext::new(
            Arc::new(EnvironmentManager::default_for_tests()),
            PathBuf::from("/tmp"),
        );

        let resolved_runtime = match runtime_context
            .resolve_server_environment("stdio", &stdio_server(DEFAULT_MCP_SERVER_ENVIRONMENT_ID))
        {
            Ok(resolved_runtime) => resolved_runtime,
            Err(error) => panic!("local stdio MCP should resolve: {error}"),
        };
        assert!(resolved_runtime.is_some());
    }

    #[tokio::test]
    async fn interrupted_refresh_prevents_confirmed_shutdown() {
        let runtime = McpRuntime::new(Arc::new(McpConnectionSet::empty(
            /*prefix_mcp_tool_names*/ false,
        )));
        let refresh = runtime
            .begin_refresh()
            .expect("refresh should start before shutdown");

        drop(refresh);

        let error = runtime
            .shutdown_confirmed()
            .await
            .expect_err("an interrupted refresh must fail closed");
        assert!(
            error
                .to_string()
                .contains("interrupted MCP refresh left a process lifecycle unconfirmed")
        );
    }

    #[tokio::test]
    async fn published_generations_are_retained_until_confirmed_shutdown() {
        let original = Arc::new(McpConnectionSet::empty(
            /*prefix_mcp_tool_names*/ false,
        ));
        let runtime = McpRuntime::new(Arc::clone(&original));
        let publication = runtime
            .begin_refresh()
            .expect("refresh should start")
            .publish(McpConnectionSet::empty(
                /*prefix_mcp_tool_names*/ false,
            ))
            .expect("refresh should publish");
        let published = publication.connections();

        assert!(Arc::ptr_eq(&runtime.snapshot(), &published));
        assert!(
            runtime
                .lifecycle
                .lock()
                .expect("lifecycle lock should not be poisoned")
                .retired
                .iter()
                .any(|connections| Arc::ptr_eq(connections, &original))
        );
        publication
            .confirm()
            .await
            .expect("previous empty generation should confirm shutdown");
        runtime
            .shutdown_confirmed()
            .await
            .expect("empty generations should confirm shutdown");
    }

    #[tokio::test]
    async fn interrupted_post_publication_cleanup_retains_predecessor_and_fails_closed() {
        let original = Arc::new(McpConnectionSet::empty(
            /*prefix_mcp_tool_names*/ false,
        ));
        let runtime = McpRuntime::new(Arc::clone(&original));
        let publication = runtime
            .begin_refresh()
            .expect("refresh should start")
            .publish(McpConnectionSet::empty(
                /*prefix_mcp_tool_names*/ false,
            ))
            .expect("refresh should publish");

        drop(publication);

        let error = runtime
            .shutdown_confirmed()
            .await
            .expect_err("abandoned predecessor cleanup must fail closed");
        assert!(
            error
                .to_string()
                .contains("interrupted MCP refresh left a process lifecycle unconfirmed")
        );
        let lifecycle = runtime
            .lifecycle
            .lock()
            .expect("lifecycle lock should not be poisoned");
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
        let runtime = McpRuntime::new(Arc::new(McpConnectionSet::empty(
            /*prefix_mcp_tool_names*/ false,
        )));
        drop(runtime.begin_refresh().expect("first refresh should start"));

        runtime
            .begin_refresh()
            .expect("later refresh should still be allowed for cleanup")
            .publish(McpConnectionSet::empty(
                /*prefix_mcp_tool_names*/ false,
            ))
            .expect("later refresh should publish")
            .confirm()
            .await
            .expect("later predecessor shutdown should confirm");

        let error = runtime
            .shutdown_confirmed()
            .await
            .expect_err("later success must not erase earlier uncertainty");
        assert!(
            error
                .to_string()
                .contains("interrupted MCP refresh left a process lifecycle unconfirmed")
        );
    }
}
