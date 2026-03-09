use crate::services::agent_manager::AgentManager;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct HotReloader {
    agent_manager: Arc<AgentManager>,
}

impl HotReloader {
    pub fn new(agent_manager: Arc<AgentManager>) -> Self {
        Self { agent_manager }
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

        watcher.watch(path.as_ref(), RecursiveMode::Recursive)?;

        // Need to move rx and agent_manager to an async task
        let am = self.agent_manager.clone();

        tokio::spawn(async move {
            // Keep the watcher alive inside the task
            let _watcher = watcher;

            while let Some(event) = rx.recv().await {
                if event.kind.is_modify() || event.kind.is_create() || event.kind.is_remove() {
                    log::info!(
                        "File change detected in {:?}, reloading agents...",
                        event.paths
                    );
                    if let Err(e) = am.load_agents() {
                        log::error!("Failed to reload agents: {}", e);
                    }
                }
            }
        });

        Ok(())
    }
}
