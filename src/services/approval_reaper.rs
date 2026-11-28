//! Background reaper for orphaned parked approvals.
//!
//! A turn that parks on a human decision is persisted with status
//! `WaitingApproval` and its pending approval is mirrored into the execution
//! store. When the process restarts the oneshot is gone, so there is no live
//! bridge to wake: the turn is an *orphan*. If a human decided while no live
//! request existed, the decision is still durable and the turn can be resumed.
//!
//! [`reap_once`] is one directly testable pass. It resumes only turns that are
//! (a) still `WaitingApproval`, (b) not parked in this process — `decide`
//! unparks before firing the oneshot, so a live turn is never eligible — and
//! (c) carry an actual grant (`Approved`) recorded in the gate. A denial, a
//! hash mismatch (no decision on file) and a still-live approval are all left
//! alone. The resume reuses the original execution id through the shared retry
//! helper, so tool effects already recorded under that id are replayed instead
//! of repeated. No new status values are introduced.
//!
//! Everything here is best effort: a store error is returned to the caller
//! (which logs a warning) and never fails boot or a chat.

use crate::api::execution_api::retry_turn;
use crate::api::handlers::AppState;
use crate::domain::execution::ExecutionStatus;
use crate::services::approval::ApprovalDecision;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// How often the boot task checks for decided orphans. Derived from `Config` —
/// the single source of truth for the `REAPER_INTERVAL_SECS` env var.
pub const DEFAULT_REAPER_INTERVAL: Duration =
    Duration::from_secs(crate::config::DEFAULT_REAPER_INTERVAL_SECS);

/// One reaper pass. Returns the number of decided orphans resumed under their
/// original execution id. Takes only `state` so the pass is directly callable
/// from tests without spawning a task; scheduling stays in the caller.
pub async fn reap_once(state: &AppState) -> Result<usize, String> {
    let store = state.execution_store.as_ref();
    // `list_interrupted` returns `Running` + `WaitingApproval`; the reaper cares
    // only about the latter. Filtering here keeps the sacred port unchanged.
    let orphans: Vec<_> = store
        .list_interrupted()
        .await?
        .into_iter()
        .filter(|execution| execution.status == ExecutionStatus::WaitingApproval)
        .collect();
    if orphans.is_empty() {
        return Ok(0);
    }

    let pendings = store.load_pending_approvals().await?;
    let gate = state.approval_gate.as_ref();
    let mut reaped = 0usize;

    for execution in orphans {
        // The gate is the authority on the decision: a rehydrated pending has
        // no oneshot, but `decision_for` exposes the human's recorded answer.
        // `parked_live` is the guard against double resume — a live turn was
        // unparked by `decide` before its oneshot fired, so anything still
        // parked here has a live bridge and must never be claimed.
        let grant = pendings.iter().find(|record| {
            record.execution_id == Some(execution.id)
                && !gate.is_parked_live(record.id)
                && matches!(
                    gate.decision_for(record.id),
                    Some(ApprovalDecision::Approved)
                )
        });
        let Some(grant) = grant else {
            tracing::debug!(
                execution_id = %execution.id,
                "waiting-approval turn has no resumable grant; skipping"
            );
            continue;
        };
        let approval_id = grant.id;

        // Claim the orphan before re-running so a second pass cannot race in
        // and resume the same turn. The retry helper flips it back to Running
        // and finalizes it; reusing the id is what engages the replay cache.
        if let Err(err) = store
            .set_status(execution.id, ExecutionStatus::Running, None)
            .await
        {
            tracing::warn!(
                error = %err,
                execution_id = %execution.id,
                "approval reaper could not claim an orphan; continuing"
            );
            continue;
        }

        // An orphan has no operator-supplied checkpoint range: replay with all
        // recorded steps, exactly as before Task 2.
        match retry_turn(state, execution.id, &execution, 0).await {
            Ok(()) => {
                // No-op for an orphan (it was never parked here), kept explicit
                // so the invariant "a resumed approval is never parked" holds.
                gate.unpark(approval_id);
                reaped += 1;
                tracing::info!(
                    execution_id = %execution.id,
                    approval_id = %approval_id,
                    "resumed an orphaned parked approval under its original execution id"
                );
            }
            Err(err) => tracing::warn!(
                error = %err,
                execution_id = %execution.id,
                "approval reaper could not resume an orphan; continuing"
            ),
        }
    }
    Ok(reaped)
}

/// Spawn the periodic reaper. The first tick fires immediately, which is what
/// resumes decided orphans shortly after boot; every pass logs its own errors
/// so a store failure is a warning, never a task panic or a failed boot.
pub fn spawn_approval_reaper(state: Arc<AppState>, interval: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            match reap_once(&state).await {
                Ok(count) if count > 0 => tracing::info!(
                    count,
                    interval_ms = interval.as_millis(),
                    "approval reaper pass resumed decided orphans"
                ),
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(error = %err, "approval reaper pass failed; continuing")
                }
            }
        }
    })
}
