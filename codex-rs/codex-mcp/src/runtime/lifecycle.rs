//! Confirmed ownership of MCP transports across refresh and terminal shutdown.

use std::sync::Arc;

use anyhow::Result;
use anyhow::anyhow;

use super::McpConnectionSet;
use super::McpRuntime;
use super::PublishedMcpRuntime;

#[derive(Default)]
pub(super) struct McpRuntimeLifecycle {
    retired: Vec<Arc<McpConnectionSet>>,
    refresh_in_progress: bool,
    refresh_tainted: bool,
    shutdown_started: bool,
}

pub(super) struct McpRuntimeRefresh<'a> {
    runtime: &'a McpRuntime,
    completed: bool,
}

impl McpRuntime {
    pub(super) fn begin_refresh(&self) -> Result<McpRuntimeRefresh<'_>> {
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

    /// Reject use after a refresh whose transport cleanup could not be confirmed.
    pub fn ensure_refresh_confirmed(&self) -> Result<()> {
        let lifecycle = self
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.refresh_tainted || lifecycle.refresh_in_progress {
            return Err(anyhow!(
                "MCP runtime refresh cleanup could not be confirmed"
            ));
        }
        if lifecycle.shutdown_started {
            return Err(anyhow!("MCP runtime shutdown has already started"));
        }
        Ok(())
    }

    /// Stop every connection generation and fail closed if a refresh was interrupted
    /// or any owned MCP transport could not be confirmed terminated.
    pub async fn shutdown_confirmed(&self) -> Result<()> {
        let (current, retired, refresh_in_progress, refresh_tainted) = {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            lifecycle.shutdown_started = true;
            (
                self.latest_connections(),
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

impl McpRuntimeRefresh<'_> {
    pub(super) fn publish(
        &self,
        current: Arc<PublishedMcpRuntime>,
    ) -> Result<Arc<McpConnectionSet>> {
        let mut lifecycle = self
            .runtime
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if lifecycle.shutdown_started {
            // Construction may already have launched processes. Retain that generation
            // even when it can no longer become visible to a new turn.
            lifecycle.retired.push(Arc::clone(&current.connections));
            lifecycle.refresh_tainted = true;
            return Err(anyhow!(
                "MCP runtime shutdown started before refresh publication"
            ));
        }
        let previous = self.runtime.current.swap(current);
        lifecycle.retired.push(Arc::clone(&previous.connections));
        Ok(Arc::clone(&previous.connections))
    }

    pub(super) async fn confirm(
        mut self,
        previous: Arc<McpConnectionSet>,
        current: Arc<McpConnectionSet>,
    ) -> Result<()> {
        // Upstream now reuses unchanged connections. Only terminate the predecessor's
        // removed/replaced clients; shared clients remain owned by the new generation.
        previous
            .shutdown_replaced_connections_confirmed(&current)
            .await?;
        let mut lifecycle = self
            .runtime
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lifecycle
            .retired
            .retain(|connections| !Arc::ptr_eq(connections, &previous));
        lifecycle.refresh_in_progress = false;
        self.completed = true;
        Ok(())
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

#[cfg(test)]
#[path = "lifecycle_tests.rs"]
mod tests;
