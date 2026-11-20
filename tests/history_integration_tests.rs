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
use llama_r::domain::agent::{Agent, AgentConfig, RagWritePolicy};
use llama_r::domain::models::ChatMessage;
use llama_r::domain::policy::PermissionPolicy;
use llama_r::domain::scope::{AgentScope, ToolsAllow};
use llama_r::ports::history::{
    ConversationExport, ConversationRecord, ConversationStore, ConversationSummary, ExportMessage,
    ExportSummary, Summarizer,
};
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
