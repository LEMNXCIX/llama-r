use crate::adapters::rig_engine::approval;
use crate::adapters::rig_engine::limits::ToolCallBudget;
use crate::adapters::rig_engine::ScopedMcpTool;
use crate::ports::engine::AgentRunEvent;
use crate::services::approval::ApprovalGate;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

pub fn rig_tool_name(server_id: &str, tool_name: &str) -> String {
    format!("{server_id}__{tool_name}")
}

pub fn parse_rig_tool_name(qualified: &str) -> Option<(String, String)> {
    qualified
        .split_once("__")
        .map(|(s, t)| (s.to_string(), t.to_string()))
}

pub struct McpToolBridge {
    pub inner: ScopedMcpTool,
    pub budget: Arc<Mutex<ToolCallBudget>>,
    /// The run's approval gate, owned by `AppState` (not the hot-swapped stack),
    /// so a settings Apply cannot drop a pending decision.
    pub gate: Arc<ApprovalGate>,
    /// `Some` on the streaming path, where `ApprovalRequired` reaches the SSE
    /// caller; `None` on the non-streaming path, where the pending is discovered
    /// by polling `GET /api/approvals` instead. Both paths block on the gate.
    pub event_tx: Option<mpsc::Sender<AgentRunEvent>>,
}

impl McpToolBridge {
    pub fn new(
        inner: ScopedMcpTool,
        budget: Arc<Mutex<ToolCallBudget>>,
        gate: Arc<ApprovalGate>,
        event_tx: Option<mpsc::Sender<AgentRunEvent>>,
    ) -> Self {
        Self {
            inner,
            budget,
            gate,
            event_tx,
        }
    }

    pub async fn invoke_json(&self, args: Value) -> Result<String, String> {
        // Defense in depth: re-check scope before charging budget or calling MCP.
        if !self
            .inner
            .scope
            .allows_tool(&self.inner.def.server_id, &self.inner.def.name)
        {
            return Err(format!(
                "tool '{}/{}' denied by agent scope",
                self.inner.def.server_id, self.inner.def.name
            ));
        }

        // Authorize (which may block on a human) BEFORE charging the budget: a
        // denied, expired or args-changed call spends nothing. The qualified
        // name is the one an agent manifest's approval rules match on.
        let tool_name = rig_tool_name(&self.inner.def.server_id, &self.inner.def.name);
        approval::authorize(
            &self.gate,
            self.event_tx.as_ref(),
            &self.inner.scope.permission,
            &tool_name,
            &args,
        )
        .await?;

        {
            let mut b = self.budget.lock().await;
            b.try_consume()?;
        }

        self.inner.invoke(args).await
    }
}

impl rig_core::tool::ToolDyn for McpToolBridge {
    fn name(&self) -> String {
        rig_tool_name(&self.inner.def.server_id, &self.inner.def.name)
    }

    fn description(&self) -> String {
        if self.inner.def.description.is_empty() {
            format!("MCP tool {}", self.name())
        } else {
            self.inner.def.description.clone()
        }
    }

    fn parameters(&self) -> Value {
        let schema = &self.inner.def.input_schema;
        if schema.is_object() {
            schema.clone()
        } else {
            serde_json::json!({"type": "object", "properties": {}})
        }
    }

    fn call<'a>(
        &'a self,
        args: String,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<String, rig_core::tool::ToolError>> {
        Box::pin(async move {
            let args_val: Value =
                serde_json::from_str(&args).map_err(rig_core::tool::ToolError::JsonError)?;

            let output = self.invoke_json(args_val).await.map_err(|e| {
                rig_core::tool::ToolError::ToolCallError(Box::new(std::io::Error::other(e)))
            })?;

            Ok(output)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::scope::{AgentScope, ToolsAllow};
    use crate::ports::mcp::{McpCallRequest, McpCallResult, McpClient, McpToolDef};
    use crate::services::approval::ApprovalGate;
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockMcpClient {
        call_count: Arc<AtomicU32>,
    }

    #[async_trait]
    impl McpClient for MockMcpClient {
        async fn list_tools(&self, _server_id: &str) -> Result<Vec<McpToolDef>, String> {
            Ok(vec![])
        }
        async fn call_tool(&self, _req: McpCallRequest) -> Result<McpCallResult, String> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(McpCallResult {
                content: serde_json::json!("mock result"),
                is_error: false,
            })
        }
        async fn health(&self, _server_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn make_scoped_tool(allowed: bool) -> (ScopedMcpTool, Arc<AtomicU32>) {
        let call_count = Arc::new(AtomicU32::new(0));
        let mut mcp_sources = HashSet::new();
        mcp_sources.insert("mock".into());
        let scope = AgentScope {
            agent_id: "test".into(),
            project_id: None,
            mcp_sources,
            tools_allow: if allowed {
                ToolsAllow::AllFromSources
            } else {
                ToolsAllow::DenyAll
            },
            local_tools_allow: ToolsAllow::DenyAll,
            rag_sources: HashSet::new(),
            rag_write: crate::domain::agent::RagWritePolicy::None,
            max_tool_calls: 10,
            max_iterations: 5,
            timeout_secs: 30,
            permission: crate::domain::policy::PermissionPolicy::default(),
        };
        let tool = ScopedMcpTool {
            def: McpToolDef {
                server_id: "mock".into(),
                name: "test_tool".into(),
                description: "A test tool".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
            client: Arc::new(MockMcpClient {
                call_count: call_count.clone(),
            }),
            scope,
        };
        (tool, call_count)
    }

    #[tokio::test]
    async fn allowed_tool_invokes_mcp() {
        let (tool, call_count) = make_scoped_tool(true);
        let bridge = McpToolBridge::new(
            tool,
            Arc::new(Mutex::new(ToolCallBudget::new(5))),
            Arc::new(ApprovalGate::default()),
            None,
        );
        let result = bridge
            .invoke_json(serde_json::json!({"key": "value"}))
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "\"mock result\"");
        assert_eq!(call_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn denied_tool_returns_error() {
        let (tool, call_count) = make_scoped_tool(false);
        let budget = Arc::new(Mutex::new(ToolCallBudget::new(5)));
        let bridge = McpToolBridge::new(
            tool,
            budget.clone(),
            Arc::new(ApprovalGate::default()),
            None,
        );
        let result = bridge
            .invoke_json(serde_json::json!({"key": "value"}))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("denied by agent scope"));
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "denied tools must not call MCP"
        );
        assert_eq!(
            budget.lock().await.used(),
            0,
            "denied tools must not consume budget"
        );
    }

    #[test]
    fn tool_name_format() {
        let name = rig_tool_name("fudi", "list_orders");
        assert_eq!(name, "fudi__list_orders");
        let parsed = parse_rig_tool_name(&name);
        assert_eq!(parsed, Some(("fudi".into(), "list_orders".into())));
    }
}
