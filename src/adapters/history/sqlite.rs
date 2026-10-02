//! SQLite adapter for conversation history and continuous learning.

use crate::domain::agent::Agent;
use crate::domain::models::ChatMessage;
use crate::domain::scope::AgentScope;
use crate::ports::history::{
    ConversationExport, ConversationRecord, ConversationStore, ConversationSummary, ExportMessage,
    ExportSummary,
};
use crate::ports::rag::{RagStore, RagUpsert};
use async_trait::async_trait;
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub struct SqliteConversationStore {
    conn: Mutex<Connection>,
    ollama_url: String,
    summarizer_model: String,
}

impl SqliteConversationStore {
    /// Open or create the database at `path`, run migrations, and return store instance.
    pub fn open(
        path: impl AsRef<Path>,
        ollama_url: impl Into<String>,
        summarizer_model: impl Into<String>,
    ) -> Result<Self, String> {
        let conn = Connection::open(path).map_err(|e| format!("failed to open history db: {e}"))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| format!("sqlite pragma error: {e}"))?;

        let store = Self {
            conn: Mutex::new(conn),
            ollama_url: ollama_url.into(),
            summarizer_model: summarizer_model.into(),
        };

        store.run_migrations()?;
        Ok(store)
    }

    fn run_migrations(&self) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS conversations (
                id                TEXT PRIMARY KEY,
                agent_qualified_id TEXT NOT NULL,
                project_id        TEXT,
                agent_id          TEXT NOT NULL,
                turn_count        INTEGER NOT NULL DEFAULT 0,
                created_at        INTEGER NOT NULL,
                updated_at        INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS messages (
                id              TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                role            TEXT NOT NULL CHECK(role IN ('user', 'assistant', 'system')),
                content         TEXT NOT NULL,
                created_at      INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_messages_conv_created
                ON messages(conversation_id, created_at ASC);

            CREATE TABLE IF NOT EXISTS tool_events (
                id              TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                message_id      TEXT NOT NULL,
                tool_name       TEXT NOT NULL,
                tool_input      TEXT,
                tool_output     TEXT,
                success         INTEGER NOT NULL DEFAULT 1,
                created_at      INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS summaries (
                id              TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
                agent_qualified_id TEXT NOT NULL,
                turn_range_start INTEGER NOT NULL,
                turn_range_end   INTEGER NOT NULL,
                summary_text    TEXT NOT NULL,
                indexed_in_rag  INTEGER NOT NULL DEFAULT 0,
                created_at      INTEGER NOT NULL
            );
            "#,
        )
        .map_err(|e| format!("migration error: {e}"))?;

        Ok(())
    }

    fn ensure_conversation(
        &self,
        conversation_id: Option<&str>,
        agent: &Agent,
    ) -> Result<String, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        let now = chrono::Utc::now().timestamp();
        let agent_qid = agent.qualified_id();

        if let Some(cid) = conversation_id {
            if !cid.trim().is_empty() {
                let mut stmt = conn
                    .prepare("SELECT id FROM conversations WHERE id = ?1")
                    .map_err(|e| format!("db prepare error: {e}"))?;
                let exists = stmt
                    .exists(params![cid])
                    .map_err(|e| format!("db query error: {e}"))?;

                if exists {
                    return Ok(cid.to_string());
                }

                conn.execute(
                    "INSERT INTO conversations (id, agent_qualified_id, project_id, agent_id, turn_count, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
                    params![cid, agent_qid, agent.config.context_project.as_deref(), agent.config.name, now, now],
                )
                .map_err(|e| format!("db insert conversation error: {e}"))?;

                return Ok(cid.to_string());
            }
        }

        let new_id = Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO conversations (id, agent_qualified_id, project_id, agent_id, turn_count, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6)",
            params![new_id, agent_qid, agent.config.context_project.as_deref(), agent.config.name, now, now],
        )
        .map_err(|e| format!("db insert conversation error: {e}"))?;

        Ok(new_id)
    }
}

#[async_trait]
impl ConversationStore for SqliteConversationStore {
    async fn append_user(
        &self,
        conversation_id: Option<&str>,
        agent: &Agent,
        content: &str,
    ) -> Result<String, String> {
        let conv_id = self.ensure_conversation(conversation_id, agent)?;
        let msg_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();

        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;

        conn.execute(
            "INSERT INTO messages (id, conversation_id, role, content, created_at) VALUES (?1, ?2, 'user', ?3, ?4)",
            params![msg_id, conv_id, content, now],
        )
        .map_err(|e| format!("db append user message error: {e}"))?;

        conn.execute(
            "UPDATE conversations SET turn_count = turn_count + 1, updated_at = ?1 WHERE id = ?2",
            params![now, conv_id],
        )
        .map_err(|e| format!("db update turn count error: {e}"))?;

        Ok(conv_id)
    }

    async fn append_assistant(
        &self,
        conversation_id: Option<&str>,
        agent: &Agent,
        content: &str,
    ) -> Result<(), String> {
        let conv_id = self.ensure_conversation(conversation_id, agent)?;
        let msg_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();

        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;

        conn.execute(
            "INSERT INTO messages (id, conversation_id, role, content, created_at) VALUES (?1, ?2, 'assistant', ?3, ?4)",
            params![msg_id, conv_id, content, now],
        )
        .map_err(|e| format!("db append assistant message error: {e}"))?;

        conn.execute(
            "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
            params![now, conv_id],
        )
        .map_err(|e| format!("db update conversation time error: {e}"))?;

        Ok(())
    }

    async fn get_history(
        &self,
        conversation_id: &str,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;

        let mut stmt = conn
            .prepare("SELECT role, content FROM messages WHERE conversation_id = ?1 ORDER BY created_at ASC")
            .map_err(|e| format!("db prepare error: {e}"))?;

        let rows = stmt
            .query_map(params![conversation_id], |row| {
                Ok(ChatMessage {
                    role: row.get(0)?,
                    content: row.get(1)?,
                })
            })
            .map_err(|e| format!("db query map error: {e}"))?;

        let mut msgs = Vec::new();
        for r in rows {
            msgs.push(r.map_err(|e| format!("db row read error: {e}"))?);
        }

        if limit > 0 && msgs.len() > limit {
            let start = msgs.len() - limit;
            msgs = msgs.split_off(start);
        }

        Ok(msgs)
    }

    async fn list_conversations(
        &self,
        agent_qualified_id: &str,
        page: usize,
        page_size: usize,
    ) -> Result<Vec<ConversationRecord>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;

        let limit = page_size.max(1);
        let offset = page * limit;

        let mut stmt = conn
            .prepare(
                "SELECT id, agent_qualified_id, project_id, agent_id, turn_count, created_at, updated_at \
                 FROM conversations WHERE agent_qualified_id = ?1 ORDER BY updated_at DESC LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| format!("db prepare error: {e}"))?;

        let rows = stmt
            .query_map(
                params![agent_qualified_id, limit as i64, offset as i64],
                |row| {
                    Ok(ConversationRecord {
                        id: row.get(0)?,
                        agent_qualified_id: row.get(1)?,
                        project_id: row.get(2)?,
                        agent_id: row.get(3)?,
                        turn_count: row.get(4)?,
                        created_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                },
            )
            .map_err(|e| format!("db query map error: {e}"))?;

        let mut records = Vec::new();
        for r in rows {
            records.push(r.map_err(|e| format!("db row read error: {e}"))?);
        }

        Ok(records)
    }

    async fn get_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationRecord>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;

        let mut stmt = conn
            .prepare(
                "SELECT id, agent_qualified_id, project_id, agent_id, turn_count, created_at, updated_at \
                 FROM conversations WHERE id = ?1",
            )
            .map_err(|e| format!("db prepare error: {e}"))?;

        let mut rows = stmt
            .query_map(params![conversation_id], |row| {
                Ok(ConversationRecord {
                    id: row.get(0)?,
                    agent_qualified_id: row.get(1)?,
                    project_id: row.get(2)?,
                    agent_id: row.get(3)?,
                    turn_count: row.get(4)?,
                    created_at: row.get(5)?,
                    updated_at: row.get(6)?,
                })
            })
            .map_err(|e| format!("db query map error: {e}"))?;

        if let Some(r) = rows.next() {
            let record = r.map_err(|e| format!("db row read error: {e}"))?;
            Ok(Some(record))
        } else {
            Ok(None)
        }
    }

    async fn delete_conversation(&self, conversation_id: &str) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;

        conn.execute(
            "DELETE FROM conversations WHERE id = ?1",
            params![conversation_id],
        )
        .map_err(|e| format!("db delete error: {e}"))?;

        Ok(())
    }

    async fn export_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<ConversationExport, String> {
        let record = self
            .get_conversation(conversation_id)
            .await?
            .ok_or_else(|| format!("conversation '{conversation_id}' not found"))?;

        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;

        let mut msg_stmt = conn
            .prepare("SELECT id, role, content, created_at FROM messages WHERE conversation_id = ?1 ORDER BY created_at ASC")
            .map_err(|e| format!("db prepare msg error: {e}"))?;

        let msg_rows = msg_stmt
            .query_map(params![conversation_id], |row| {
                Ok(ExportMessage {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    content: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .map_err(|e| format!("db query msg error: {e}"))?;

        let mut messages = Vec::new();
        for r in msg_rows {
            messages.push(r.map_err(|e| format!("msg read error: {e}"))?);
        }

        let mut sum_stmt = conn
            .prepare("SELECT id, turn_range_start, turn_range_end, summary_text, indexed_in_rag, created_at FROM summaries WHERE conversation_id = ?1 ORDER BY created_at ASC")
            .map_err(|e| format!("db prepare sum error: {e}"))?;

        let sum_rows = sum_stmt
            .query_map(params![conversation_id], |row| {
                let indexed: i64 = row.get(4)?;
                Ok(ExportSummary {
                    id: row.get(0)?,
                    turn_range_start: row.get(1)?,
                    turn_range_end: row.get(2)?,
                    summary_text: row.get(3)?,
                    indexed_in_rag: indexed != 0,
                    created_at: row.get(5)?,
                })
            })
            .map_err(|e| format!("db query sum error: {e}"))?;

        let mut summaries = Vec::new();
        for r in sum_rows {
            summaries.push(r.map_err(|e| format!("sum read error: {e}"))?);
        }

        Ok(ConversationExport {
            record,
            messages,
            summaries,
        })
    }

    async fn purge_old_conversations(&self, retention_days: u32) -> Result<u64, String> {
        // 0 means "never purge" (documented in .env.example / AGENTS.md).
        // Without this guard the cutoff would be `now`, deleting every conversation.
        if retention_days == 0 {
            return Ok(0);
        }
        let cutoff = chrono::Utc::now().timestamp() - (retention_days as i64 * 86_400);
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;

        let deleted = conn
            .execute(
                "DELETE FROM conversations WHERE updated_at < ?1",
                params![cutoff],
            )
            .map_err(|e| format!("purge db error: {e}"))?;

        Ok(deleted as u64)
    }

    async fn maybe_summarize_and_index(
        &self,
        agent: &Agent,
        scope: &AgentScope,
        rag: Option<Arc<dyn RagStore>>,
    ) -> Result<Option<ConversationSummary>, String> {
        if !agent.config.memory.index_summaries {
            return Ok(None);
        }

        let qid = agent.qualified_id();
        let interval = agent.config.memory.summarize_every_n_turns.max(1);

        let (conv_id, turn_count) = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| format!("lock poisoned: {e}"))?;
            let mut stmt = conn
                .prepare("SELECT id, turn_count FROM conversations WHERE agent_qualified_id = ?1 ORDER BY updated_at DESC LIMIT 1")
                .map_err(|e| format!("db prepare error: {e}"))?;

            let mut rows = stmt
                .query_map(params![qid], |row| {
                    let id: String = row.get(0)?;
                    let turns: u32 = row.get(1)?;
                    Ok((id, turns))
                })
                .map_err(|e| format!("query error: {e}"))?;

            if let Some(r) = rows.next() {
                r.map_err(|e| format!("row read error: {e}"))?
            } else {
                return Ok(None);
            }
        };

        if turn_count == 0 || turn_count % interval != 0 {
            return Ok(None);
        }

        let turn_start = turn_count.saturating_sub(interval);
        let turn_end = turn_count;

        {
            let conn = self
                .conn
                .lock()
                .map_err(|e| format!("lock poisoned: {e}"))?;
            let mut stmt = conn
                .prepare("SELECT COUNT(*) FROM summaries WHERE conversation_id = ?1 AND turn_range_end = ?2")
                .map_err(|e| format!("db prepare check error: {e}"))?;
            let count: i64 = stmt
                .query_row(params![conv_id, turn_end], |r| r.get(0))
                .unwrap_or(0);
            if count > 0 {
                return Ok(None);
            }
        }

        let recent_msgs = self.get_history(&conv_id, (interval * 2) as usize).await?;
        if recent_msgs.is_empty() {
            return Ok(None);
        }

        let mut prompt = String::from(
            "Summarize the following conversation turns in 2-3 sentences, focusing on key decisions and context for future reference:\n\n",
        );
        for m in &recent_msgs {
            prompt.push_str(&format!("{}: {}\n", m.role, m.content));
        }

        let url = if self.ollama_url.ends_with('/') {
            format!("{}api/chat", self.ollama_url)
        } else {
            format!("{}/api/chat", self.ollama_url)
        };

        let body = serde_json::json!({
            "model": self.summarizer_model,
            "messages": [
                { "role": "user", "content": prompt }
            ],
            "stream": false
        });

        let http_client = reqwest::Client::new();
        let resp = match http_client
            .post(&url)
            .timeout(std::time::Duration::from_secs(30))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => return Err(format!("summarizer http request error: {e}")),
        };

        if !resp.status().is_success() {
            return Err(format!("summarizer http error status: {}", resp.status()));
        }

        let json_val: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Err(format!("summarizer json parse error: {e}")),
        };

        let summary_text = json_val
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();

        if summary_text.is_empty() {
            return Ok(None);
        }

        let summary_id = Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp();
        let target_source_id = agent
            .config
            .memory
            .summary_collection
            .clone()
            .unwrap_or_else(|| agent.memory_source_id());

        let mut indexed = false;
        if let Some(rag_store) = &rag {
            if scope.allows_rag_write(&target_source_id) {
                let doc = RagUpsert {
                    source_id: target_source_id.clone(),
                    id: format!("summary-{}", summary_id),
                    text: summary_text.clone(),
                    metadata: serde_json::json!({
                        "conversation_id": conv_id,
                        "turn_start": turn_start,
                        "turn_end": turn_end,
                        "agent_qualified_id": qid,
                    }),
                };

                if let Ok(_) = rag_store.upsert_scoped(scope, vec![doc]).await {
                    indexed = true;
                }
            }
        }

        {
            let conn = self
                .conn
                .lock()
                .map_err(|e| format!("lock poisoned: {e}"))?;
            conn.execute(
                "INSERT INTO summaries (id, conversation_id, agent_qualified_id, turn_range_start, turn_range_end, summary_text, indexed_in_rag, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![summary_id, conv_id, qid, turn_start, turn_end, summary_text, if indexed { 1 } else { 0 }, now],
            )
            .map_err(|e| format!("db insert summary error: {e}"))?;
        }

        Ok(Some(ConversationSummary {
            conversation_id: conv_id,
            agent_qualified_id: qid,
            summary: summary_text,
            turn_count,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentConfig;
    use tempfile::tempdir;

    fn make_test_agent(name: &str, project: Option<&str>) -> Agent {
        Agent {
            id: name.to_string(),
            project_id: project.map(|s| s.to_string()),
            config: AgentConfig {
                name: name.to_string(),
                model: "llama3.2".to_string(),
                system_prompt: "test".to_string(),
                context_project: project.map(|s| s.to_string()),
                ..AgentConfig::default()
            },
        }
    }

    #[tokio::test]
    async fn purge_with_zero_retention_keeps_history() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("retention_zero.db");
        let store = SqliteConversationStore::open(&db_path, "http://localhost:11434", "llama3.2")
            .expect("Failed to open db");
        let agent = make_test_agent("agent1", Some("projA"));

        let conv = store
            .append_user(None, &agent, "hello")
            .await
            .expect("append");
        store
            .append_assistant(Some(&conv), &agent, "hi")
            .await
            .expect("append");

        // Age the row by 100 days so "never purge" is actually exercised:
        // a zero-retention purge must not touch it.
        let old_time = chrono::Utc::now().timestamp() - (100 * 86_400);
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
                params![old_time, conv],
            )
            .unwrap();
        }

        // retention_days = 0 must mean "never purge", not "delete everything".
        let deleted = store.purge_old_conversations(0).await.expect("purge");
        assert_eq!(deleted, 0, "retention 0 must not delete any conversation");

        let after = store.get_conversation(&conv).await.expect("get");
        assert!(
            after.is_some(),
            "conversation must survive a zero-retention purge"
        );
    }

    #[tokio::test]
    async fn purge_with_positive_retention_still_deletes_old() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("retention_positive.db");
        let store = SqliteConversationStore::open(&db_path, "http://localhost:11434", "llama3.2")
            .expect("Failed to open db");
        let agent = make_test_agent("agent1", Some("projA"));

        let old = store
            .append_user(None, &agent, "hello")
            .await
            .expect("append");

        // Age the row by 2 days so it is genuinely older than the 1-day cutoff.
        let old_time = chrono::Utc::now().timestamp() - (2 * 86_400);
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
                params![old_time, old],
            )
            .unwrap();
        }

        let deleted = store.purge_old_conversations(1).await.expect("purge");
        assert_eq!(deleted, 1, "a conversation older than 1 day must be purged");
        assert!(store.get_conversation(&old).await.expect("get").is_none());
    }

    #[tokio::test]
    async fn test_sqlite_conversation_store_crud_and_isolation() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test_history.db");

        let store = SqliteConversationStore::open(&db_path, "http://localhost:11434", "llama3.2")
            .expect("Failed to open db");

        let agent1 = make_test_agent("agent1", Some("projA"));
        let agent2 = make_test_agent("agent2", Some("projA"));

        // 1. Append messages for agent 1
        let conv_id1 = store
            .append_user(None, &agent1, "Hello from user 1")
            .await
            .unwrap();
        store
            .append_assistant(Some(&conv_id1), &agent1, "Hello back from assistant 1")
            .await
            .unwrap();

        // 2. Append messages for agent 2
        let conv_id2 = store
            .append_user(None, &agent2, "Hello from user 2")
            .await
            .unwrap();
        store
            .append_assistant(Some(&conv_id2), &agent2, "Hello back from assistant 2")
            .await
            .unwrap();

        // 3. Verify history for agent 1
        let history1 = store.get_history(&conv_id1, 10).await.unwrap();
        assert_eq!(history1.len(), 2);
        assert_eq!(history1[0].role, "user");
        assert_eq!(history1[0].content, "Hello from user 1");
        assert_eq!(history1[1].role, "assistant");
        assert_eq!(history1[1].content, "Hello back from assistant 1");

        // 4. Verify isolation: list_conversations for agent 1 should only return conv_id1
        let list1 = store
            .list_conversations(&agent1.qualified_id(), 0, 10)
            .await
            .unwrap();
        assert_eq!(list1.len(), 1);
        assert_eq!(list1[0].id, conv_id1);

        let list2 = store
            .list_conversations(&agent2.qualified_id(), 0, 10)
            .await
            .unwrap();
        assert_eq!(list2.len(), 1);
        assert_eq!(list2[0].id, conv_id2);

        // 5. Export test
        let export1 = store.export_conversation(&conv_id1).await.unwrap();
        assert_eq!(export1.record.id, conv_id1);
        assert_eq!(export1.messages.len(), 2);

        // 6. Delete test
        store.delete_conversation(&conv_id1).await.unwrap();
        let get_deleted = store.get_conversation(&conv_id1).await.unwrap();
        assert!(get_deleted.is_none());
    }

    #[tokio::test]
    async fn test_sqlite_purge_old_conversations() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test_purge.db");

        let store = SqliteConversationStore::open(&db_path, "http://localhost:11434", "llama3.2")
            .expect("Failed to open db");

        let agent = make_test_agent("agent1", None);
        let conv_id = store
            .append_user(None, &agent, "Old conversation message")
            .await
            .unwrap();

        // Artificially update created_at and updated_at to 100 days ago
        let old_time = chrono::Utc::now().timestamp() - (100 * 86_400);
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
                params![old_time, conv_id],
            )
            .unwrap();
        }

        let purged_count = store.purge_old_conversations(90).await.unwrap();
        assert_eq!(purged_count, 1);

        let record = store.get_conversation(&conv_id).await.unwrap();
        assert!(record.is_none());
    }
}
