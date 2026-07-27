use crate::services::agent_registry::AgentRegistry;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::mpsc;

type McpReloadFn = Arc<dyn Fn() + Send + Sync>;

pub struct HotReloader {
    registry: Arc<AgentRegistry>,
    known_mcp_servers: Vec<String>,
    /// Optional callback to reload MCP server configs at runtime.
    mcp_reload: Option<McpReloadFn>,
}

impl HotReloader {
    pub fn new(registry: Arc<AgentRegistry>, known_mcp_servers: Vec<String>) -> Self {
        Self {
            registry,
            known_mcp_servers,
            mcp_reload: None,
        }
    }

    /// Attach a callback that reloads MCP server configs.
    pub fn with_mcp_reload(mut self, reload: McpReloadFn) -> Self {
        self.mcp_reload = Some(reload);
        self
    }
    pub fn watch<P: AsRef<Path>>(&self, path: P) -> notify::Result<()> {
        let (tx, mut rx) = mpsc::channel(100);

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                if let Ok(event) = res {
                    let _ = tx.blocking_send(event);
                }
            },
            Config::default(),
        )?;

        let base_path: PathBuf = path.as_ref().to_path_buf();

        watcher.watch(&base_path, RecursiveMode::Recursive)?;

        let registry = self.registry.clone();
        let known_servers = self.known_mcp_servers.clone();
        let mcp_reload = self.mcp_reload.clone();
        let is_reloading = Arc::new(AtomicBool::new(false));

        let logs_dir = base_path.join("logs");
        let _ = std::fs::create_dir_all(&logs_dir);
        let canonical_logs_dir =
            std::fs::canonicalize(&logs_dir).unwrap_or_else(|_| logs_dir.clone());

        tokio::spawn(async move {
            let _watcher = watcher;

            while let Some(event) = rx.recv().await {
                if !(event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove()) {
                    continue;
                }

                let is_log_event = event.paths.iter().any(|p| {
                    let path_str = p.to_string_lossy();
                    if path_str.contains(".log") {
                        return true;
                    }

                    let canonical_path = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                    canonical_path.starts_with(&canonical_logs_dir)
                });

                if is_log_event {
                    continue;
                }

                is_reloading.store(true, Ordering::SeqCst);

                if let Some(ref reload) = mcp_reload {
                    reload();
                }

                if let Err(e) = registry.reload_all(&known_servers) {
                    tracing::error!("Failed to reload agents: {}", e);
                }
            }
        });

        Ok(())
    }
}
