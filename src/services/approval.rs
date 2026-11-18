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

/// One pending human decision. Only non-secret data is kept here: the hash is
/// one-way, and the raw arguments travel in the stream event / API response
/// built by the caller, not in this struct.
#[derive(Debug)]
pub struct PendingApproval {
    pub tool_name: String,
    pub args_hash: [u8; 32],
    pub created_at: DateTime<Utc>,
    tx: oneshot::Sender<ApprovalDecision>,
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
    #[error("approval arguments changed since the request was made")]
    ArgsChanged,
}

impl From<ApprovalError> for AppError {
    fn from(err: ApprovalError) -> Self {
        match err {
            ApprovalError::NotFound(id) => AppError::ApprovalNotFound(id.to_string()),
            ApprovalError::Expired(id) => AppError::ApprovalExpired(id.to_string()),
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
            args_hash: args_hash(args),
            created_at: Utc::now(),
            tx,
        };
        self.pending
            .write()
            .expect("approval gate lock poisoned")
            .insert(id, entry);
        (id, rx)
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

        let entry = pending.get(&id).ok_or(ApprovalError::NotFound(id))?;
        if entry.args_hash != submitted_hash {
            return Err(ApprovalError::ArgsChanged);
        }
        let entry = pending.remove(&id).expect("entry was just checked");
        let decision = if approve {
            ApprovalDecision::Approved
        } else {
            ApprovalDecision::Denied {
                reason: "denied by human".to_string(),
            }
        };
        // The receiver may have been dropped (client disconnected); that must
        // not turn a valid decision into an error.
        let _ = entry.tx.send(decision);
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
