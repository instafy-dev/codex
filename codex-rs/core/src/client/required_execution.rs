//! Turn-scoped Instafy requirement for a concrete execution-tool action.

use codex_protocol::models::ResponseItem;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub(crate) struct RequiredExecution(pub(crate) Arc<AtomicBool>);

pub(super) fn is_instafy_required_execution_tool(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::LocalShellCall { .. } => true,
        ResponseItem::FunctionCall {
            name, namespace, ..
        }
        | ResponseItem::CustomToolCall {
            name, namespace, ..
        } => is_execution_tool_name(namespace.as_deref(), name),
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::Message { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::ConfigurationUpdate { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => false,
    }
}

fn is_instafy_browser_execution_tool(namespace: Option<&str>, name: &str) -> bool {
    const BROWSER_EXECUTION_TOOL_NAMES: [&str; 6] =
        ["snapshot", "navigate", "click", "type", "press", "scroll"];

    if namespace.is_some_and(is_instafy_browser_namespace) {
        return BROWSER_EXECUTION_TOOL_NAMES.contains(&name);
    }

    [
        "mcp__instafy_personal_browser__",
        "mcp__instafy_shared_browser__",
    ]
    .into_iter()
    .find_map(|prefix| name.strip_prefix(prefix))
    .is_some_and(|name| BROWSER_EXECUTION_TOOL_NAMES.contains(&name))
}

fn is_instafy_browser_namespace(namespace: &str) -> bool {
    matches!(
        namespace,
        "mcp__instafy_personal_browser" | "mcp__instafy_shared_browser"
    )
}

fn is_instafy_browser_flat_tool_name(name: &str) -> bool {
    name.starts_with("mcp__instafy_personal_browser__")
        || name.starts_with("mcp__instafy_shared_browser__")
}

pub(crate) fn is_execution_tool_name(namespace: Option<&str>, name: &str) -> bool {
    is_instafy_browser_execution_tool(namespace, name)
        || namespace.is_some_and(|namespace| {
            namespace.starts_with("mcp__") && !is_instafy_browser_namespace(namespace)
        })
        || (name.starts_with("mcp__") && !is_instafy_browser_flat_tool_name(name))
        || (namespace.is_none_or(|namespace| namespace == "functions")
            && matches!(
                name,
                "apply_patch" | "exec_command" | "shell" | "shell_command" | "write_stdin"
            ))
}
