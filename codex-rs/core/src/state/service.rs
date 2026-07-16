use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use crate::SkillsService;
use crate::agent::AgentControl;
use crate::attestation::AttestationProvider;
use crate::client::ModelClient;
use crate::config::NetworkProxyAuditMetadata;
use crate::config::StartedNetworkProxy;
use crate::current_time::TimeProvider;
use crate::environment_selection::ThreadEnvironments;
use crate::exec_policy::ExecPolicyManager;
use crate::guardian::GuardianRejection;
use crate::guardian::GuardianRejectionCircuitBreaker;
use crate::mcp::McpManager;
use crate::tools::code_mode::CodeModeService;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::network_approval::NetworkApprovalService;
use crate::tools::sandboxing::ApprovalStore;
use crate::unified_exec::UnifiedExecProcessManager;
use anyhow::Result;
use arc_swap::ArcSwap;
use arc_swap::ArcSwapOption;
use codex_analytics::AnalyticsEventsClient;
use codex_core_plugins::PluginsManager;
use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::ExtensionRegistry;
use codex_hooks::Hooks;
use codex_login::AuthManager;
use codex_mcp::McpConnectionManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_otel::SessionTelemetry;
use codex_rollout::state_db::StateDbHandle;
use codex_rollout_trace::ThreadTraceContext;
use codex_thread_store::LiveThread;
use codex_thread_store::ThreadStore;
use std::path::PathBuf;
use tokio::runtime::Handle;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

pub(crate) struct SessionServices {
    /// The latest manager; callers retain an owned handle while performing MCP I/O.
    pub(crate) mcp_connection_manager: Arc<ArcSwap<McpConnectionManager>>,
    /// Serializes manager construction/replacement with session shutdown. Managers are
    /// registered here before their confirmed shutdown is awaited so cancellation of a
    /// refresh cannot make an older stdio process unreachable to final teardown.
    pub(crate) mcp_connection_manager_lifecycle: Mutex<McpConnectionManagerLifecycle>,
    pub(crate) mcp_startup_cancellation_token: Mutex<CancellationToken>,
    pub(crate) unified_exec_manager: UnifiedExecProcessManager,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) shell_zsh_path: Option<PathBuf>,
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) main_execve_wrapper_exe: Option<PathBuf>,
    pub(crate) analytics_events_client: AnalyticsEventsClient,
    pub(crate) hooks: ArcSwap<Hooks>,
    pub(crate) rollout_thread_trace: ThreadTraceContext,
    pub(crate) user_shell: Arc<crate::shell::Shell>,
    pub(crate) show_raw_agent_reasoning: bool,
    pub(crate) exec_policy: Arc<ExecPolicyManager>,
    pub(crate) auth_manager: Arc<AuthManager>,
    pub(crate) models_manager: SharedModelsManager,
    pub(crate) session_telemetry: SessionTelemetry,
    pub(crate) tool_approvals: Mutex<ApprovalStore>,
    pub(crate) guardian_rejections: Mutex<HashMap<String, GuardianRejection>>,
    pub(crate) guardian_rejection_circuit_breaker: Mutex<GuardianRejectionCircuitBreaker>,
    pub(crate) runtime_handle: Handle,
    pub(crate) skills_service: Arc<SkillsService>,
    pub(crate) plugins_manager: Arc<PluginsManager>,
    pub(crate) mcp_manager: Arc<McpManager>,
    pub(crate) extensions: Arc<ExtensionRegistry<crate::config::Config>>,
    pub(crate) session_extension_data: ExtensionData,
    pub(crate) thread_extension_data: ExtensionData,
    pub(crate) supports_openai_form_elicitation: AtomicBool,
    pub(crate) mcp_thread_init: ExtensionDataInit,
    pub(crate) agent_control: AgentControl,
    pub(crate) network_proxy: ArcSwapOption<StartedNetworkProxy>,
    pub(crate) network_proxy_audit_metadata: NetworkProxyAuditMetadata,
    pub(crate) managed_network_requirements_configured: bool,
    pub(crate) network_approval: Arc<NetworkApprovalService>,
    pub(crate) state_db: Option<StateDbHandle>,
    pub(crate) live_thread: Option<LiveThread>,
    pub(crate) thread_store: Arc<dyn ThreadStore>,
    pub(crate) attestation_provider: Option<Arc<dyn AttestationProvider>>,
    pub(crate) time_provider: Arc<dyn TimeProvider>,
    /// Session-scoped model client shared across turns.
    pub(crate) model_client: ModelClient,
    pub(crate) code_mode_service: CodeModeService,
    pub(crate) tool_search_handler_cache: ToolSearchHandlerCache,
    pub(crate) turn_environments: Arc<ThreadEnvironments>,
}

#[derive(Default)]
pub(crate) struct McpConnectionManagerLifecycle {
    pub(crate) retired: Vec<Arc<McpConnectionManager>>,
    /// Set before refresh construction starts and cleared only after the replacement is
    /// registered and every preceding manager has completed confirmed shutdown.
    pub(crate) refresh_in_progress: bool,
    /// Sticky evidence that an earlier refresh was abandoned while a newly launched stdio
    /// process may still have been unregistered. Later successful refreshes must not erase it.
    pub(crate) refresh_tainted: bool,
}

impl SessionServices {
    /// Installs the manager before validating required servers so startup-time elicitation can
    /// resolve through the session's manager while validation waits.
    pub(crate) async fn install_mcp_connection_manager(
        &self,
        manager: McpConnectionManager,
    ) -> Result<()> {
        let mut lifecycle = self.mcp_connection_manager_lifecycle.lock().await;
        let previous = self.mcp_connection_manager.swap(Arc::new(manager));
        lifecycle.retired.push(Arc::clone(&previous));
        previous.shutdown_confirmed().await?;
        lifecycle
            .retired
            .retain(|retired| !Arc::ptr_eq(retired, &previous));
        self.mcp_connection_manager
            .load_full()
            .validate_required_servers()
            .await
    }

    /// Confirms shutdown for the active manager and every older manager whose prior
    /// refresh teardown did not complete. This is the authority-handoff barrier used by
    /// session shutdown.
    pub(crate) async fn shutdown_mcp_connection_managers_confirmed(&self) -> Result<()> {
        let mut lifecycle = self.mcp_connection_manager_lifecycle.lock().await;
        let current = self.mcp_connection_manager.load_full();
        let mut failures = Vec::new();

        if lifecycle.refresh_in_progress {
            failures.push(
                "an MCP refresh remains in progress or was interrupted before its full process lifecycle was confirmed"
                    .to_string(),
            );
        }
        if lifecycle.refresh_tainted {
            failures.push(
                "an earlier MCP refresh left an unregistered process lifecycle unconfirmed"
                    .to_string(),
            );
        }

        if let Err(error) = current.shutdown_confirmed().await {
            failures.push(format!("active manager: {error:#}"));
        }

        let mut still_unconfirmed = Vec::new();
        for retired in lifecycle.retired.drain(..) {
            match retired.shutdown_confirmed().await {
                Ok(()) => {}
                Err(error) => {
                    failures.push(format!("retired manager: {error:#}"));
                    still_unconfirmed.push(retired);
                }
            }
        }
        lifecycle.retired = still_unconfirmed;

        if failures.is_empty() {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "MCP manager shutdown could not be confirmed: {}",
                failures.join("; ")
            ))
        }
    }
}
