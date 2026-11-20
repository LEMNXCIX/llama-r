//! Contract tests for the Fase 5 history port changes.
//!
//! Everything here runs against in-memory fakes: no SQLite, no Ollama, no
//! network. These tests pin the *shape* of the `ConversationStore` port — the
//! real SQLite behaviour belongs to the adapter tests, and the boot/purge
//! wiring belongs to the runtime tests.
//!
//! Later tasks extend this file with adapter-backed cases that reuse the same
//! port contract.

use async_trait::async_trait;
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy};
use llama_r::domain::models::ChatMessage;
use llama_r::domain::policy::PermissionPolicy;
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::history::{
    ConversationExport, ConversationRecord, ConversationStore, ConversationSummary, ExportMessage,
    ExportSummary,
};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

fn sample_agent() -> Agent {
    Agent {
        id: "agent-a".into(),
        project_id: Some("proj".into()),
        config: AgentConfig {
            name: "Agent A".into(),
            model: "llama3.2".into(),
            system_prompt: "test".into(),
            ..AgentConfig::default()
        },
    }
}

fn sample_scope() -> AgentScope {
    AgentScope {
        agent_id: "agent-a".into(),
        project_id: Some("proj".into()),
        mcp_sources: HashSet::new(),
        tools_allow: ToolsAllow::DenyAll,
        local_tools_allow: ToolsAllow::DenyAll,
        rag_sources: HashSet::new(),
        rag_write: RagWritePolicy::None,
        max_tool_calls: 8,
        max_iterations: 6,
        timeout_secs: 90,
        permission: PermissionPolicy::default(),
    }
}

/// Everything a fake store needs to remember for this task's contract tests.
#[derive(Default)]
struct RecordedCalls {
    summarize_ids: Vec<Option<String>>,
    tool_events: Vec<(String, String, bool)>,
    purges: Vec<(u32, Option<HashMap<String, u32>>)>,
}

/// A `ConversationStore` that records only the calls this task cares about and
/// returns inert values for the rest. No I/O, no SQLite.
struct RecordingStore {
    calls: Arc<Mutex<RecordedCalls>>,
}

impl RecordingStore {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(RecordedCalls::default())),
        }
    }

    fn summarize_ids(&self) -> Vec<Option<String>> {
        self.calls.lock().unwrap().summarize_ids.clone()
    }

    fn tool_events(&self) -> Vec<(String, String, bool)> {
        self.calls.lock().unwrap().tool_events.clone()
    }

    fn purges(&self) -> Vec<(u32, Option<HashMap<String, u32>>)> {
        self.calls.lock().unwrap().purges.clone()
    }
}

#[async_trait]
impl ConversationStore for RecordingStore {
    async fn append_user(
        &self,
        conversation_id: Option<&str>,
        _agent: &Agent,
        _content: &str,
    ) -> Result<String, String> {
        Ok(conversation_id.unwrap_or("generated-id").to_string())
    }

    async fn append_assistant(
        &self,
        _conversation_id: Option<&str>,
        _agent: &Agent,
        _content: &str,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn maybe_summarize_and_index(
        &self,
        conversation_id: Option<&str>,
        _agent: &Agent,
        _scope: &AgentScope,
        _rag: Option<Arc<dyn llama_r::ports::rag::RagStore>>,
    ) -> Result<Option<ConversationSummary>, String> {
        self.calls
            .lock()
            .unwrap()
            .summarize_ids
            .push(conversation_id.map(str::to_string));
        Ok(None)
    }

    async fn append_tool_event(
        &self,
        conversation_id: &str,
        tool_name: &str,
        success: bool,
    ) -> Result<(), String> {
        self.calls.lock().unwrap().tool_events.push((
            conversation_id.to_string(),
            tool_name.to_string(),
            success,
        ));
        Ok(())
    }

    async fn get_history(
        &self,
        _conversation_id: &str,
        _limit: usize,
    ) -> Result<Vec<ChatMessage>, String> {
        Ok(Vec::new())
    }

    async fn list_conversations(
        &self,
        _agent_qualified_id: &str,
        _page: usize,
        _page_size: usize,
    ) -> Result<Vec<ConversationRecord>, String> {
        Ok(Vec::new())
    }

    async fn get_conversation(
        &self,
        _conversation_id: &str,
    ) -> Result<Option<ConversationRecord>, String> {
        Ok(None)
    }

    async fn delete_conversation(&self, _conversation_id: &str) -> Result<(), String> {
        Ok(())
    }

    async fn export_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<ConversationExport, String> {
        Ok(ConversationExport {
            record: ConversationRecord {
                id: conversation_id.to_string(),
                agent_qualified_id: String::new(),
                project_id: None,
                agent_id: String::new(),
                turn_count: 0,
                created_at: 0,
                updated_at: 0,
            },
            messages: Vec::<ExportMessage>::new(),
            summaries: Vec::<ExportSummary>::new(),
        })
    }

    async fn purge_old_conversations(
        &self,
        retention_days: u32,
        per_agent_days: Option<&HashMap<String, u32>>,
    ) -> Result<u64, String> {
        self.calls
            .lock()
            .unwrap()
            .purges
            .push((retention_days, per_agent_days.cloned()));
        Ok(0)
    }
}

#[tokio::test]
async fn summarize_receives_the_turn_conversation_id() {
    let store = RecordingStore::new();
    let agent = sample_agent();
    let scope = sample_scope();

    store
        .maybe_summarize_and_index(Some("turn-42"), &agent, &scope, None)
        .await
        .expect("summarize with a turn id");
    // `None` must still be representable: it is the legacy "latest conversation" path.
    store
        .maybe_summarize_and_index(None, &agent, &scope, None)
        .await
        .expect("summarize without a turn id");

    assert_eq!(
        store.summarize_ids(),
        vec![Some("turn-42".to_string()), None],
        "the exact conversation id must reach the store, and `None` must survive too"
    );
}

#[tokio::test]
async fn append_tool_event_records_name_and_success() {
    let store = RecordingStore::new();

    store
        .append_tool_event("conv-1", "search", true)
        .await
        .expect("tool event (success)");
    store
        .append_tool_event("conv-1", "write", false)
        .await
        .expect("tool event (failure)");

    assert_eq!(
        store.tool_events(),
        vec![
            ("conv-1".to_string(), "search".to_string(), true),
            ("conv-1".to_string(), "write".to_string(), false),
        ],
        "tool name and success flag must both reach the store"
    );
}

#[tokio::test]
async fn purge_receives_per_agent_map() {
    let store = RecordingStore::new();
    let mut per_agent = HashMap::new();
    per_agent.insert("proj/agent-a".to_string(), 365);
    per_agent.insert("agent-b".to_string(), 7);

    store
        .purge_old_conversations(90, Some(&per_agent))
        .await
        .expect("purge with a per-agent map");
    store
        .purge_old_conversations(90, None)
        .await
        .expect("purge with the legacy global-only path");

    let purges = store.purges();
    assert_eq!(purges.len(), 2, "both purge calls must be recorded");
    assert_eq!(purges[0].0, 90, "the global retention must be preserved");
    assert_eq!(
        purges[0].1.as_ref(),
        Some(&per_agent),
        "the per-agent map must reach the store unchanged"
    );
    assert_eq!(purges[1].1, None, "the legacy no-map path must survive");
}
