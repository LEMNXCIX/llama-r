//! Human-in-the-loop approval gate.
//!
//! The gate only *registers* and *resolves* pending approvals. It does not know
//! about tools, bridges or HTTP — the caller emits an `ApprovalRequired` engine
//! event and awaits the oneshot returned by [`ApprovalGate::request`]. Wiring
//! the gate into a tool call is a separate concern.
//!
//! Two invariants matter:
//! - a decision is bound to the **exact arguments** that were shown to the
//!   human, via a SHA-256 hash of the canonical JSON; a changed hash is
//!   rejected rather than executing something the human never saw;
//! - `decide` takes the pending entry under the write lock, so two concurrent
//!   decisions can never resolve the same oneshot.
//!
//! # Durability (Fase 7)
//!
//! The oneshot is process-local, so a restart loses it. The gate is therefore a
//! *hybrid*: it also mirrors every pending and decision into the durable
//! [`ExecutionStore`](crate::ports::execution::ExecutionStore) when one is
//! attached (`.with_store(...)`). At boot [`ApprovalGate::load_from_store`]
//! rehydrates the pendings **without** a oneshot — there is no live request to
//! unblock after a restart. A human decision on such an entry is persisted and
//! recorded in memory, and [`ApprovalGate::decision_for`] exposes it to the
//! retry path, which consumes it before re-running the tool. The store's
//! futures are bounded synchronous work (`Mutex<Connection>` + rusqlite, or a
//! no-op), so the synchronous gate API can drive them with `block_on` without
//! ever parking on a tokio primitive.

use crate::error::AppError;
// The canonical argument hash lives in `services::redaction` so approval
// binding and tool-call `operation_id` mint the same id from the same args.
// Re-exported here: existing callers import it from this module.
use crate::ports::execution::{ApprovalDecisionRecord, ExecutionStore, PendingApprovalRecord};
pub use crate::services::redaction::{args_hash, args_hash_hex};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::oneshot;
use uuid::Uuid;

/// How long a pending approval stays valid before a reaper drops it.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// The human's answer, delivered over the oneshot from [`ApprovalGate::request`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Proceed with the exact arguments that were hashed at request time.
    Approved,
    /// Do not proceed. The reason is safe to surface as a named tool error.
    Denied { reason: String },
}

/// One pending human decision.
///
/// The one-way hash is what binds a decision to what the human saw. The raw
/// arguments are also kept so the approval-list endpoint can show *what* is
/// being approved to a non-streaming client that has no SSE event to read;
/// that endpoint redacts secret-bearing fields before serializing them. The
/// SSE stream never carries the raw arguments (only the hash), so the stream
/// cannot leak them either.
#[derive(Debug)]
pub struct PendingApproval {
    pub tool_name: String,
    pub args: Value,
    pub args_hash: [u8; 32],
    /// The turn that requested this approval. Set on live requests that carry a
    /// durable `execution_id`, rehydrated from the store after a restart. It is
    /// what scopes a persisted decision to a retry of *that* turn — without it
    /// an approval from one turn could silently re-approve an unrelated one.
    pub execution_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// Set once the decision was recorded. A decided entry is kept until reaped
    /// so a second decision can be told apart from an unknown id (409 vs 404).
    pub decided: bool,
    /// The recorded answer, if any. Rehydrated entries carry the decision read
    /// from the store; live entries get it when `decide` runs. `decision_for`
    /// exposes it to the retry path after a restart.
    pub decision: Option<ApprovalDecision>,
    /// `Some` for a live request (the oneshot awaited by the bridge); `None`
    /// for a rehydrated entry with no live request after a restart.
    tx: Option<oneshot::Sender<ApprovalDecision>>,
}

/// A read-only snapshot of a pending approval for the HTTP list endpoint. The
/// gate never hands out its internal sender; callers get one of these instead.
#[derive(Debug, Clone)]
pub struct PendingApprovalView {
    pub id: Uuid,
    pub tool_name: String,
    pub args: Value,
    pub created_at: DateTime<Utc>,
}

/// Registry of pending approvals, with a reaper that runs on `request` and
/// `decide` (there is no background task to schedule or shut down).
pub struct ApprovalGate {
    pending: RwLock<HashMap<Uuid, PendingApproval>>,
    timeout: Duration,
    /// Durable mirror, when one was installed. `None` keeps the primitive
    /// usable in tests and in degraded/no-store deployments.
    store: Option<Arc<dyn ExecutionStore>>,
}

impl std::fmt::Debug for ApprovalGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The store is a trait object without `Debug`; report only whether one
        // is attached, never its contents.
        f.debug_struct("ApprovalGate")
            .field("pending", &self.pending)
            .field("timeout", &self.timeout)
            .field("durable", &self.store.is_some())
            .finish()
    }
}

/// Why a decision could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    #[error("approval request {0} not found")]
    NotFound(Uuid),
    #[error("approval request {0} has expired")]
    Expired(Uuid),
    #[error("approval request {0} was already decided")]
    AlreadyDecided(Uuid),
    #[error("approval arguments changed since the request was made")]
    ArgsChanged,
}

impl From<ApprovalError> for AppError {
    fn from(err: ApprovalError) -> Self {
        match err {
            ApprovalError::NotFound(id) => AppError::ApprovalNotFound(id.to_string()),
            ApprovalError::Expired(id) => AppError::ApprovalExpired(id.to_string()),
            ApprovalError::AlreadyDecided(id) => AppError::ApprovalAlreadyDecided(id.to_string()),
            ApprovalError::ArgsChanged => AppError::ApprovalArgsChanged("arguments changed".into()),
        }
    }
}

impl Default for ApprovalGate {
    fn default() -> Self {
        Self::new(DEFAULT_APPROVAL_TIMEOUT)
    }
}

impl ApprovalGate {
    pub fn new(timeout: Duration) -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            timeout,
            store: None,
        }
    }

    /// Attach the durable store used to mirror pendings and decisions. Boot
    /// installs the same `Arc` that `AppState` owns; callers without a store
    /// (tests, degraded/no-history builds) keep the pure in-memory behaviour.
    pub fn with_store(mut self, store: Arc<dyn ExecutionStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// How long a pending decision stays valid. The bridge wraps its `rx.await`
    /// in a `tokio::time::timeout` of this length so a request nobody decides
    /// fails with a named error instead of hanging forever (the gate's own
    /// reaper only runs when `request`/`decide` is next called).
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Register a pending approval with no execution scope (live request outside
    /// the durable engine, e.g. a unit test or the TUI). Kept as the unchanged
    /// entry point so existing callers keep the same behaviour.
    pub fn request(
        &self,
        tool_name: impl Into<String>,
        args: &Value,
    ) -> (Uuid, oneshot::Receiver<ApprovalDecision>) {
        self.request_for(None, tool_name, args)
    }

    /// Register a pending approval for one turn. Reaps expired entries first,
    /// then returns a fresh id and the receiver the caller awaits before running
    /// the tool. When a durable store is attached the pending (including its
    /// execution id) is persisted, so it survives a restart; a store failure is
    /// warned about and never fails the request (durability is best effort).
    ///
    /// When `execution_id` is `Some` and a decision for this exact
    /// `(execution, tool, args)` was already recorded — a human decided after a
    /// crash, while no live oneshot existed —
    /// the persisted decision is consumed and returned through a resolved
    /// receiver instead of asking again. This is the retry path's approval seam;
    /// the binding is still the SHA-256 of the exact arguments, and the scope is
    /// the execution id, so an approval can never leak into a different turn.
    pub fn request_for(
        &self,
        execution_id: Option<Uuid>,
        tool_name: impl Into<String>,
        args: &Value,
    ) -> (Uuid, oneshot::Receiver<ApprovalDecision>) {
        self.prune_expired();
        let tool_name = tool_name.into();
        let args_hash = args_hash(args);
        let created_at = Utc::now();

        // Read candidate ids under one lock, then resolve them through the
        // public accessor. Two lock acquisitions rather than a recursive read
        // (which can deadlock on some platforms with a waiting writer).
        if let Some(execution_id) = execution_id {
            let reusable: Vec<Uuid> = {
                let pending = self.pending.read().expect("approval gate lock poisoned");
                pending
                    .iter()
                    .filter(|(_, entry)| {
                        entry.execution_id == Some(execution_id)
                            && entry.tool_name == tool_name
                            && entry.args_hash == args_hash
                            && entry.decided
                    })
                    .map(|(id, _)| *id)
                    .collect()
            };
            if let Some(id) = reusable.into_iter().next() {
                if let Some(decision) = self.decision_for(id) {
                    // A pre-resolved receiver: the bridge proceeds immediately,
                    // so the retry does not re-ask a human who already answered.
                    let (tx, rx) = oneshot::channel();
                    let _ = tx.send(decision);
                    return (id, rx);
                }
            }
        }

        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        let entry = PendingApproval {
            tool_name: tool_name.clone(),
            args: args.clone(),
            args_hash,
            execution_id,
            created_at,
            decided: false,
            decision: None,
            tx: Some(tx),
        };
        self.pending
            .write()
            .expect("approval gate lock poisoned")
            .insert(id, entry);

        if let Some(store) = &self.store {
            // `args` is forwarded verbatim: the store redacts secret-bearing
            // fields before persistence (one shared implementation, Task 1/2).
            let record = PendingApprovalRecord {
                id,
                execution_id,
                tool_name,
                args: args.clone(),
                args_hash,
                created_at,
                decided: false,
                decision: None,
                decided_at: None,
            };
            if let Err(err) = futures::executor::block_on(store.save_pending_approval(&record)) {
                tracing::warn!(
                    error = %err,
                    approval_id = %id,
                    "Failed to persist pending approval; continuing without durability"
                );
            }
        }
        (id, rx)
    }

    /// Snapshot every *undecided* pending approval, oldest first, for the HTTP
    /// list endpoint. Reaping runs here too, so an expired request is never
    /// offered to a human as something they can still act on. The raw arguments
    /// are included; the API layer redacts secret-bearing fields before they
    /// leave the process.
    pub fn list_pending(&self) -> Vec<PendingApprovalView> {
        self.prune_expired();
        let pending = self.pending.read().expect("approval gate lock poisoned");
        let mut views: Vec<_> = pending
            .iter()
            .filter(|(_, entry)| !entry.decided)
            .map(|(id, entry)| PendingApprovalView {
                id: *id,
                tool_name: entry.tool_name.clone(),
                args: entry.args.clone(),
                created_at: entry.created_at,
            })
            .collect();
        views.sort_by_key(|view| view.created_at);
        views
    }

    /// The hash a pending entry was registered with. Used by the HTTP endpoint
    /// when a caller omits `args_hash`, so its decision is still bound to the
    /// exact arguments on file rather than becoming an unchecked approval.
    /// Returns `None` for an unknown id; a decided entry still returns its
    /// hash so the subsequent `decide` can report `AlreadyDecided` (409).
    pub fn stored_hash(&self, id: Uuid) -> Option<[u8; 32]> {
        self.pending
            .read()
            .expect("approval gate lock poisoned")
            .get(&id)
            .map(|entry| entry.args_hash)
    }

    /// Resolve a pending approval.
    ///
    /// The whole check-and-take runs under one write lock: two simultaneous
    /// decisions cannot both find the entry, so the oneshot is sent at most
    /// once. A hash mismatch leaves the pending intact so a human can retry
    /// against the original arguments; an expired one is removed and rejected.
    pub fn decide(
        &self,
        id: Uuid,
        approve: bool,
        submitted_hash: [u8; 32],
    ) -> Result<(), ApprovalError> {
        let mut pending = self.pending.write().expect("approval gate lock poisoned");
        let now = Utc::now();

        if pending
            .get(&id)
            .map(|entry| is_expired(entry, now, self.timeout))
            .unwrap_or(false)
        {
            pending.remove(&id);
            return Err(ApprovalError::Expired(id));
        }
        // Reap the rest while the write lock is already held. Decided entries
        // are kept: a repeated decision is a conflict, not a miss, and the
        // retry path needs the recorded decision to stay observable.
        pending.retain(|_, entry| entry.decided || !is_expired(entry, now, self.timeout));

        let entry = pending.get_mut(&id).ok_or(ApprovalError::NotFound(id))?;
        if entry.decided {
            // Kept on purpose: a repeated decision is a conflict, not a miss.
            return Err(ApprovalError::AlreadyDecided(id));
        }
        if entry.args_hash != submitted_hash {
            return Err(ApprovalError::ArgsChanged);
        }
        // Mark decided and record the answer. Take the sender (if any) so the
        // oneshot fires at most once; a rehydrated entry has no sender because
        // there is no live request after a restart.
        entry.decided = true;
        let decision = if approve {
            ApprovalDecision::Approved
        } else {
            ApprovalDecision::Denied {
                reason: "denied by human".to_string(),
            }
        };
        entry.decision = Some(decision.clone());
        if let Some(tx) = entry.tx.take() {
            // The receiver may have been dropped (client disconnected); that
            // must not turn a valid decision into an error.
            let _ = tx.send(decision.clone());
        }
        // Persist under the SAME lock that fired the oneshot: the durable record
        // and the in-memory resolution cannot diverge for concurrent decisions.
        if let Some(store) = &self.store {
            if let Err(err) = futures::executor::block_on(store.decide_pending(
                id,
                decision_record(&decision),
                now,
            )) {
                tracing::warn!(
                    error = %err,
                    approval_id = %id,
                    "Failed to persist approval decision; continuing"
                );
            }
        }
        Ok(())
    }

    /// Drop every *undecided* pending approval older than the configured
    /// timeout. Decided entries are retained so their decision stays visible
    /// to `decision_for` and the retry path.
    pub fn prune_expired(&self) {
        let now = Utc::now();
        let mut pending = self.pending.write().expect("approval gate lock poisoned");
        pending.retain(|_, entry| entry.decided || !is_expired(entry, now, self.timeout));
    }

    /// Rehydrate pendings from the attached store at boot. Entries loaded this
    /// way have **no oneshot**: there is no live bridge to unblock after a
    /// restart, so a decision made through HTTP is persisted and exposed via
    /// [`ApprovalGate::decision_for`] to the retry path instead. Returns the
    /// number of entries loaded (0 when there is no store).
    pub async fn load_from_store(&self) -> Result<usize, String> {
        let Some(store) = &self.store else {
            return Ok(0);
        };
        let records = store.load_pending_approvals().await?;
        let mut pending = self.pending.write().expect("approval gate lock poisoned");
        let mut loaded = 0usize;
        for record in records {
            let entry = PendingApproval {
                tool_name: record.tool_name,
                args: record.args,
                args_hash: record.args_hash,
                execution_id: record.execution_id,
                created_at: record.created_at,
                decided: record.decided,
                decision: record.decision.as_ref().map(decision_from_record),
                tx: None, // no live request after a restart
            };
            pending.insert(record.id, entry);
            loaded += 1;
        }
        Ok(loaded)
    }

    /// The decision recorded for an approval, if it has been decided. This is
    /// the accessor the retry path uses after a restart: a decision made while
    /// no live oneshot existed is observable here (and in the store), so the
    /// retry can proceed with Approved/Denied instead of hanging or re-asking.
    pub fn decision_for(&self, id: Uuid) -> Option<ApprovalDecision> {
        self.pending
            .read()
            .expect("approval gate lock poisoned")
            .get(&id)
            .and_then(|entry| entry.decision.clone())
    }
}

fn decision_record(decision: &ApprovalDecision) -> ApprovalDecisionRecord {
    match decision {
        ApprovalDecision::Approved => ApprovalDecisionRecord::Approved,
        ApprovalDecision::Denied { reason } => ApprovalDecisionRecord::Denied {
            reason: reason.clone(),
        },
    }
}

fn decision_from_record(record: &ApprovalDecisionRecord) -> ApprovalDecision {
    match record {
        ApprovalDecisionRecord::Approved => ApprovalDecision::Approved,
        ApprovalDecisionRecord::Denied { reason } => ApprovalDecision::Denied {
            reason: reason.clone(),
        },
    }
}

fn is_expired(entry: &PendingApproval, now: DateTime<Utc>, timeout: Duration) -> bool {
    match now.signed_duration_since(entry.created_at).to_std() {
        Ok(elapsed) => elapsed >= timeout,
        // A future-dated created_at (clock skew) is not expired.
        Err(_) => false,
    }
}
