//! Contract tests for the Fase 5 history port changes.
//!
//! The first half pins the *shape* of the `ConversationStore` port against an
//! in-memory fake — no SQLite, no Ollama, no network. The second half drives the
//! real SQLite adapter against a temp DB and a one-shot fake Ollama listener, so
//! the adapter behaviour (owner identity, targeted summaries, no-args tool
//! events, per-agent retention) is verified end to end. Boot/purge wiring
//! belongs to the runtime tests.

use async_trait::async_trait;
use llama_r::adapters::history::{NoopSummarizer, OllamaSummarizer, SqliteConversationStore};
use llama_r::config::Config;
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy};
use llama_r::domain::models::ChatMessage;
use llama_r::domain::policy::PermissionPolicy;
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::history::{
    ConversationExport, ConversationRecord, ConversationStore, ConversationSummary, ExportMessage,
    ExportSummary, Summarizer,
};
use llama_r::runtime::{build_agent_retention_map, build_summarizer};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

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
    append_user_calls: usize,
    assistant_messages: Vec<(Option<String>, String)>,
    summarize_ids: Vec<Option<String>>,
    tool_events: Vec<(String, String, bool)>,
    tool_event_attempts: usize,
    fail_tool_events: bool,
    fail_append_user: bool,
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

    /// A store whose `append_tool_event` always fails, to prove best effort.
    fn failing_tool_events() -> Self {
        let store = Self::new();
        store.calls.lock().unwrap().fail_tool_events = true;
        store
    }

    /// A store whose `append_user` always fails: a client id may be supplied,
    /// but no conversation row exists for later writes to attach to.
    fn failing_append_user() -> Self {
        let store = Self::new();
        store.calls.lock().unwrap().fail_append_user = true;
        store
    }

    fn append_user_calls(&self) -> usize {
        self.calls.lock().unwrap().append_user_calls
    }

    fn assistant_messages(&self) -> Vec<(Option<String>, String)> {
        self.calls.lock().unwrap().assistant_messages.clone()
    }

    fn tool_event_attempts(&self) -> usize {
        self.calls.lock().unwrap().tool_event_attempts
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
        let mut calls = self.calls.lock().unwrap();
        calls.append_user_calls += 1;
        if calls.fail_append_user {
            return Err("append_user is down".into());
        }
        Ok(conversation_id.unwrap_or("generated-id").to_string())
    }

    async fn append_assistant(
        &self,
        conversation_id: Option<&str>,
        _agent: &Agent,
        content: &str,
    ) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .assistant_messages
            .push((conversation_id.map(str::to_string), content.to_string()));
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
        let mut calls = self.calls.lock().unwrap();
        calls.tool_event_attempts += 1;
        if calls.fail_tool_events {
            return Err("tool event store is down".into());
        }
        calls
            .tool_events
            .push((conversation_id.to_string(), tool_name.to_string(), success));
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

// ---------------------------------------------------------------------------
// Real SQLite adapter + one-shot fake Ollama listener (Task 2).
// ---------------------------------------------------------------------------

fn named_agent(id: &str, name: &str, project: Option<&str>) -> Agent {
    Agent {
        id: id.to_string(),
        project_id: project.map(str::to_string),
        config: AgentConfig {
            name: name.to_string(),
            model: "llama3.2".into(),
            system_prompt: "test".into(),
            ..AgentConfig::default()
        },
    }
}

/// Agent configured so a single turn triggers a summary immediately.
fn summarizing_agent(id: &str) -> Agent {
    let mut agent = named_agent(id, id, None);
    agent.config.memory.index_summaries = true;
    agent.config.memory.summarize_every_n_turns = 1;
    agent
}

/// Deterministic summarizer injected through `with_summarizer`.
struct FixedSummarizer {
    text: String,
    calls: Arc<Mutex<Vec<Vec<ExportMessage>>>>,
}

#[async_trait]
impl Summarizer for FixedSummarizer {
    async fn summarize(&self, messages: &[ExportMessage]) -> Result<String, String> {
        self.calls.lock().unwrap().push(messages.to_vec());
        Ok(self.text.clone())
    }
}

type ToolEventRow = (String, String, Option<String>, Option<String>, i64);

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// A one-shot HTTP listener that answers a single Ollama `/api/chat` POST and
/// records the path + JSON body it received.
async fn one_shot_ollama(
    reply_content: &str,
) -> (
    String,
    Arc<Mutex<Option<(String, serde_json::Value)>>>,
    JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(None));
    let sink = seen.clone();
    let reply = serde_json::json!({ "message": { "role": "assistant", "content": reply_content } });

    let handle = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let (headers, body_bytes) = loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(end) = find_header_end(&buf) {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let len = head
                    .lines()
                    .find_map(|line| {
                        let (k, v) = line.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if buf.len() >= end + 4 + len {
                    let body = buf[end + 4..end + 4 + len].to_vec();
                    break (head, body);
                }
            }
        };

        let path = headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_string();
        let body: serde_json::Value =
            serde_json::from_slice(&body_bytes).unwrap_or(serde_json::Value::Null);
        *sink.lock().unwrap() = Some((path, body));

        let payload = serde_json::to_vec(&reply).unwrap();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.write_all(&payload).await.unwrap();
        let _ = socket.flush().await;
    });

    (format!("http://{}", addr), seen, handle)
}

fn age_conversation(db_path: &std::path::Path, conversation_id: &str, days_ago: i64) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let old = chrono::Utc::now().timestamp() - days_ago * 86_400;
    conn.execute(
        "UPDATE conversations SET created_at = ?1, updated_at = ?1 WHERE id = ?2",
        rusqlite::params![old, conversation_id],
    )
    .unwrap();
}

#[tokio::test]
async fn conversation_owner_is_agent_id_not_display_name() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("owner.db");
    let store = SqliteConversationStore::open(&db, "http://127.0.0.1:1", "m").unwrap();

    // Two agents that share a *display name* but have different ids.
    let a1 = named_agent("agent-one", "Shared Name", None);
    let a2 = named_agent("agent-two", "Shared Name", None);

    let c1 = store.append_user(None, &a1, "one").await.unwrap();
    let c2 = store.append_user(None, &a2, "two").await.unwrap();
    assert_ne!(c1, c2);

    let r1 = store.get_conversation(&c1).await.unwrap().unwrap();
    let r2 = store.get_conversation(&c2).await.unwrap().unwrap();
    assert_eq!(r1.agent_id, "agent-one");
    assert_eq!(r2.agent_id, "agent-two");
    assert_ne!(
        r1.agent_id, r2.agent_id,
        "the owner column must hold the agent id, not the display name"
    );
}

#[tokio::test]
async fn summary_targets_the_turn_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("summary_target.db");
    let summarizer = Arc::new(FixedSummarizer {
        text: "turn summary".into(),
        calls: Arc::new(Mutex::new(Vec::new())),
    });
    let store = SqliteConversationStore::with_summarizer(&db, summarizer.clone()).unwrap();
    let agent = summarizing_agent("agent-a");

    let c1 = store.append_user(None, &agent, "first turn").await.unwrap();
    let c2 = store
        .append_user(None, &agent, "second turn")
        .await
        .unwrap();

    let result = store
        .maybe_summarize_and_index(Some(&c1), &agent, &sample_scope(), None)
        .await
        .expect("summarize with a turn id")
        .expect("a summary is produced");
    assert_eq!(result.conversation_id, c1);

    let e1 = store.export_conversation(&c1).await.unwrap();
    let e2 = store.export_conversation(&c2).await.unwrap();
    assert_eq!(
        e1.summaries.len(),
        1,
        "the targeted conversation gets a row"
    );
    assert_eq!(
        e2.summaries.len(),
        0,
        "the other conversation stays untouched"
    );

    let calls = summarizer.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].iter().any(|m| m.content == "first turn"));
    assert!(!calls[0].iter().any(|m| m.content == "second turn"));
}

#[tokio::test]
async fn tool_event_stores_no_args() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tool_events.db");
    let store = SqliteConversationStore::open(&db, "http://127.0.0.1:1", "m").unwrap();
    let agent = named_agent("agent-a", "Agent A", None);
    let conv = store.append_user(None, &agent, "hello").await.unwrap();

    store
        .append_tool_event(&conv, "search", true)
        .await
        .unwrap();
    store
        .append_tool_event(&conv, "write", false)
        .await
        .unwrap();

    let conn = rusqlite::Connection::open(&db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT message_id, tool_name, tool_input, tool_output, success \
             FROM tool_events WHERE conversation_id = ?1 ORDER BY created_at ASC, rowid ASC",
        )
        .unwrap();
    let rows: Vec<ToolEventRow> = stmt
        .query_map(rusqlite::params![conv], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1, "search");
    assert_eq!(rows[0].4, 1, "success is stored as 1");
    assert_eq!(rows[1].1, "write");
    assert_eq!(rows[1].4, 0, "failure is stored as 0");
    for row in &rows {
        assert_eq!(row.0, "", "message_id must be empty");
        assert!(row.2.is_none(), "tool_input must be NULL, never arguments");
        assert!(row.3.is_none(), "tool_output must be NULL, never arguments");
    }
}

#[tokio::test]
async fn purge_respects_per_agent_retention() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("retention.db");
    let store = SqliteConversationStore::open(&db, "http://127.0.0.1:1", "m").unwrap();

    let a = named_agent("agent-a", "A", Some("proj"));
    let b = named_agent("agent-b", "B", Some("proj"));
    let c = named_agent("agent-c", "C", Some("proj"));
    let d = named_agent("agent-d", "D", Some("proj"));

    let ca = store.append_user(None, &a, "a").await.unwrap();
    let cb = store.append_user(None, &b, "b").await.unwrap();
    let cc = store.append_user(None, &c, "c").await.unwrap();
    let cd = store.append_user(None, &d, "d").await.unwrap();

    age_conversation(&db, &ca, 100);
    age_conversation(&db, &cb, 100);
    age_conversation(&db, &cc, 100);
    age_conversation(&db, &cd, 10);

    let mut per_agent = HashMap::new();
    per_agent.insert(a.qualified_id(), 365); // keep, despite being old
    per_agent.insert(b.qualified_id(), 7); // purge, overridden low

    let deleted = store
        .purge_old_conversations(30, Some(&per_agent))
        .await
        .unwrap();

    assert!(
        store.get_conversation(&ca).await.unwrap().is_some(),
        "per-agent high retention must survive"
    );
    assert!(
        store.get_conversation(&cb).await.unwrap().is_none(),
        "per-agent low retention must be purged"
    );
    assert!(
        store.get_conversation(&cc).await.unwrap().is_none(),
        "an agent absent from the map uses the 30-day global retention"
    );
    assert!(
        store.get_conversation(&cd).await.unwrap().is_some(),
        "an unknown agent younger than the global retention survives"
    );
    assert_eq!(deleted, 2);
}

#[tokio::test]
async fn zero_global_retention_is_an_absolute_kill_switch() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("retention_zero_override.db");
    let store = SqliteConversationStore::open(&db, "http://127.0.0.1:1", "m").unwrap();

    let agent = named_agent("agent-a", "A", Some("proj"));
    let conv = store.append_user(None, &agent, "old turn").await.unwrap();
    age_conversation(&db, &conv, 100);

    let mut per_agent = HashMap::new();
    per_agent.insert(agent.qualified_id(), 7); // would purge if the global were nonzero

    let deleted = store
        .purge_old_conversations(0, Some(&per_agent))
        .await
        .unwrap();

    assert_eq!(
        deleted, 0,
        "a 0 global retention purges nothing, even with a per-agent override"
    );
    assert!(
        store.get_conversation(&conv).await.unwrap().is_some(),
        "the conversation must survive the absolute kill-switch"
    );
}

#[tokio::test]
async fn ollama_summarizer_wire() {
    let (url, seen, handle) = one_shot_ollama("The user prefers tea.").await;
    let summarizer = OllamaSummarizer::new(url, "llama3.2".to_string());
    let messages = vec![
        ExportMessage {
            id: "m1".into(),
            role: "user".into(),
            content: "I like tea".into(),
            created_at: 1,
        },
        ExportMessage {
            id: "m2".into(),
            role: "assistant".into(),
            content: "Noted".into(),
            created_at: 2,
        },
    ];

    let out = summarizer.summarize(&messages).await.expect("summarize");
    assert_eq!(out, "The user prefers tea.");

    let (path, body) = seen
        .lock()
        .unwrap()
        .clone()
        .expect("one request was recorded");
    assert_eq!(path, "/api/chat", "the Ollama wire path");
    assert_eq!(body["model"], "llama3.2");
    assert_eq!(body["stream"], false);
    assert_eq!(body["messages"][0]["role"], "user");
    let prompt = body["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("user: I like tea"), "prompt: {prompt}");
    assert!(prompt.contains("assistant: Noted"), "prompt: {prompt}");

    handle.await.unwrap();
}

#[tokio::test]
async fn noop_summarizer_is_quiet() {
    let summarizer = NoopSummarizer;
    let out = summarizer.summarize(&[]).await.expect("no-op never errors");
    assert_eq!(out, "", "no summary is an empty string, not an error");
}

// ---------------------------------------------------------------------------
// Conversation id out to the client (Task 3).
// ---------------------------------------------------------------------------

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::Response,
    routing::post,
    Router,
};
use llama_r::adapters::context::NoopCompactor;
use llama_r::adapters::execution::NoOpExecutionStore;
use llama_r::adapters::mcp::StaticMcpRegistry;
use llama_r::api::handlers::{chat, AppState};
use llama_r::api::observability::AppObservability;
use llama_r::config::ProviderKind;
use llama_r::context::analyzer::ContextEnricher;
use llama_r::context::store::ContextStore;
use llama_r::domain::models::{ChatRequest, ChatResponse, ModelInfo};
use llama_r::optimizer::metrics::TokenMetrics;
use llama_r::ports::engine::{
    AgentEngine, AgentRunEvent, AgentRunRequest, AgentRunResult, ToolEvent,
};
use llama_r::providers::registry::ProviderRegistry;
use llama_r::providers::LLMProvider;
use llama_r::services::agent_registry::AgentRegistry;
use llama_r::services::agent_runtime::{AgentRuntime, RuntimeChatRequest};
use llama_r::services::approval::ApprovalGate;
use llama_r::services::provider_stack::ProviderStack;
use llama_r::services::router_config::RouterConfig;
use llama_r::services::skill_manager::SkillManager;
use std::collections::VecDeque;
use std::error::Error;
use std::pin::Pin;
use std::sync::RwLock;
use tokio::sync::{mpsc, RwLock as TokioRwLock};
use tokio_stream::Stream;
use tower::ServiceExt;

/// Returns a fixed answer so a turn reaches the engine without any network.
struct FixedEngine {
    text: String,
}

#[async_trait]
impl AgentEngine for FixedEngine {
    async fn run(&self, _req: AgentRunRequest) -> Result<AgentRunResult, String> {
        Ok(AgentRunResult {
            text: self.text.clone(),
            tool_calls: 0,
            iterations: 0,
            model: "m".into(),
        })
    }

    async fn run_stream(
        &self,
        _req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        let (tx, rx) = mpsc::channel(8);
        let text = self.text.clone();
        tokio::spawn(async move {
            let _ = tx.send(AgentRunEvent::Token { text }).await;
            let _ = tx
                .send(AgentRunEvent::Completed {
                    result: AgentRunResult {
                        text: String::new(),
                        tool_calls: 0,
                        iterations: 0,
                        model: "m".into(),
                    },
                })
                .await;
        });
        Ok(rx)
    }
}

/// The fallback provider is never reached on the engine path; it exists only so
/// a `ProviderStack` can be built without a live upstream.
struct StubProvider;

#[async_trait]
impl LLMProvider for StubProvider {
    fn get_base_url(&self) -> String {
        "http://stub".into()
    }
    async fn health_check(&self) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
    async fn list_models(&self) -> Result<Vec<ModelInfo>, Box<dyn Error + Send + Sync>> {
        Ok(Vec::new())
    }
    async fn chat(&self, _req: ChatRequest) -> Result<ChatResponse, Box<dyn Error + Send + Sync>> {
        Err("stub provider is not used on the engine path".into())
    }
    async fn chat_stream(
        &self,
        _req: ChatRequest,
    ) -> Result<
        Pin<
            Box<
                dyn Stream<
                        Item = Result<
                            llama_r::domain::models::ChatStreamEvent,
                            Box<dyn Error + Send + Sync>,
                        >,
                    > + Send,
            >,
        >,
        Box<dyn Error + Send + Sync>,
    > {
        Err("stub provider is not used on the engine path".into())
    }
}

struct ChatWorld {
    state: Arc<AppState>,
    store: Arc<RecordingStore>,
}

/// A real `AgentRuntime` + real chat dispatch, wired to a fake store and a fake
/// engine. `RecordingStore::append_user` echoes a supplied id and returns the
/// literal `generated-id` when none was supplied, so a test can tell whether the
/// id on the response came from `append_user` rather than a second UUID.
fn chat_world(persist_history: bool) -> ChatWorld {
    let registry = Arc::new(AgentRegistry::new());
    let mut agent = sample_agent(); // id "agent-a", project "proj"
    agent.config.memory.persist_history = persist_history;
    registry.register(agent, sample_scope());

    let store = Arc::new(RecordingStore::new());
    let enricher = Arc::new(ContextEnricher::new(
        Arc::new(ContextStore::new()),
        Arc::new(SkillManager::new()),
        "fallback-model".into(),
        Arc::new(ProviderRegistry::default()),
        ProviderKind::Ollama,
    ));
    let runtime = Arc::new(AgentRuntime {
        registry: registry.clone(),
        engine: Arc::new(FixedEngine {
            text: "hello from engine".into(),
        }),
        history: Some(store.clone()),
        rag: None,
        enricher: enricher.clone(),
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(llama_r::adapters::context::CurrentModelSelector),
    });

    let mut stack = ProviderStack::new(Arc::new(StubProvider), "fallback-model".to_string());
    stack.provider_name = "ollama".to_string();
    stack.agent_runtime = Some(runtime);

    let state = Arc::new(AppState {
        config: llama_r::config::Config::default(),
        services: Arc::new(TokioRwLock::new(Arc::new(stack))),
        agent_registry: registry,
        skill_manager: Arc::new(SkillManager::new()),
        context_store: Arc::new(ContextStore::new()),
        context_enricher: Mutex::new(enricher),
        metrics: Arc::new(TokenMetrics::new()),
        observability: Arc::new(AppObservability::new()),
        api_running: false.into(),
        grpc_running: false.into(),
        logs: Arc::new(Mutex::new(VecDeque::new())),
        known_mcp_servers: RwLock::new(Vec::new()),
        mcp_registry: Arc::new(StaticMcpRegistry::new()),
        history_store: None,
        router_config: Arc::new(RouterConfig::default()),
        router_healthy: false.into(),
        approval_gate: Arc::new(ApprovalGate::default()),
        auto_decider: None,
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(llama_r::adapters::context::CurrentModelSelector),
        execution_store: Arc::new(NoOpExecutionStore),
        delegator: std::sync::OnceLock::new(),
        channel_verifier: Arc::new(llama_r::adapters::channel::HmacSha256Verifier),
        exporter: Arc::new(llama_r::adapters::telemetry::NoopExporter),
    });
    ChatWorld { state, store }
}

async fn post_chat(state: Arc<AppState>, conversation_id: Option<&str>, stream: bool) -> Response {
    let app = Router::new().route("/chat", post(chat)).with_state(state);
    let mut builder = Request::builder()
        .method("POST")
        .uri("/chat")
        .header("content-type", "application/json")
        .header("x-project", "proj")
        .header("x-agent", "agent-a");
    if let Some(id) = conversation_id {
        builder = builder.header("x-conversation-id", id);
    }
    let body = serde_json::json!({
        "model": "fallback-model",
        "messages": [{ "role": "user", "content": "hello" }],
        "stream": stream,
    });
    let request = builder.body(Body::from(body.to_string())).unwrap();
    app.oneshot(request).await.unwrap()
}

fn response_header(resp: &Response) -> Option<String> {
    resp.headers()
        .get("x-conversation-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn json_body(resp: Response) -> serde_json::Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn generated_conversation_id_reaches_json_and_header() {
    let world = chat_world(true);
    let resp = post_chat(world.state.clone(), None, false).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let header = response_header(&resp).expect("X-Conversation-Id header on the JSON response");
    assert_eq!(
        header, "generated-id",
        "the header must carry the id returned by append_user"
    );

    let body = json_body(resp).await;
    assert_eq!(body["conversation_id"].as_str(), Some("generated-id"));
    assert_eq!(body["conversation_id"].as_str(), Some(header.as_str()));
}

#[tokio::test]
async fn supplied_conversation_id_is_echoed_unchanged() {
    let world = chat_world(true);
    let resp = post_chat(world.state.clone(), Some("client-supplied-42"), false).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let header = response_header(&resp).expect("X-Conversation-Id header on the JSON response");
    assert_eq!(header, "client-supplied-42");

    let body = json_body(resp).await;
    assert_eq!(body["conversation_id"].as_str(), Some("client-supplied-42"));
}

#[tokio::test]
async fn no_persist_history_means_no_id_and_no_header() {
    let world = chat_world(false);
    let resp = post_chat(world.state.clone(), None, false).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        response_header(&resp).is_none(),
        "nothing was persisted, so no header"
    );

    let body = json_body(resp).await;
    assert!(
        body.get("conversation_id").is_none(),
        "nothing was persisted, so the field is omitted: {body}"
    );
}

#[tokio::test]
async fn no_persist_history_with_supplied_id_still_no_header() {
    let world = chat_world(false);
    let resp = post_chat(world.state.clone(), Some("client-supplied-42"), false).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(world.store.append_user_calls(), 0, "no history was written");

    assert!(
        response_header(&resp).is_none(),
        "a supplied id must not be echoed when nothing was persisted"
    );

    let body = json_body(resp).await;
    assert!(
        body.get("conversation_id").is_none(),
        "the supplied id must not reach the DTO when nothing was persisted: {body}"
    );
}

#[tokio::test]
async fn old_client_without_the_field_still_parses() {
    // A payload from a client that predates the field: serde must default the
    // missing `Option` to `None`, not reject the response.
    let legacy = serde_json::json!({
        "model": "m",
        "created_at": "now",
        "message": { "role": "assistant", "content": "hi" },
        "done": true,
    });
    let dto: ChatResponse = serde_json::from_value(legacy).expect("legacy payload parses");
    assert!(dto.conversation_id.is_none());

    // And the field is omitted on the wire while it is `None`.
    let wire = serde_json::to_value(&dto).unwrap();
    assert!(
        wire.get("conversation_id").is_none(),
        "skip_serializing_if must omit a None conversation id"
    );
}

#[tokio::test]
async fn generated_conversation_id_reaches_stream_header() {
    let world = chat_world(true);
    let resp = post_chat(world.state.clone(), None, true).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let header = response_header(&resp).expect("X-Conversation-Id header on the SSE response");
    assert_eq!(header, "generated-id");
    // Drain the stream so the turn completes without a dangling task.
    let _ = to_bytes(resp.into_body(), usize::MAX).await;
}

// ---------------------------------------------------------------------------
// Streaming assistant + tool-event sink (Task 4).
// ---------------------------------------------------------------------------

/// A runtime over one in-memory store and an arbitrary engine, so a test can
/// observe exactly what a turn persisted without SQLite or a live upstream.
fn runtime_with(store: Arc<RecordingStore>, engine: Arc<dyn AgentEngine>) -> AgentRuntime {
    let registry = Arc::new(AgentRegistry::new());
    let mut agent = sample_agent();
    agent.config.memory.persist_history = true;
    registry.register(agent, sample_scope());
    AgentRuntime {
        registry,
        engine,
        history: Some(store),
        rag: None,
        enricher: Arc::new(ContextEnricher::new(
            Arc::new(ContextStore::new()),
            Arc::new(SkillManager::new()),
            "fallback-model".into(),
            Arc::new(ProviderRegistry::default()),
            ProviderKind::Ollama,
        )),
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(llama_r::adapters::context::CurrentModelSelector),
    }
}

fn runtime_request() -> RuntimeChatRequest {
    RuntimeChatRequest {
        project_id: Some("proj".into()),
        agent_id: Some("agent-a".into()),
        conversation_id: None,
        user_message: "hello".into(),
        history: Vec::new(),
        model_override: None,
        system_prompt: Some("sys".into()),
        default_model: "fallback-model".into(),
        execution_id: uuid::Uuid::new_v4(),
        max_iterations_override: None,
        shared_budget: None,
        delegation_context: None,
        approval_event_tx: None,
        tool_event_tx: None,
    }
}

/// A token, a completed event carrying the full answer, and one tool event sent
/// through the request's additive sink — the three things the real bridges and
/// the Rig streaming driver emit, without a Rig loop or a network.
struct ToolEventEngine {
    tool_name: String,
    final_text: String,
}

#[async_trait]
impl AgentEngine for ToolEventEngine {
    async fn run(&self, req: AgentRunRequest) -> Result<AgentRunResult, String> {
        if let Some(tx) = &req.tool_event_tx {
            let _ = tx
                .send(ToolEvent {
                    tool_name: self.tool_name.clone(),
                    success: true,
                })
                .await;
        }
        Ok(AgentRunResult {
            text: self.final_text.clone(),
            tool_calls: 1,
            iterations: 1,
            model: "m".into(),
        })
    }

    async fn run_stream(
        &self,
        req: AgentRunRequest,
    ) -> Result<mpsc::Receiver<AgentRunEvent>, String> {
        let (tx, rx) = mpsc::channel(8);
        let sink = req.tool_event_tx.clone();
        let name = self.tool_name.clone();
        let text = self.final_text.clone();
        tokio::spawn(async move {
            if let Some(sink) = sink {
                let _ = sink
                    .send(ToolEvent {
                        tool_name: name,
                        success: true,
                    })
                    .await;
            }
            let _ = tx.send(AgentRunEvent::Token { text: text.clone() }).await;
            let _ = tx
                .send(AgentRunEvent::Completed {
                    result: AgentRunResult {
                        text,
                        tool_calls: 1,
                        iterations: 1,
                        model: "m".into(),
                    },
                })
                .await;
        });
        Ok(rx)
    }
}

/// The forwarder runs on its own task; give it a bounded window to drain.
async fn wait_for(predicate: impl Fn() -> bool) {
    for _ in 0..100 {
        if predicate() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the background forwarder never reached the expected state");
}

#[tokio::test]
async fn chat_stream_persists_final_assistant() {
    let store = Arc::new(RecordingStore::new());
    let runtime = runtime_with(
        store.clone(),
        Arc::new(ToolEventEngine {
            tool_name: "fake__tool".into(),
            final_text: "final streamed answer".into(),
        }),
    );

    let result = runtime
        .chat_stream(runtime_request())
        .await
        .expect("stream");
    assert_eq!(result.conversation_id.as_deref(), Some("generated-id"));

    let mut events = result.events;
    while events.recv().await.is_some() {}

    let assistant = store.assistant_messages();
    assert_eq!(
        assistant.len(),
        1,
        "one assistant row from Completed, not one per token: {assistant:?}"
    );
    assert_eq!(assistant[0].0.as_deref(), Some("generated-id"));
    assert_eq!(assistant[0].1, "final streamed answer");
}

#[tokio::test]
async fn tool_event_sink_carries_no_args() {
    let store = Arc::new(RecordingStore::new());
    let runtime = runtime_with(
        store.clone(),
        Arc::new(ToolEventEngine {
            tool_name: "fake__tool".into(),
            final_text: "done".into(),
        }),
    );

    // A non-streaming turn writes one row through the sink.
    let reply = runtime.chat(runtime_request()).await.expect("chat");
    assert_eq!(reply.text, "done");
    wait_for(|| !store.tool_events().is_empty()).await;

    // A streaming turn writes one row through the very same sink. `ToolEvent`
    // has only `tool_name` and `success` — there is no argument field to leak.
    let streamed = runtime
        .chat_stream(runtime_request())
        .await
        .expect("stream");
    let mut events = streamed.events;
    while events.recv().await.is_some() {}
    wait_for(|| store.tool_events().len() >= 2).await;

    let recorded = store.tool_events();
    assert_eq!(recorded.len(), 2, "one row per tool call: {recorded:?}");
    for (cid, name, success) in &recorded {
        assert_eq!(cid, "generated-id");
        assert_eq!(name, "fake__tool");
        assert!(*success);
    }
}

#[tokio::test]
async fn tool_event_failure_is_best_effort() {
    let store = Arc::new(RecordingStore::failing_tool_events());
    let runtime = runtime_with(
        store.clone(),
        Arc::new(ToolEventEngine {
            tool_name: "fake__tool".into(),
            final_text: "done".into(),
        }),
    );

    let reply = runtime
        .chat(runtime_request())
        .await
        .expect("a store error must not fail the chat");
    assert_eq!(reply.text, "done");
    wait_for(|| store.tool_event_attempts() >= 1).await;
}

#[tokio::test]
async fn failed_append_user_never_writes_assistant_or_tool_rows() {
    let store = Arc::new(RecordingStore::failing_append_user());
    let runtime = runtime_with(
        store.clone(),
        Arc::new(ToolEventEngine {
            tool_name: "fake__tool".into(),
            final_text: "still answered".into(),
        }),
    );

    let mut req = runtime_request();
    req.conversation_id = Some("client-supplied".into());
    let result = runtime
        .chat_stream(req)
        .await
        .expect("a failed append must not fail the stream");
    // Only an actually-persisted id is advertised back to the client.
    assert!(result.conversation_id.is_none());
    let mut events = result.events;
    while events.recv().await.is_some() {}

    // Give any background forwarder a window to write into the wrong id.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(store.append_user_calls(), 1);
    assert!(
        store.assistant_messages().is_empty(),
        "no user row was persisted, so no assistant row may be written: {:?}",
        store.assistant_messages()
    );
    assert_eq!(
        store.tool_event_attempts(),
        0,
        "a conversation that was never created must not collect tool events"
    );
}

// ---------------------------------------------------------------------------
// Boot wiring, purge map and gap-8 concurrency (Task 5).
// ---------------------------------------------------------------------------

/// Two independent callers append to one store at once. The store keeps a
/// `Mutex<Connection>` in WAL mode: both writes must land, neither lost. This is
/// the deliberate concurrency decision for gap 8 — no async mutex, no
/// spawn_blocking, no refactor of the synchronous SQLite operations.
#[tokio::test]
async fn concurrent_appends_keep_both_rows() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("concurrent.db");
    let store = Arc::new(SqliteConversationStore::open(&db, "http://127.0.0.1:1", "m").unwrap());
    let agent = named_agent("agent-a", "A", None);

    let s1 = store.clone();
    let a1 = agent.clone();
    let s2 = store.clone();
    let a2 = agent.clone();
    let first = tokio::spawn(async move { s1.append_user(None, &a1, "one").await.unwrap() });
    let second = tokio::spawn(async move { s2.append_user(None, &a2, "two").await.unwrap() });
    let (c1, c2) = tokio::join!(first, second);
    let (c1, c2) = (c1.unwrap(), c2.unwrap());

    assert_ne!(c1, c2, "each append must create its own conversation id");

    let e1 = store.export_conversation(&c1).await.unwrap();
    let e2 = store.export_conversation(&c2).await.unwrap();
    assert_eq!(e1.messages.len(), 1, "first append's row survived");
    assert_eq!(e1.messages[0].content, "one");
    assert_eq!(e2.messages.len(), 1, "second append's row survived");
    assert_eq!(e2.messages[0].content, "two");
}

/// An Ollama boot installs the Ollama-wire summarizer: calling it must actually
/// POST to `/api/chat`, which the one-shot listener proves.
#[tokio::test]
async fn boot_builds_ollama_summarizer_for_ollama() {
    let (url, seen, handle) = one_shot_ollama("boot summary").await;
    let config = Config {
        ollama_url: url.clone(),
        default_model: "llama3.2".into(),
        ..Config::default()
    };
    let summarizer = build_summarizer(&config, ProviderKind::Ollama, "llama3.2");
    let messages = vec![ExportMessage {
        id: "m1".into(),
        role: "user".into(),
        content: "remember this".into(),
        created_at: 1,
    }];

    let out = summarizer
        .summarize(&messages)
        .await
        .expect("the Ollama summarizer must speak the Ollama wire format");
    assert_eq!(out, "boot summary");

    let (path, body) = seen.lock().unwrap().clone().expect("one request recorded");
    assert_eq!(path, "/api/chat");
    assert_eq!(body["model"], "llama3.2");
    handle.await.unwrap();
}

/// With no usable credential the boot installs the quiet summarizer, never a
/// real one aimed at a wrong/empty endpoint. `is_noop()` is the seam Task 3 uses
/// to pair this with `NoopCompactor` instead of the summarizing one.
#[tokio::test]
async fn boot_builds_noop_summarizer_when_a_key_is_missing() {
    let config = Config {
        provider: ProviderKind::OpenRouter,
        default_model: "m".into(),
        openrouter_api_key: None,
        ..Config::default()
    };
    let summarizer = build_summarizer(&config, ProviderKind::OpenRouter, "m");
    assert!(summarizer.is_noop());
    let messages = vec![ExportMessage {
        id: "m1".into(),
        role: "user".into(),
        content: "remember this".into(),
        created_at: 1,
    }];

    let out = summarizer
        .summarize(&messages)
        .await
        .expect("the no-op summarizer never errors, never dials out");
    assert_eq!(
        out, "",
        "a boot without a key must not POST to an upstream endpoint"
    );
}

/// The purge map is built from `AgentRegistry::list()`, keyed by the same
/// `agent_qualified_id` the SQLite purge reads. A zero (invalid) per-agent
/// retention is warned about and dropped so the global value applies — never
/// clamped. The resulting map is what drives a real purge.
#[tokio::test]
async fn daily_purge_uses_registry_retention_map() {
    let registry = AgentRegistry::new();
    let mut keeper = named_agent("keeper", "Keeper", Some("proj"));
    keeper.config.memory.retention_days = 365;
    let mut invalid = named_agent("invalid", "Invalid", Some("proj"));
    invalid.config.memory.retention_days = 0; // invalid → warning + global
    registry.register(keeper.clone(), sample_scope());
    registry.register(invalid.clone(), sample_scope());

    let map = build_agent_retention_map(&registry).expect("one valid agent yields a map");
    assert_eq!(map.get(&keeper.qualified_id()), Some(&365));
    assert!(
        !map.contains_key(&invalid.qualified_id()),
        "a 0 retention_days is invalid and must fall back to the global value"
    );

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("registry_purge.db");
    let store = SqliteConversationStore::open(&db, "http://127.0.0.1:1", "m").unwrap();
    let other = named_agent("other", "Other", Some("proj")); // absent → global
    let c_keep = store.append_user(None, &keeper, "keep").await.unwrap();
    let c_invalid = store.append_user(None, &invalid, "invalid").await.unwrap();
    let c_other = store.append_user(None, &other, "other").await.unwrap();
    for c in [&c_keep, &c_invalid, &c_other] {
        age_conversation(&db, c, 100);
    }

    let deleted = store.purge_old_conversations(30, Some(&map)).await.unwrap();
    assert!(
        store.get_conversation(&c_keep).await.unwrap().is_some(),
        "the 365-day per-agent override survives"
    );
    assert!(
        store.get_conversation(&c_invalid).await.unwrap().is_none(),
        "the invalid agent falls back to the global 30 days and is purged"
    );
    assert!(
        store.get_conversation(&c_other).await.unwrap().is_none(),
        "an agent absent from the map uses the global 30 days"
    );
    assert_eq!(deleted, 2);
}

/// A non-streaming turn summarizes the conversation it actually persisted, not
/// an id synthesized for the summary call or the "latest conversation" fallback.
#[tokio::test]
async fn non_streaming_turn_summarizes_persisted_conversation_id() {
    let store = Arc::new(RecordingStore::new());
    let registry = Arc::new(AgentRegistry::new());
    let mut agent = sample_agent();
    agent.config.memory.persist_history = true;
    agent.config.memory.index_summaries = true;
    registry.register(agent, sample_scope());
    let runtime = AgentRuntime {
        registry,
        engine: Arc::new(FixedEngine {
            text: "answer".into(),
        }),
        history: Some(store.clone()),
        rag: None,
        enricher: Arc::new(ContextEnricher::new(
            Arc::new(ContextStore::new()),
            Arc::new(SkillManager::new()),
            "fallback-model".into(),
            Arc::new(ProviderRegistry::default()),
            ProviderKind::Ollama,
        )),
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(llama_r::adapters::context::CurrentModelSelector),
    };

    let mut req = runtime_request();
    req.conversation_id = Some("turn-77".into());
    let result = runtime.chat(req).await.expect("chat");
    assert_eq!(result.conversation_id.as_deref(), Some("turn-77"));
    assert_eq!(
        store.summarize_ids(),
        vec![Some("turn-77".to_string())],
        "summarization must target the exact conversation this turn persisted"
    );
}

/// The same persisted-id gate: a failed append leaves no id, so summarization
/// receives `None` (the legacy "latest conversation" path) rather than echoing a
/// client-supplied id that was never written.
#[tokio::test]
async fn summarize_uses_none_when_append_failed() {
    let store = Arc::new(RecordingStore::failing_append_user());
    let registry = Arc::new(AgentRegistry::new());
    let mut agent = sample_agent();
    agent.config.memory.persist_history = true;
    agent.config.memory.index_summaries = true;
    registry.register(agent, sample_scope());
    let runtime = AgentRuntime {
        registry,
        engine: Arc::new(FixedEngine {
            text: "answer".into(),
        }),
        history: Some(store.clone()),
        rag: None,
        enricher: Arc::new(ContextEnricher::new(
            Arc::new(ContextStore::new()),
            Arc::new(SkillManager::new()),
            "fallback-model".into(),
            Arc::new(ProviderRegistry::default()),
            ProviderKind::Ollama,
        )),
        compactor: Arc::new(NoopCompactor),
        selector: Arc::new(llama_r::adapters::context::CurrentModelSelector),
    };

    let mut req = runtime_request();
    req.conversation_id = Some("client-supplied".into());
    let result = runtime
        .chat(req)
        .await
        .expect("a failed append must not fail chat");
    assert_eq!(result.conversation_id, None);
    assert_eq!(
        store.summarize_ids(),
        vec![None],
        "no row was persisted, so no id may reach summarization"
    );
}
