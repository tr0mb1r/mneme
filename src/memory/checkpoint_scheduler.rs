//! Phase 6 §8.2 — background scheduler for L1 working-session
//! checkpoints.
//!
//! Per spec §8.3 the working session is flushed on whichever of two
//! triggers fires first:
//!
//! * Wall-clock: every `[checkpoints] session_interval_secs` (default
//!   30 s).
//! * Turn count: every `[checkpoints] session_interval_turns` (default
//!   5 turns), where a "turn" is one successful `tools/call` against
//!   the MCP server.
//!
//! The scheduler wraps an [`ActiveSession`] (which owns the redb-
//! adjacent on-disk session snapshot) and the two atomic counters that
//! make the triggers cheap to evaluate. It looks structurally identical
//! to [`crate::memory::consolidation_scheduler::ConsolidationScheduler`]
//! by design: same `tokio::select!{sleep, notify}` shape, same shutdown
//! protocol, same observability surface.
//!
//! # Crash safety
//!
//! Inherits from [`super::working::Session::checkpoint`], which writes
//! to `<id>.snapshot.tmp` and renames in. A crash mid-flush leaves
//! either the previous snapshot or the new one — never a torn write.
//! `clean_shutdown` is only set on graceful shutdown, so a recovered
//! snapshot can be distinguished from a crash-survivor.
//!
//! # Cadence rationale
//!
//! Why two triggers? Pure wall-clock under-flushes a busy session
//! (5 turns of dense back-and-forth in 10 s would survive only as the
//! 30-s checkpoint, losing 20 s of work on a crash). Pure turn-count
//! under-flushes a slow session (a long `recall` followed by silence
//! could leave one turn unflushed indefinitely). "First-to-fire" is
//! the simplest combinator that bounds both axes.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::Result;
use crate::config::CheckpointsConfig;
use crate::ids::SessionId;
use crate::memory::working::ActiveSession;

/// Scheduler tunables. Built from
/// [`crate::config::CheckpointsConfig`] in production; hand-rolled in
/// tests for fast cadences.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointSchedulerConfig {
    /// Wall-clock ceiling between checkpoints. The scheduler fires
    /// at least this often when the session has any pending turns.
    pub interval_secs: u64,
    /// Turn-count ceiling. The scheduler fires when
    /// `turns_since_last_checkpoint() >= interval_turns`, regardless
    /// of wall-clock.
    pub interval_turns: u32,
    /// Off → no spawned task, `metrics()` reads zeros, `shutdown()`
    /// is a no-op.
    pub enabled: bool,
}

impl CheckpointSchedulerConfig {
    /// Production default: 30-s wall-clock, 5-turn count.
    pub fn production() -> Self {
        Self {
            interval_secs: 30,
            interval_turns: 5,
            enabled: true,
        }
    }

    pub fn disabled() -> Self {
        Self {
            interval_secs: 0,
            interval_turns: 0,
            enabled: false,
        }
    }

    pub fn from_config(c: &CheckpointsConfig) -> Self {
        Self {
            interval_secs: c.session_interval_secs,
            interval_turns: c.session_interval_turns,
            enabled: true,
        }
    }
}

/// Snapshot of scheduler counters for `mneme://stats` and the `stats`
/// tool. Cheap atomic loads; values are monotonic so a stale read
/// only ever underestimates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointMetrics {
    /// The active session's id. Stable for the process lifetime.
    pub session_id: SessionId,
    /// When the active session started (process start, basically).
    pub started_at: DateTime<Utc>,
    /// Most recent successful checkpoint, if any.
    pub last_checkpoint_at: Option<DateTime<Utc>>,
    /// Total turns recorded since boot.
    pub turns_total: u64,
    /// Successful checkpoint count since boot, including any final
    /// shutdown flush.
    pub checkpoints_total: u64,
    /// Errors observed during background flushes; doesn't count a
    /// failed shutdown flush (which surfaces directly).
    pub errors_total: u64,
}

struct SchedulerState {
    active: Arc<ActiveSession>,
    interval: Duration,
    interval_turns: u64,

    shutdown: AtomicBool,
    notify: Notify,

    /// Last unix-millis the loop fired a successful checkpoint;
    /// `0` until the first one. Used to stamp metrics; the
    /// authoritative `last_checkpoint_at` lives on `ActiveSession`.
    last_flush_unix_ms: AtomicI64,
    errors_total: AtomicU64,
}

impl SchedulerState {
    fn metrics(&self) -> CheckpointMetrics {
        let last_ms = self.last_flush_unix_ms.load(Ordering::SeqCst);
        // Prefer the ActiveSession's authoritative timestamp; fall
        // back to the loop's stamp if `last_checkpoint_at` is None
        // (the only difference is ordering, since the same flush
        // updates both).
        let last_checkpoint_at = self.active.last_checkpoint_at().or_else(|| {
            if last_ms == 0 {
                None
            } else {
                Utc.timestamp_millis_opt(last_ms).single()
            }
        });
        CheckpointMetrics {
            session_id: self.active.id(),
            started_at: self.active.started_at(),
            last_checkpoint_at,
            turns_total: self.active.turns_total(),
            checkpoints_total: self.active.checkpoints_total(),
            errors_total: self.errors_total.load(Ordering::SeqCst),
        }
    }
}

/// Background runner for L1 session checkpoints. Owned by `cli::run`
/// for the lifetime of `mneme run`.
pub struct CheckpointScheduler {
    state: Arc<SchedulerState>,
    join: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl CheckpointScheduler {
    /// Spawn the scheduler. Must be called inside a tokio runtime.
    pub fn start(active: Arc<ActiveSession>, config: CheckpointSchedulerConfig) -> Arc<Self> {
        let state = Arc::new(SchedulerState {
            active,
            interval: Duration::from_secs(config.interval_secs),
            interval_turns: config.interval_turns as u64,
            shutdown: AtomicBool::new(false),
            notify: Notify::new(),
            last_flush_unix_ms: AtomicI64::new(0),
            errors_total: AtomicU64::new(0),
        });

        let join = if config.enabled {
            let task_state = Arc::clone(&state);
            let handle = tokio::spawn(async move {
                scheduler_loop(task_state).await;
            });
            std::sync::Mutex::new(Some(handle))
        } else {
            std::sync::Mutex::new(None)
        };

        Arc::new(Self { state, join })
    }

    /// Cheap snapshot of all observability counters. Safe to call
    /// from any thread.
    pub fn metrics(&self) -> CheckpointMetrics {
        self.state.metrics()
    }

    /// Wake the scheduler immediately if it's currently sleeping.
    /// Used by the MCP server after each successful `tools/call` so
    /// the turn-count threshold is checked promptly rather than at
    /// the next wall-clock tick.
    pub fn poke(&self) {
        self.state.notify.notify_one();
    }

    /// Stop the scheduler gracefully. Sets the shutdown flag, wakes
    /// the loop, awaits the spawned task, and writes a final
    /// checkpoint if there are any unflushed turns. Idempotent; a
    /// second call after shutdown returns immediately.
    pub async fn shutdown(&self) -> Result<()> {
        self.state.shutdown.store(true, Ordering::SeqCst);
        self.state.notify.notify_one();

        let join = match self.join.lock() {
            Ok(mut g) => g.take(),
            Err(e) => {
                tracing::warn!(error = %e, "checkpoint scheduler join mutex poisoned");
                None
            }
        };
        if let Some(j) = join
            && let Err(e) = j.await
            && !e.is_cancelled()
        {
            tracing::warn!(error = %e, "checkpoint scheduler task panicked during shutdown");
        }

        // Final flush — `ActiveSession::shutdown` marks
        // `clean_shutdown = true` and writes one last snapshot. The
        // background loop may have already done a flush on the
        // shutdown wake; the snapshot is idempotent so a duplicate
        // flush is safe.
        self.state.active.shutdown()
    }

    /// Run one checkpoint pass synchronously, ignoring the scheduler
    /// gate. Tests use this to drive deterministic state changes
    /// without timing coupling.
    #[doc(hidden)]
    pub fn force_run(&self) -> Result<()> {
        do_one_pass(&self.state)
    }
}

async fn scheduler_loop(state: Arc<SchedulerState>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(state.interval) => {
                // Wall-clock trigger: flush whatever's pending. The
                // interval ceiling is the whole point of this branch
                // — if turns are pending, they shouldn't outlive
                // `interval_secs` even if poke is silent.
                if state.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let pending = state.active.turns_since_last_checkpoint();
                if pending > 0
                    && let Err(e) = do_one_pass(&state)
                {
                    tracing::warn!(error = %e, "session checkpoint failed (wall-clock); will retry next tick");
                }
            }
            _ = state.notify.notified() => {
                // Poke / shutdown trigger. Shutdown short-circuits;
                // otherwise only flush when the turn-count threshold
                // is crossed so we don't disk-write on every single
                // tools/call.
                if state.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let pending = state.active.turns_since_last_checkpoint();
                if pending >= state.interval_turns
                    && pending > 0
                    && let Err(e) = do_one_pass(&state)
                {
                    tracing::warn!(error = %e, "session checkpoint failed (turn-count); will retry next tick");
                }
            }
        }
    }
}

fn do_one_pass(state: &SchedulerState) -> Result<()> {
    match state.active.checkpoint() {
        Ok(()) => {
            state
                .last_flush_unix_ms
                .store(Utc::now().timestamp_millis(), Ordering::SeqCst);
            Ok(())
        }
        Err(e) => {
            state.errors_total.fetch_add(1, Ordering::SeqCst);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fast_config_secs(secs: u64) -> CheckpointSchedulerConfig {
        CheckpointSchedulerConfig {
            interval_secs: secs,
            interval_turns: 5,
            enabled: true,
        }
    }

    /// Disabled config builds the scheduler with no spawned task —
    /// `metrics()` reads zeros (apart from session_id/started_at),
    /// `shutdown()` writes one final clean-shutdown checkpoint.
    #[tokio::test]
    async fn disabled_scheduler_still_flushes_on_shutdown() {
        let tmp = TempDir::new().unwrap();
        let active = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        active.push_turn("user", "hi");
        let sched =
            CheckpointScheduler::start(Arc::clone(&active), CheckpointSchedulerConfig::disabled());
        let m_before = sched.metrics();
        assert_eq!(m_before.checkpoints_total, 0);
        sched.shutdown().await.unwrap();
        let m_after = sched.metrics();
        assert_eq!(m_after.checkpoints_total, 1);
        assert!(m_after.last_checkpoint_at.is_some());
    }

    /// With a fast 1-second wall-clock and unmoving turn count, the
    /// scheduler should still flush once turns are pending.
    #[tokio::test]
    async fn fires_on_wall_clock_with_pending_turns() {
        let tmp = TempDir::new().unwrap();
        let active = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        active.push_turn("user", "hello");
        active.push_turn("assistant", "hi");

        let sched = CheckpointScheduler::start(
            Arc::clone(&active),
            CheckpointSchedulerConfig {
                interval_secs: 1,
                interval_turns: 9999, // disable the turn trigger
                enabled: true,
            },
        );

        // Wait up to 3 s for a wall-clock-driven flush.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if sched.metrics().checkpoints_total >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "scheduler never fired wall-clock flush; metrics = {:?}",
                    sched.metrics()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        sched.shutdown().await.unwrap();
    }

    /// Turn-count trigger: fast `poke()` after enough turns produces
    /// a flush before the wall-clock interval would have fired.
    #[tokio::test]
    async fn fires_on_turn_count_via_poke() {
        let tmp = TempDir::new().unwrap();
        let active = ActiveSession::open(tmp.path().to_path_buf()).unwrap();

        let sched = CheckpointScheduler::start(
            Arc::clone(&active),
            CheckpointSchedulerConfig {
                interval_secs: 30, // wall-clock effectively off for this test
                interval_turns: 3,
                enabled: true,
            },
        );

        // Push 3 turns + poke after each — the scheduler reads
        // turns_since_last_checkpoint on wake, so any wake with
        // pending work flushes.
        for i in 0..3 {
            active.push_turn("user", format!("msg {i}"));
            sched.poke();
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if sched.metrics().checkpoints_total >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "scheduler never fired turn-count flush; metrics = {:?}",
                    sched.metrics()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        sched.shutdown().await.unwrap();
    }

    /// `force_run` bypasses the gate and updates metrics. Useful for
    /// tests that want a deterministic flush.
    #[tokio::test]
    async fn force_run_writes_a_checkpoint() {
        let tmp = TempDir::new().unwrap();
        let active = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        active.push_turn("user", "x");

        let sched =
            CheckpointScheduler::start(Arc::clone(&active), CheckpointSchedulerConfig::disabled());
        sched.force_run().unwrap();
        let m = sched.metrics();
        assert_eq!(m.checkpoints_total, 1);
        assert_eq!(m.turns_total, 1);
        assert!(m.last_checkpoint_at.is_some());
        sched.shutdown().await.unwrap();
    }

    /// Empty-session ticks don't generate spurious checkpoints — a
    /// session with zero turns should never produce a flush.
    #[tokio::test]
    async fn empty_session_does_not_flush_on_tick() {
        let tmp = TempDir::new().unwrap();
        let active = ActiveSession::open(tmp.path().to_path_buf()).unwrap();
        let sched = CheckpointScheduler::start(Arc::clone(&active), fast_config_secs(1));

        // Wait long enough for at least one tick to expire.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        // Background loop produces zero flushes when there are no
        // pending turns. The shutdown call below adds the final one.
        assert_eq!(sched.metrics().checkpoints_total, 0);
        sched.shutdown().await.unwrap();
        // After shutdown the count is 1 (the final clean-shutdown
        // flush always runs, even on an empty session).
        assert_eq!(sched.metrics().checkpoints_total, 1);
    }
}
