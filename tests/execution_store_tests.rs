//! Contract tests for the Fase 7 execution domain types and `ExecutionStore`
//! port, plus adapter-backed CRUD/seq/idempotency/approval tests against
//! `SqliteExecutionStore` (the latter gated on the `history` feature, which is
//! what pulls in `rusqlite`).
//!
//! The contract-only tests at the top run under every feature combination:
//! the types round trip through serde, event construction needs no boilerplate,
//! and the port is usable as `Arc<dyn ExecutionStore>` (the form `AppState`
//! will hold). The adapter tests below prove the durable behaviour the runtime
//! will rely on after restart.

use chrono::Utc;
use llama_r::domain::execution::{AgentExecution, EventKind, ExecutionEvent, ExecutionStatus};
use llama_r::ports::execution::{ApprovalDecisionRecord, ExecutionStore, PendingApprovalRecord};
use serde_json::json;
use uuid::Uuid;

fn sample_execution() -> AgentExecution {
    let now = Utc::now();
    AgentExecution {
        id: Uuid::new_v4(),
        conversation_id: Some("conv-1".into()),
        agent_qualified_id: "project::agent".into(),
        provider: "ollama".into(),
        model: "llama3".into(),
        status: ExecutionStatus::Running,
        current_step: 2,
        user_message: Some("show me the {table}".into()),
        user_message_hash: "ab".repeat(32),
        created_at: now,
        updated_at: now,
        finished_at: None,
        error: None,
    }
}

#[test]
fn agent_execution_survives_a_serde_round_trip() {
    let execution = sample_execution();
    let encoded = serde_json::to_string(&execution).unwrap();
    let decoded: AgentExecution = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, execution);
    assert!(encoded.contains("\"user_message\""));
    assert!(encoded.contains("\"user_message_hash\""));
}

#[test]
fn every_execution_status_survives_a_serde_round_trip() {
    let all = [
        ExecutionStatus::Pending,
        ExecutionStatus::Running,
        ExecutionStatus::WaitingApproval,
        ExecutionStatus::Completed,
        ExecutionStatus::Failed,
        ExecutionStatus::Interrupted,
        ExecutionStatus::Cancelled,
    ];
    for status in all {
        let encoded = serde_json::to_string(&status).unwrap();
        let decoded: ExecutionStatus = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, status, "status {encoded} did not round trip");
    }
}

#[test]
fn an_event_can_be_created_without_picking_an_id_or_timestamp() {
    let execution_id = Uuid::new_v4();
    let event = ExecutionEvent::new(
        execution_id,
        EventKind::ToolCallCompleted,
        json!({"tool": "llama_r__skill", "replayed": true}),
    );

    assert_ne!(event.id, Uuid::nil());
    assert_eq!(event.execution_id, execution_id);
    assert_eq!(event.seq, 0, "seq is assigned by the store");
    assert_eq!(event.kind, EventKind::ToolCallCompleted);
    assert_eq!(event.payload["replayed"], true);
    assert!(event.created_at <= Utc::now());
}

#[test]
fn an_execution_event_survives_a_serde_round_trip() {
    let execution_id = Uuid::new_v4();
    let event = ExecutionEvent::new(execution_id, EventKind::Started, json!({"step": 1}));
    let encoded = serde_json::to_string(&event).unwrap();
    let decoded: ExecutionEvent = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, event);
}

#[test]
fn every_event_kind_survives_a_serde_round_trip() {
    let all = [
        EventKind::Started,
        EventKind::ModelRequested,
        EventKind::ToolCallRequested,
        EventKind::ToolCallCompleted,
        EventKind::CheckpointCreated,
        EventKind::WaitingApproval,
        EventKind::ApprovalGranted,
        EventKind::ExecutionResumed,
        EventKind::ExecutionCompleted,
        EventKind::ExecutionFailed,
        EventKind::Interrupted,
    ];
    for kind in all {
        let encoded = serde_json::to_string(&kind).unwrap();
        let decoded: EventKind = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, kind, "kind {encoded} did not round trip");
    }
}

#[test]
fn a_persisted_pending_approval_survives_a_serde_round_trip() {
    let record = PendingApprovalRecord {
        id: Uuid::new_v4(),
        execution_id: Some(Uuid::new_v4()),
        tool_name: "mail__send".into(),
        args: json!({"to": "a@example.com", "api_key": "[REDACTED]"}),
        args_hash: [0xab; 32],
        created_at: Utc::now(),
        decided: true,
        decision: Some(ApprovalDecisionRecord::Denied {
            reason: "denied by human".into(),
        }),
        decided_at: Some(Utc::now()),
    };

    let encoded = serde_json::to_string(&record).unwrap();
    let decoded: PendingApprovalRecord = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn the_store_port_is_usable_as_a_thread_safe_trait_object() {
    // `AppState` holds `Arc<dyn ExecutionStore>` across every server task; this
    // test is the compile-time proof that the async trait stays object safe.
    fn accepts(_store: &dyn ExecutionStore) {}
    fn accepts_shared(_store: std::sync::Arc<dyn ExecutionStore>) {}
    let _ = (accepts, accepts_shared);
}

/// The no-op substitute is compiled under **every** feature combination: it is
/// both the `--no-default-features` store and the degradation fallback when the
/// SQLite store cannot be opened. Every read is empty, every write is a
/// successful no-op — a caller never has to branch on which store it holds.
#[tokio::test]
async fn a_no_op_store_is_always_constructible_and_harmless() {
    use llama_r::adapters::execution::NoOpExecutionStore;

    let store = NoOpExecutionStore;
    // Prove it satisfies the port as a trait object too — this is the exact
    // shape `AppState.execution_store` holds in a `--no-default-features` build.
    let shared: std::sync::Arc<dyn ExecutionStore> = std::sync::Arc::new(store);

    let execution = sample_execution();
    shared.create(&execution).await.unwrap();
    let event = ExecutionEvent::new(execution.id, EventKind::Started, json!({"step": 1}));
    shared.append_event(execution.id, &event).await.unwrap();
    shared
        .set_status(execution.id, ExecutionStatus::Completed, None)
        .await
        .unwrap();
    assert!(shared.get(execution.id).await.unwrap().is_none());
    assert!(shared.list_interrupted().await.unwrap().is_empty());
    assert!(shared.list(10).await.unwrap().is_empty());
    assert!(shared.get_events(execution.id).await.unwrap().is_empty());

    shared
        .record_operation("op-1", &json!({"result": "ok"}))
        .await
        .unwrap();
    assert!(shared.get_operation("op-1").await.unwrap().is_none());

    let record = PendingApprovalRecord {
        id: Uuid::new_v4(),
        execution_id: Some(execution.id),
        tool_name: "mail__send".into(),
        args: json!({"to": "a@example.com"}),
        args_hash: [0xab; 32],
        created_at: Utc::now(),
        decided: false,
        decision: None,
        decided_at: None,
    };
    shared.save_pending_approval(&record).await.unwrap();
    assert!(shared.load_pending_approvals().await.unwrap().is_empty());
    shared
        .decide_pending(record.id, ApprovalDecisionRecord::Approved, Utc::now())
        .await
        .unwrap();
}

/// Boot recovery: a turn left mid-flight by a crash is marked `Interrupted`
/// once at boot. Gated on `history` only because seeding real rows needs the
/// SQLite adapter — `mark_interrupted` itself is feature-agnostic.
#[cfg(feature = "history")]
mod boot_recovery {
    use super::*;
    use llama_r::adapters::execution::SqliteExecutionStore;
    use llama_r::runtime::mark_interrupted;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn store_at(path: impl AsRef<std::path::Path>) -> Arc<SqliteExecutionStore> {
        Arc::new(SqliteExecutionStore::new(path).expect("open execution store"))
    }

    #[tokio::test]
    async fn boot_marks_running_and_waiting_as_interrupted() {
        let dir = tempdir().unwrap();
        let store = store_at(dir.path().join("history.db"));

        let mut running = sample_execution();
        running.status = ExecutionStatus::Running;
        let mut waiting = sample_execution();
        waiting.status = ExecutionStatus::WaitingApproval;
        store.create(&running).await.unwrap();
        store.create(&waiting).await.unwrap();

        let count = mark_interrupted(store.as_ref()).await.unwrap();
        assert_eq!(count, 2, "exactly the two mid-flight turns are marked");
        assert_eq!(
            store.get(running.id).await.unwrap().unwrap().status,
            ExecutionStatus::Interrupted
        );
        assert_eq!(
            store.get(waiting.id).await.unwrap().unwrap().status,
            ExecutionStatus::Interrupted
        );
        assert!(
            store.list_interrupted().await.unwrap().is_empty(),
            "no mid-flight turns remain after boot recovery"
        );
    }

    #[tokio::test]
    async fn a_completed_execution_is_untouched() {
        let dir = tempdir().unwrap();
        let store = store_at(dir.path().join("history.db"));

        let mut completed = sample_execution();
        completed.status = ExecutionStatus::Completed;
        completed.finished_at = Some(Utc::now());
        store.create(&completed).await.unwrap();

        let count = mark_interrupted(store.as_ref()).await.unwrap();
        assert_eq!(count, 0, "a finished turn is not recovery work");
        let loaded = store.get(completed.id).await.unwrap().unwrap();
        assert_eq!(loaded.status, ExecutionStatus::Completed);
        assert_eq!(loaded.finished_at, completed.finished_at);
    }
}

/// Adapter-backed contract. `rusqlite` is pulled in by the `history` feature,
/// so the whole module is skipped under `--no-default-features` where the store
/// adapter is not built.
#[cfg(feature = "history")]
mod sqlite_adapter {
    use super::*;
    use llama_r::adapters::execution::SqliteExecutionStore;
    use llama_r::services::redaction::REDACTED_MARKER;
    use rusqlite::params;
    use tempfile::tempdir;

    fn store_at(path: impl AsRef<std::path::Path>) -> SqliteExecutionStore {
        SqliteExecutionStore::new(path).expect("open execution store")
    }

    #[tokio::test]
    async fn an_old_history_db_opens_with_the_new_tables() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.db");
        {
            // Simulate a pre-Fase-7 history.db: only the legacy tables, with a
            // row in each. The execution schema must be purely additive.
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE conversations (
                     id TEXT PRIMARY KEY,
                     agent_qualified_id TEXT NOT NULL,
                     project_id TEXT,
                     agent_id TEXT NOT NULL,
                     turn_count INTEGER NOT NULL DEFAULT 0,
                     created_at INTEGER NOT NULL,
                     updated_at INTEGER NOT NULL
                 );
                 CREATE TABLE messages (
                     id TEXT PRIMARY KEY,
                     conversation_id TEXT NOT NULL,
                     role TEXT NOT NULL,
                     content TEXT NOT NULL,
                     created_at INTEGER NOT NULL
                 );
                 INSERT INTO conversations VALUES ('legacy', 'agent', NULL, 'agent', 0, 1, 1);
                 INSERT INTO messages VALUES ('m1', 'legacy', 'user', 'old message', 1);",
            )
            .unwrap();
        }

        // Opening the execution store must not fail on the legacy file.
        let _store = store_at(&path);

        let conn = rusqlite::Connection::open(&path).unwrap();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for table in [
            "conversations",
            "messages",
            "executions",
            "execution_events",
            "tool_operations",
            "pending_approvals",
        ] {
            assert!(names.contains(&table.to_string()), "missing table {table}");
        }
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' ORDER BY name")
            .unwrap();
        let indexes: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            indexes
                .iter()
                .any(|name| name == "idx_events_execution_seq"),
            "missing execution events index"
        );
        let legacy_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(legacy_count, 1, "legacy rows must survive the migration");
    }

    #[tokio::test]
    async fn execution_crud_and_status() {
        let dir = tempdir().unwrap();
        let store = store_at(dir.path().join("history.db"));

        let execution = sample_execution();
        store.create(&execution).await.unwrap();
        assert_eq!(
            store.get(execution.id).await.unwrap(),
            Some(execution.clone()),
            "a freshly created execution round trips verbatim"
        );

        store
            .set_status(execution.id, ExecutionStatus::Completed, None)
            .await
            .unwrap();
        let done = store.get(execution.id).await.unwrap().unwrap();
        assert_eq!(done.status, ExecutionStatus::Completed);
        assert!(
            done.finished_at.is_some(),
            "terminal status sets finished_at"
        );

        store
            .set_status(execution.id, ExecutionStatus::Failed, Some("boom"))
            .await
            .unwrap();
        let failed = store.get(execution.id).await.unwrap().unwrap();
        assert_eq!(failed.status, ExecutionStatus::Failed);
        assert_eq!(
            failed.error.as_deref(),
            Some("boom"),
            "error stored verbatim"
        );

        assert!(store.get(Uuid::new_v4()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn events_have_monotonic_seq_per_execution() {
        let dir = tempdir().unwrap();
        let store = store_at(dir.path().join("history.db"));

        let a = sample_execution();
        let b = sample_execution();
        store.create(&a).await.unwrap();
        store.create(&b).await.unwrap();

        let first = ExecutionEvent::new(a.id, EventKind::Started, json!({"step": 1}));
        let mut second =
            ExecutionEvent::new(a.id, EventKind::CheckpointCreated, json!({"step": 2}));
        second.seq = 999; // placeholder: the store must ignore it
        let other = ExecutionEvent::new(b.id, EventKind::Started, json!({}));

        store.append_event(a.id, &first).await.unwrap();
        store.append_event(a.id, &second).await.unwrap();
        store.append_event(b.id, &other).await.unwrap();

        let events = store.get_events(a.id).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 1);
        assert_eq!(
            events[1].seq, 2,
            "seq grows regardless of the caller placeholder"
        );
        assert_eq!(events[0].id, first.id);
        assert_eq!(events[1].id, second.id);
        assert!(events[0].created_at <= events[1].created_at);

        let events_b = store.get_events(b.id).await.unwrap();
        assert_eq!(events_b.len(), 1);
        assert_eq!(events_b[0].seq, 1, "seq is monotonic per execution");
        assert!(store.get_events(Uuid::new_v4()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_interrupted_returns_only_running_and_waiting() {
        let dir = tempdir().unwrap();
        let store = store_at(dir.path().join("history.db"));

        let mut running = sample_execution();
        running.status = ExecutionStatus::Running;
        let mut waiting = sample_execution();
        waiting.status = ExecutionStatus::WaitingApproval;
        let mut completed = sample_execution();
        completed.status = ExecutionStatus::Completed;
        let mut failed = sample_execution();
        failed.status = ExecutionStatus::Failed;
        for execution in [&running, &waiting, &completed, &failed] {
            store.create(execution).await.unwrap();
        }

        let ids: Vec<Uuid> = store
            .list_interrupted()
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&running.id));
        assert!(ids.contains(&waiting.id));
        assert!(!ids.contains(&completed.id));
        assert!(!ids.contains(&failed.id));
    }

    #[tokio::test]
    async fn list_is_capped_and_newest_first() {
        let dir = tempdir().unwrap();
        let store = store_at(dir.path().join("history.db"));

        // Fixed-precision timestamps must sort correctly even when one lands
        // exactly on a whole second (no fractional part) and the others are a
        // few nanoseconds later — a variable-width RFC 3339 string would invert
        // them lexicographically.
        let base = Utc::now() - chrono::Duration::hours(2);
        let base_secs = base.timestamp();
        let whole_second = chrono::DateTime::from_timestamp(base_secs, 0).unwrap();
        let mut old = sample_execution();
        old.created_at = whole_second;
        let mut mid = sample_execution();
        mid.created_at = whole_second + chrono::Duration::nanoseconds(1);
        let mut new = sample_execution();
        new.created_at = whole_second + chrono::Duration::nanoseconds(2);
        for execution in [&old, &mid, &new] {
            store.create(execution).await.unwrap();
        }

        assert!(store.list(0).await.unwrap().is_empty(), "limit 0 is empty");
        let page = store.list(2).await.unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, new.id, "most recent first");
        assert_eq!(page[1].id, mid.id);
        assert_eq!(store.list(10).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn an_operation_id_is_idempotent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.db");
        let store = store_at(&path);

        assert!(store.get_operation("op-1").await.unwrap().is_none());
        store
            .record_operation("op-1", &json!({"result": "first"}))
            .await
            .unwrap();
        store
            .record_operation("op-1", &json!({"result": "second"}))
            .await
            .unwrap();

        assert_eq!(
            store.get_operation("op-1").await.unwrap(),
            Some(json!({"result": "first"})),
            "the first result wins and a replay never re-executes"
        );
        let conn = rusqlite::Connection::open(&path).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tool_operations WHERE operation_id = 'op-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "operation_id is a primary key");
    }

    #[tokio::test]
    async fn pending_approval_round_trips_and_decides() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.db");
        let store = store_at(&path);

        let execution = sample_execution();
        store.create(&execution).await.unwrap();
        let record = PendingApprovalRecord {
            id: Uuid::new_v4(),
            execution_id: Some(execution.id),
            tool_name: "mail__send".into(),
            args: json!({"to": "a@example.com", "api_key": "sk-approval-secret"}),
            args_hash: [0xab; 32],
            created_at: Utc::now(),
            decided: false,
            decision: None,
            decided_at: None,
        };
        store.save_pending_approval(&record).await.unwrap();

        let loaded = store.load_pending_approvals().await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, record.id);
        assert_eq!(loaded[0].execution_id, Some(execution.id));
        assert_eq!(loaded[0].tool_name, "mail__send");
        assert_eq!(loaded[0].args["to"], "a@example.com");
        assert_eq!(loaded[0].args["api_key"], REDACTED_MARKER, "args redacted");
        assert_eq!(loaded[0].args_hash, [0xab; 32]);
        assert!(!loaded[0].decided);

        let conn = rusqlite::Connection::open(&path).unwrap();
        let args_on_disk: String = conn
            .query_row(
                "SELECT args FROM pending_approvals WHERE id = ?1",
                params![record.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !args_on_disk.contains("sk-approval-secret"),
            "the secret must not be on disk"
        );

        let decided_at = Utc::now();
        store
            .decide_pending(record.id, ApprovalDecisionRecord::Approved, decided_at)
            .await
            .unwrap();
        let loaded = store.load_pending_approvals().await.unwrap();
        assert_eq!(loaded.len(), 1, "decide updates in place, no second row");
        assert!(loaded[0].decided);
        assert_eq!(loaded[0].decision, Some(ApprovalDecisionRecord::Approved));
        assert_eq!(loaded[0].decided_at, Some(decided_at));

        // Boot reload of the same id is an upsert, not a duplicate.
        store.save_pending_approval(&record).await.unwrap();
        assert_eq!(store.load_pending_approvals().await.unwrap().len(), 1);

        assert!(
            store
                .decide_pending(Uuid::new_v4(), ApprovalDecisionRecord::Approved, Utc::now())
                .await
                .is_err(),
            "deciding an unknown id must error, not silently vanish"
        );
    }

    #[tokio::test]
    async fn a_secret_in_an_event_payload_never_reaches_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("history.db");
        let store = store_at(&path);

        let execution = sample_execution();
        store.create(&execution).await.unwrap();
        let secret = "sk-LEAKME-9f8e7d";
        let event = ExecutionEvent::new(
            execution.id,
            EventKind::ToolCallRequested,
            json!({
                "tool": "db__query",
                "api_key": secret,
                "nested": {"Authorization": format!("Bearer {secret}"), "safe": "ok"}
            }),
        );
        store.append_event(execution.id, &event).await.unwrap();

        let events = store.get_events(execution.id).await.unwrap();
        assert_eq!(events.len(), 1);
        let encoded = serde_json::to_string(&events[0].payload).unwrap();
        assert!(
            !encoded.contains(secret),
            "secret must be redacted in memory"
        );
        assert!(encoded.contains(REDACTED_MARKER));
        assert_eq!(
            events[0].payload["nested"]["safe"], "ok",
            "other fields survive"
        );

        let conn = rusqlite::Connection::open(&path).unwrap();
        let payload: String = conn
            .query_row(
                "SELECT payload FROM execution_events WHERE id = ?1",
                params![event.id.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!payload.contains(secret), "secret must not be on disk");
        assert!(payload.contains(REDACTED_MARKER));
    }

    #[tokio::test]
    async fn a_structured_user_message_is_redacted_on_insert() {
        let dir = tempdir().unwrap();
        let store = store_at(dir.path().join("history.db"));

        let mut execution = sample_execution();
        execution.user_message = Some(
            json!({"prompt": "show me the table", "api_key": "sk-user-message-secret"}).to_string(),
        );
        store.create(&execution).await.unwrap();

        let loaded = store.get(execution.id).await.unwrap().unwrap();
        let message = loaded.user_message.unwrap();
        assert!(!message.contains("sk-user-message-secret"));
        assert!(message.contains(REDACTED_MARKER));
        assert!(message.contains("show me the table"));
        assert_eq!(loaded.user_message_hash, execution.user_message_hash);
    }
}
