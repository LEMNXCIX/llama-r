//! SQLite adapter for durable agent executions.
//!
//! One `Mutex<Connection>` guards `history.db`, matching the conversation
//! history adapter. The schema is purely additive: the four Fase 7 tables are
//! created alongside the pre-existing history tables, so an old db opens
//! without a migration step. The `history` feature owns the `rusqlite`
//! dependency; this module is only compiled when that feature is on.
//!
//! Enums and timestamps are stored as JSON text / RFC 3339 respectively, so
//! reading a row is a lossless serde round trip. Secret-bearing fields are
//! redacted with the shared `services::redaction::redact_secrets` on every
//! write (event payloads, pending-approval args and structured user messages).

use crate::domain::execution::{AgentExecution, ExecutionEvent, ExecutionStatus};
use crate::ports::execution::{ApprovalDecisionRecord, ExecutionStore, PendingApprovalRecord};
use crate::services::redaction::{redact_secrets, redact_trace_payload};
use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, params_from_iter, Connection};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Mutex;
use uuid::Uuid;

/// Additive schema. `IF NOT EXISTS` everywhere: an old `history.db` must open
/// and keep its legacy rows. `UNIQUE(execution_id, seq)` plus the atomic
/// `MAX(seq)+1` insert in `append_event` make event sequence numbers monotonic.
const MIGRATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS executions (
    id                 TEXT PRIMARY KEY,
    conversation_id    TEXT,
    agent_qualified_id TEXT NOT NULL,
    provider           TEXT NOT NULL,
    model              TEXT NOT NULL,
    status             TEXT NOT NULL,
    current_step       INTEGER NOT NULL DEFAULT 0,
    user_message       TEXT,
    user_message_hash  TEXT NOT NULL,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    finished_at        TEXT,
    error              TEXT
);
CREATE INDEX IF NOT EXISTS idx_executions_status ON executions(status);
CREATE INDEX IF NOT EXISTS idx_executions_created ON executions(created_at DESC);

CREATE TABLE IF NOT EXISTS execution_events (
    id           TEXT PRIMARY KEY,
    execution_id TEXT NOT NULL REFERENCES executions(id) ON DELETE CASCADE,
    seq          INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    payload      TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    UNIQUE(execution_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_events_execution_seq ON execution_events(execution_id, seq ASC);

CREATE TABLE IF NOT EXISTS tool_operations (
    operation_id TEXT PRIMARY KEY,
    result_json  TEXT NOT NULL,
    created_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS pending_approvals (
    id           TEXT PRIMARY KEY,
    execution_id TEXT,
    tool_name    TEXT NOT NULL,
    args         TEXT NOT NULL,
    args_hash    TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    decided      INTEGER NOT NULL DEFAULT 0,
    decision     TEXT,
    decided_at   TEXT
);
CREATE INDEX IF NOT EXISTS idx_pending_approvals_execution ON pending_approvals(execution_id);
"#;

const EXECUTION_COLUMNS: &str =
    "id, conversation_id, agent_qualified_id, provider, model, status, current_step, \
     user_message, user_message_hash, created_at, updated_at, finished_at, error";

/// SQLite-backed [`ExecutionStore`]. Cheap to clone-free-share: all callers go
/// through the same `Arc`, and the mutex serialises writes (including the
/// read-modify-write that assigns an event's `seq`).
pub struct SqliteExecutionStore {
    conn: Mutex<Connection>,
}

impl SqliteExecutionStore {
    /// Open (or create) the durable execution database.
    ///
    /// `path` is normally the same `data/history.db` the conversation store
    /// uses; the new tables are additive and never disturb legacy ones.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, String> {
        let conn =
            Connection::open(path).map_err(|e| format!("failed to open execution db: {e}"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| format!("sqlite pragma error: {e}"))?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        store.run_migrations()?;
        Ok(store)
    }

    fn run_migrations(&self) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        conn.execute_batch(MIGRATIONS)
            .map_err(|e| format!("execution schema migration error: {e}"))
    }

    /// Shared row fetch for `get`, `list` and `list_interrupted`.
    fn query_executions<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> Result<Vec<AgentExecution>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| format!("db prepare error: {e}"))?;
        let rows = stmt
            .query_map(params, |row| {
                Ok(ExecutionRow {
                    id: row.get(0)?,
                    conversation_id: row.get(1)?,
                    agent_qualified_id: row.get(2)?,
                    provider: row.get(3)?,
                    model: row.get(4)?,
                    status: row.get(5)?,
                    current_step: row.get(6)?,
                    user_message: row.get(7)?,
                    user_message_hash: row.get(8)?,
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
                    finished_at: row.get(11)?,
                    error: row.get(12)?,
                })
            })
            .map_err(|e| format!("db query error: {e}"))?;

        let mut out = Vec::new();
        for row in rows {
            out.push(decode_execution(
                row.map_err(|e| format!("db row read error: {e}"))?,
            )?);
        }
        Ok(out)
    }
}

#[async_trait]
impl ExecutionStore for SqliteExecutionStore {
    async fn create(&self, execution: &AgentExecution) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        // A structured prompt may carry credentials (e.g. a JSON payload);
        // redact them before the bytes reach disk. Plain text has nothing for
        // redact_secrets to act on and is preserved verbatim.
        let user_message = execution.user_message.as_deref().map(redact_message);
        conn.execute(
            "INSERT INTO executions (id, conversation_id, agent_qualified_id, provider, model, \
             status, current_step, user_message, user_message_hash, created_at, updated_at, \
             finished_at, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                execution.id.to_string(),
                execution.conversation_id.as_deref(),
                execution.agent_qualified_id,
                execution.provider,
                execution.model,
                to_json_text(&execution.status)?,
                execution.current_step,
                user_message,
                execution.user_message_hash,
                ts_text(&execution.created_at),
                ts_text(&execution.updated_at),
                execution.finished_at.as_ref().map(ts_text),
                execution.error.as_deref(),
            ],
        )
        .map_err(|e| format!("db insert execution error: {e}"))?;
        Ok(())
    }

    async fn append_event(&self, execution_id: Uuid, event: &ExecutionEvent) -> Result<(), String> {
        let kind = to_json_text(&event.kind)?;
        let payload = to_json_text(&redact_trace_payload(&event.payload))?;
        let created_at = ts_text(&Utc::now());
        let exec_id = execution_id.to_string();

        let mut conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        // seq assignment and insert share one transaction: concurrent appends
        // are serialised by the mutex, and a crash cannot leave a gap.
        let tx = conn
            .transaction()
            .map_err(|e| format!("db transaction error: {e}"))?;
        let seq: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM execution_events WHERE execution_id = ?1",
                params![exec_id],
                |row| row.get(0),
            )
            .map_err(|e| format!("db seq query error: {e}"))?;
        tx.execute(
            "INSERT INTO execution_events (id, execution_id, seq, kind, payload, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.id.to_string(),
                exec_id,
                seq,
                kind,
                payload,
                created_at
            ],
        )
        .map_err(|e| format!("db insert event error: {e}"))?;
        tx.commit().map_err(|e| format!("db commit error: {e}"))?;
        Ok(())
    }

    async fn set_status(
        &self,
        execution_id: Uuid,
        status: ExecutionStatus,
        error: Option<&str>,
    ) -> Result<(), String> {
        let now = Utc::now();
        let terminal = matches!(
            status,
            ExecutionStatus::Completed
                | ExecutionStatus::Failed
                | ExecutionStatus::Interrupted
                | ExecutionStatus::Cancelled
        );
        let finished_at = if terminal { Some(ts_text(&now)) } else { None };
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        conn.execute(
            "UPDATE executions SET status = ?1, updated_at = ?2, finished_at = ?3, error = ?4 \
             WHERE id = ?5",
            params![
                to_json_text(&status)?,
                ts_text(&now),
                finished_at,
                error,
                execution_id.to_string()
            ],
        )
        .map_err(|e| format!("db update status error: {e}"))?;
        Ok(())
    }

    async fn claim_for_retry(
        &self,
        execution_id: Uuid,
        from: &[ExecutionStatus],
        to: ExecutionStatus,
    ) -> Result<bool, String> {
        if from.is_empty() {
            return Ok(false);
        }
        // ONE conditional UPDATE is the whole claim: the status test and the
        // write commit together, so two concurrent retries cannot both win.
        // Statuses use the same JSON text `create`/`set_status` write, and the
        // reset of finished_at/error mirrors re-running the turn from scratch.
        let placeholders = (0..from.len())
            .map(|index| format!("?{}", index + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE executions SET status = ?1, updated_at = ?2, finished_at = NULL, error = NULL \
             WHERE id = ?{} AND status IN ({placeholders}) RETURNING id",
            from.len() + 3
        );
        let mut values = Vec::with_capacity(from.len() + 3);
        values.push(to_json_text(&to)?);
        values.push(ts_text(&Utc::now()));
        for status in from {
            values.push(to_json_text(status)?);
        }
        values.push(execution_id.to_string());

        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        match conn.query_row(
            &sql,
            params_from_iter(values.iter().map(String::as_str)),
            |row| row.get::<_, String>(0),
        ) {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(format!("db claim execution error: {e}")),
        }
    }

    async fn set_current_step(&self, execution_id: Uuid, step: u32) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        conn.execute(
            "UPDATE executions SET current_step = ?1, updated_at = ?2 WHERE id = ?3",
            params![step, ts_text(&Utc::now()), execution_id.to_string()],
        )
        .map_err(|e| format!("db update current_step error: {e}"))?;
        Ok(())
    }

    async fn get(&self, execution_id: Uuid) -> Result<Option<AgentExecution>, String> {
        let sql = format!("SELECT {EXECUTION_COLUMNS} FROM executions WHERE id = ?1");
        Ok(self
            .query_executions(&sql, params![execution_id.to_string()])?
            .into_iter()
            .next())
    }

    async fn list_interrupted(&self) -> Result<Vec<AgentExecution>, String> {
        let running = to_json_text(&ExecutionStatus::Running)?;
        let waiting = to_json_text(&ExecutionStatus::WaitingApproval)?;
        let sql = format!(
            "SELECT {EXECUTION_COLUMNS} FROM executions WHERE status IN (?1, ?2) \
             ORDER BY created_at ASC"
        );
        self.query_executions(&sql, params![running, waiting])
    }

    async fn list(&self, limit: usize) -> Result<Vec<AgentExecution>, String> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let sql =
            format!("SELECT {EXECUTION_COLUMNS} FROM executions ORDER BY created_at DESC LIMIT ?1");
        self.query_executions(&sql, params![limit as i64])
    }

    async fn get_events(&self, execution_id: Uuid) -> Result<Vec<ExecutionEvent>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, execution_id, seq, kind, payload, created_at FROM execution_events \
                 WHERE execution_id = ?1 ORDER BY seq ASC",
            )
            .map_err(|e| format!("db prepare error: {e}"))?;
        let rows = stmt
            .query_map(params![execution_id.to_string()], |row| {
                Ok(EventRow {
                    id: row.get(0)?,
                    execution_id: row.get(1)?,
                    seq: row.get(2)?,
                    kind: row.get(3)?,
                    payload: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })
            .map_err(|e| format!("db query error: {e}"))?;

        let mut out = Vec::new();
        for row in rows {
            out.push(decode_event(
                row.map_err(|e| format!("db row read error: {e}"))?,
            )?);
        }
        Ok(out)
    }

    async fn record_operation(
        &self,
        operation_id: &str,
        result_json: &Value,
    ) -> Result<(), String> {
        let result = to_json_text(result_json)?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        // PRIMARY KEY + OR IGNORE makes replay atomic: a retry after a crash
        // never overwrites the result the first call committed.
        conn.execute(
            "INSERT OR IGNORE INTO tool_operations (operation_id, result_json, created_at) \
             VALUES (?1, ?2, ?3)",
            params![operation_id, result, ts_text(&Utc::now())],
        )
        .map_err(|e| format!("db insert operation error: {e}"))?;
        Ok(())
    }

    async fn get_operation(&self, operation_id: &str) -> Result<Option<Value>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare("SELECT result_json FROM tool_operations WHERE operation_id = ?1")
            .map_err(|e| format!("db prepare error: {e}"))?;
        match stmt.query_row(params![operation_id], |row| row.get::<_, String>(0)) {
            Ok(text) => Ok(Some(from_json_text(&text)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("db query operation error: {e}")),
        }
    }

    async fn save_pending_approval(&self, approval: &PendingApprovalRecord) -> Result<(), String> {
        let args = to_json_text(&redact_secrets(&approval.args))?;
        let decision = approval.decision.as_ref().map(to_json_text).transpose()?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        // Keyed by id: a boot reload that rewrites the pending record and the
        // HTTP decision that follows it agree on the same row.
        conn.execute(
            "INSERT INTO pending_approvals (id, execution_id, tool_name, args, args_hash, \
             created_at, decided, decision, decided_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT(id) DO UPDATE SET execution_id = excluded.execution_id, \
             tool_name = excluded.tool_name, args = excluded.args, args_hash = excluded.args_hash, \
             created_at = excluded.created_at, decided = excluded.decided, \
             decision = excluded.decision, decided_at = excluded.decided_at",
            params![
                approval.id.to_string(),
                approval.execution_id.map(|id| id.to_string()),
                approval.tool_name,
                args,
                hash_hex(&approval.args_hash),
                ts_text(&approval.created_at),
                if approval.decided { 1 } else { 0 },
                decision,
                approval.decided_at.as_ref().map(ts_text),
            ],
        )
        .map_err(|e| format!("db save pending approval error: {e}"))?;
        Ok(())
    }

    async fn load_pending_approvals(&self) -> Result<Vec<PendingApprovalRecord>, String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, execution_id, tool_name, args, args_hash, created_at, decided, \
                 decision, decided_at FROM pending_approvals ORDER BY created_at ASC",
            )
            .map_err(|e| format!("db prepare error: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(PendingApprovalRow {
                    id: row.get(0)?,
                    execution_id: row.get(1)?,
                    tool_name: row.get(2)?,
                    args: row.get(3)?,
                    args_hash: row.get(4)?,
                    created_at: row.get(5)?,
                    decided: row.get(6)?,
                    decision: row.get(7)?,
                    decided_at: row.get(8)?,
                })
            })
            .map_err(|e| format!("db query error: {e}"))?;

        let mut out = Vec::new();
        for row in rows {
            out.push(decode_pending(
                row.map_err(|e| format!("db row read error: {e}"))?,
            )?);
        }
        Ok(out)
    }

    async fn decide_pending(
        &self,
        id: Uuid,
        decision: ApprovalDecisionRecord,
        decided_at: DateTime<Utc>,
    ) -> Result<(), String> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        let changed = conn
            .execute(
                "UPDATE pending_approvals SET decided = 1, decision = ?1, decided_at = ?2 \
                 WHERE id = ?3",
                params![
                    to_json_text(&decision)?,
                    ts_text(&decided_at),
                    id.to_string()
                ],
            )
            .map_err(|e| format!("db decide pending error: {e}"))?;
        if changed == 0 {
            return Err(format!("no pending approval with id {id}"));
        }
        Ok(())
    }
}

// ── Persistence helpers ─────────────────────────────────────────────────────

fn to_json_text<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|e| format!("serialize error: {e}"))
}

fn from_json_text<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    serde_json::from_str(text).map_err(|e| format!("deserialize error: {e}"))
}

fn ts_text(ts: &DateTime<Utc>) -> String {
    // Fixed 9-digit precision keeps RFC 3339 text sortable lexicographically
    // (AutoSi would drop the fraction for whole seconds, making `.500Z` sort
    // before `Z`) while still parsing back to an identical DateTime.
    ts.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn ts_parse(text: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| format!("invalid stored timestamp '{text}': {e}"))
}

fn parse_uuid(text: &str) -> Result<Uuid, String> {
    Uuid::parse_str(text).map_err(|e| format!("invalid stored uuid '{text}': {e}"))
}

fn u32_from(value: i64, field: &str) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| format!("stored {field} is out of range: {value}"))
}

fn hash_hex(hash: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in hash {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn hash_from_hex(text: &str) -> Result<[u8; 32], String> {
    if text.len() != 64 {
        return Err(format!(
            "invalid stored args_hash: expected 64 hex chars, got {}",
            text.len()
        ));
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|e| format!("invalid stored args_hash '{text}': {e}"))?;
    }
    Ok(out)
}

/// Redact a user message. Only structured JSON (object/array) has fields for
/// `redact_secrets` to inspect; plain prose is stored unchanged, otherwise a
/// prompt that happens to contain a quoted number would be mangled.
fn redact_message(message: &str) -> String {
    match serde_json::from_str::<Value>(message) {
        Ok(value @ (Value::Object(_) | Value::Array(_))) => {
            serde_json::to_string(&redact_secrets(&value)).unwrap_or_else(|_| message.to_string())
        }
        _ => message.to_string(),
    }
}

// ── Raw SQL row shapes (decoded after fetching to keep errors informative) ──

struct ExecutionRow {
    id: String,
    conversation_id: Option<String>,
    agent_qualified_id: String,
    provider: String,
    model: String,
    status: String,
    current_step: i64,
    user_message: Option<String>,
    user_message_hash: String,
    created_at: String,
    updated_at: String,
    finished_at: Option<String>,
    error: Option<String>,
}

fn decode_execution(row: ExecutionRow) -> Result<AgentExecution, String> {
    Ok(AgentExecution {
        id: parse_uuid(&row.id)?,
        conversation_id: row.conversation_id,
        agent_qualified_id: row.agent_qualified_id,
        provider: row.provider,
        model: row.model,
        status: from_json_text(&row.status)?,
        current_step: u32_from(row.current_step, "current_step")?,
        user_message: row.user_message,
        user_message_hash: row.user_message_hash,
        created_at: ts_parse(&row.created_at)?,
        updated_at: ts_parse(&row.updated_at)?,
        finished_at: row.finished_at.as_deref().map(ts_parse).transpose()?,
        error: row.error,
    })
}

struct EventRow {
    id: String,
    execution_id: String,
    seq: i64,
    kind: String,
    payload: String,
    created_at: String,
}

fn decode_event(row: EventRow) -> Result<ExecutionEvent, String> {
    Ok(ExecutionEvent {
        id: parse_uuid(&row.id)?,
        execution_id: parse_uuid(&row.execution_id)?,
        seq: u32_from(row.seq, "seq")?,
        kind: from_json_text(&row.kind)?,
        payload: from_json_text(&row.payload)?,
        created_at: ts_parse(&row.created_at)?,
    })
}

struct PendingApprovalRow {
    id: String,
    execution_id: Option<String>,
    tool_name: String,
    args: String,
    args_hash: String,
    created_at: String,
    decided: i64,
    decision: Option<String>,
    decided_at: Option<String>,
}

fn decode_pending(row: PendingApprovalRow) -> Result<PendingApprovalRecord, String> {
    Ok(PendingApprovalRecord {
        id: parse_uuid(&row.id)?,
        execution_id: row.execution_id.as_deref().map(parse_uuid).transpose()?,
        tool_name: row.tool_name,
        args: from_json_text(&row.args)?,
        args_hash: hash_from_hex(&row.args_hash)?,
        created_at: ts_parse(&row.created_at)?,
        decided: row.decided != 0,
        decision: row.decision.as_deref().map(from_json_text).transpose()?,
        decided_at: row.decided_at.as_deref().map(ts_parse).transpose()?,
    })
}
