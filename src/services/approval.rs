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

use crate::error::AppError;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::RwLock;
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
    pub created_at: DateTime<Utc>,
    /// Set once the oneshot has fired. A decided entry is kept until reaped so
    /// a second decision can be told apart from an unknown id (409 vs 404).
    pub decided: bool,
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
#[derive(Debug)]
pub struct ApprovalGate {
    pending: RwLock<HashMap<Uuid, PendingApproval>>,
    timeout: Duration,
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

/// SHA-256 over the canonical JSON of the arguments.
///
/// `serde_json::Value` uses a `BTreeMap` for objects unless `preserve_order`
/// is enabled (it is not), so key order is sorted and a reordered payload
/// hashes identically. A nondeterministic hash would reject legitimate
/// approvals when the same arguments come back with different key order.
pub fn args_hash(args: &Value) -> [u8; 32] {
    let canonical = serde_json::to_vec(args).expect("a serde_json::Value always serializes");
    let digest = Sha256::digest(&canonical);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// The lowercase hex form sent to (and compared from) HTTP clients.
pub fn args_hash_hex(args: &Value) -> String {
    let mut hex = String::with_capacity(64);
    for byte in args_hash(args) {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
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
        }
    }

    /// How long a pending decision stays valid. The bridge wraps its `rx.await`
    /// in a `tokio::time::timeout` of this length so a request nobody decides
    /// fails with a named error instead of hanging forever (the gate's own
    /// reaper only runs when `request`/`decide` is next called).
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Register a pending approval. Reaps expired entries first, then returns
    /// a fresh id and the receiver the caller awaits before executing a tool.
    pub fn request(
        &self,
        tool_name: impl Into<String>,
        args: &Value,
    ) -> (Uuid, oneshot::Receiver<ApprovalDecision>) {
        self.prune_expired();
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        let entry = PendingApproval {
            tool_name: tool_name.into(),
            args: args.clone(),
            args_hash: args_hash(args),
            created_at: Utc::now(),
            decided: false,
            tx: Some(tx),
        };
        self.pending
            .write()
            .expect("approval gate lock poisoned")
            .insert(id, entry);
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
        // Reap the rest while the write lock is already held.
        pending.retain(|_, entry| !is_expired(entry, now, self.timeout));

        let entry = pending.get_mut(&id).ok_or(ApprovalError::NotFound(id))?;
        if entry.decided {
            // Kept on purpose: a repeated decision is a conflict, not a miss.
            return Err(ApprovalError::AlreadyDecided(id));
        }
        if entry.args_hash != submitted_hash {
            return Err(ApprovalError::ArgsChanged);
        }
        // Mark decided and take the sender so the oneshot can fire at most once,
        // but keep the entry until the reaper collects it so a second decision
        // is distinguishable from an unknown id.
        entry.decided = true;
        let tx = entry
            .tx
            .take()
            .expect("an undecided entry still owns its sender");
        let decision = if approve {
            ApprovalDecision::Approved
        } else {
            ApprovalDecision::Denied {
                reason: "denied by human".to_string(),
            }
        };
        // The receiver may have been dropped (client disconnected); that must
        // not turn a valid decision into an error.
        let _ = tx.send(decision);
        Ok(())
    }

    /// Drop every pending approval older than the configured timeout.
    pub fn prune_expired(&self) {
        let now = Utc::now();
        let mut pending = self.pending.write().expect("approval gate lock poisoned");
        pending.retain(|_, entry| !is_expired(entry, now, self.timeout));
    }
}

fn is_expired(entry: &PendingApproval, now: DateTime<Utc>, timeout: Duration) -> bool {
    match now.signed_duration_since(entry.created_at).to_std() {
        Ok(elapsed) => elapsed >= timeout,
        // A future-dated created_at (clock skew) is not expired.
        Err(_) => false,
    }
}
