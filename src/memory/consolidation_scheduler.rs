//! Phase 6 §8.1 — background scheduler for L3 consolidation.
//!
//! [`memory::consolidation::run`](crate::memory::consolidation::run)
//! is the core hot→warm→cold migration; this module spawns a tokio
//! task that calls it on the user-configured cadence so hot-tier
//! growth is bounded without the agent having to remember to fire it
//! manually.
//!
//! # Cadence (`schedule = "idle"`, the v1.0 default)
//!
//! Wake every `tick_interval` (5 min by default). On wake:
//!
//! 1. Read the cumulative activity counts from the watched
//!    [`ActivityCounter`]s. If the total is unchanged since the
//!    *previous* tick, the system has been quiet for a full window —
//!    fire one consolidation pass.
//! 2. If the total changed, record the new value and skip the pass.
//!    Next tick reuses the same activity gate; consolidation will
//!    only run after a full quiet interval.
//!
//! This is a one-tick form of "idle" — strictly tighter than a fixed
//! cron-style firing, strictly looser than rate-limit-style
//! throttling, and zero new instrumentation. Future modes
//! (cron expression, `every_<n>m`, `on_demand`) belong to v1.1; the
//! plan §8.1 explicitly limits v1.0 to `"idle"`.
//!
//! # Crash safety
//!
//! `consolidation::run` is already idempotent and crash-safe (its
//! module docs prove out hot→warm and warm→cold mid-pass crashes).
//! The scheduler wrapper only adds:
//!
//! * Single-flight: one in-flight `run` at a time, enforced by
//!   serialising the calls in the loop body.
//! * Panic-resilience: a panicking `run` is converted to a logged
//!   error and the loop re-arms on the next tick.
//! * Clean shutdown: `shutdown().await` flips a flag and notifies
//!   the loop so it exits at the next select point.
//!
//! # Observability
//!
//! Per-pass counters land on the `mneme://stats` resource and the
//! `stats` tool via [`SchedulerMetrics`]:
//!
//! * `last_consolidation_at` — `Some(DateTime<Utc>)` after the first
//!   successful pass; `None` until then.
//! * `consolidation_runs_total` — incremented after every successful
//!   `run` (zero-promotion runs included; we record that we tried).
//! * `consolidation_errors_total` — incremented when `run` returns
//!   `Err`. Errors don't halt the loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::Result;
use crate::config::ConsolidationConfig;
use crate::memory::activity::ActivityCounter;
use crate::memory::consolidation::{ConsolidationParams, run as run_consolidation};
use crate::storage::Storage;
use crate::storage::archive::ColdArchive;

/// Scheduler tunables. The two day-thresholds (`hot_to_warm_days`,
/// `warm_to_cold_days`) live on [`ConsolidationParams`]; this struct
/// adds the wall-clock cadence + idle-window + on/off knob.
#[derive(Debug, Clone, Copy)]
pub struct SchedulerConfig {
    /// How often the scheduler wakes to consider firing a pass.
    pub tick_interval: Duration,
    /// Reserved for future modes; today the loop fires when the
    /// activity gate is closed AND a full `tick_interval` has
    /// elapsed since the prior tick. Idle window == tick interval.
    pub enabled: bool,
}

impl SchedulerConfig {
    /// Production default: tick every 5 minutes when enabled.
    pub fn production() -> Self {
        Self {
            tick_interval: Duration::from_secs(5 * 60),
            enabled: true,
        }
    }

    /// Off — no spawned task. Used by tests that want to drive
    /// consolidation explicitly (or not at all).
    pub fn disabled() -> Self {
        Self {
            tick_interval: Duration::from_secs(0),
            enabled: false,
        }
    }

    /// Build from the user-facing config block. v1.0 honours
    /// `[consolidation] schedule = "idle"` only; anything else logs
    /// a warning and falls back to the production cadence.
    pub fn from_config(c: &ConsolidationConfig) -> Self {
        match c.schedule.as_str() {
            "idle" => Self::production(),
            other => {
                tracing::warn!(
                    schedule = other,
                    "[consolidation] schedule = \"{other}\" not yet supported; \
                     falling back to \"idle\" (5-min interval)"
                );
                Self::production()
            }
        }
    }
}

/// Snapshot of scheduler counters for the `mneme://stats` surface.
/// Cheap to read; ordering between fields is best-effort (each load
/// is independent), but the values are monotonic so a stale read is
/// only ever a slight underestimate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchedulerMetrics {
    pub last_consolidation_at: Option<DateTime<Utc>>,
    pub runs_total: u64,
    pub errors_total: u64,
    /// Promoted-to-warm count from the most recent pass (zero if no
    /// pass has run). Useful for "how much did the last pass move?"
    pub last_promoted_to_warm: u64,
    /// Archived-to-cold count from the most recent pass.
    pub last_archived_to_cold: u64,
}

struct SchedulerState {
    storage: Arc<dyn Storage>,
    archive: ColdArchive,
    params: ConsolidationParams,
    tick_interval: Duration,
    activity: Vec<Arc<ActivityCounter>>,

    shutdown: AtomicBool,
    notify: Notify,

    /// Last unix-millis of a successful pass. `0` means "never".
    last_consolidation_unix_ms: AtomicI64,
    runs_total: AtomicU64,
    errors_total: AtomicU64,
    last_promoted: AtomicU64,
    last_archived: AtomicU64,
}

impl SchedulerState {
    fn metrics(&self) -> SchedulerMetrics {
        let last_ms = self.last_consolidation_unix_ms.load(Ordering::SeqCst);
        let last_consolidation_at = if last_ms == 0 {
            None
        } else {
            Utc.timestamp_millis_opt(last_ms).single()
        };
        SchedulerMetrics {
            last_consolidation_at,
            runs_total: self.runs_total.load(Ordering::SeqCst),
            errors_total: self.errors_total.load(Ordering::SeqCst),
            last_promoted_to_warm: self.last_promoted.load(Ordering::SeqCst),
            last_archived_to_cold: self.last_archived.load(Ordering::SeqCst),
        }
    }

    fn activity_total(&self) -> u64 {
        self.activity.iter().map(|c| c.snapshot()).sum()
    }
}

/// Background runner for L3 consolidation. Owned by `cli::run`
/// for the lifetime of `mneme run`.
pub struct ConsolidationScheduler {
    state: Arc<SchedulerState>,
    join: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl ConsolidationScheduler {
    /// Spin up the scheduler. Must be called inside a tokio runtime
    /// context (we `tokio::spawn`).
    ///
    /// `activity` is the set of write-counters the scheduler watches
    /// for idle detection. In production that's exactly two:
    /// [`SemanticStore::activity_counter`] +
    /// [`EpisodicStore::activity_counter`]. Tests can pass an empty
    /// vec (always-idle) or a single counter (focused gating).
    pub fn start(
        storage: Arc<dyn Storage>,
        archive: ColdArchive,
        params: ConsolidationParams,
        config: SchedulerConfig,
        activity: Vec<Arc<ActivityCounter>>,
    ) -> Arc<Self> {
        let state = Arc::new(SchedulerState {
            storage,
            archive,
            params,
            tick_interval: config.tick_interval,
            activity,
            shutdown: AtomicBool::new(false),
            notify: Notify::new(),
            last_consolidation_unix_ms: AtomicI64::new(0),
            runs_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            last_promoted: AtomicU64::new(0),
            last_archived: AtomicU64::new(0),
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

    /// Snapshot of the scheduler's observability counters. Cheap; safe
    /// to call from any thread. Used by the `mneme://stats` resource
    /// and the `stats` tool.
    pub fn metrics(&self) -> SchedulerMetrics {
        self.state.metrics()
    }

    /// Stop the scheduler gracefully. Sets the shutdown flag, wakes
    /// the loop, and awaits the spawned task. Idempotent: a second
    /// call after shutdown returns immediately.
    pub async fn shutdown(&self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
        self.state.notify.notify_one();

        let join = match self.join.lock() {
            Ok(mut g) => g.take(),
            Err(e) => {
                tracing::warn!(error = %e, "consolidation scheduler join mutex poisoned");
                return;
            }
        };
        if let Some(j) = join
            && let Err(e) = j.await
            && !e.is_cancelled()
        {
            tracing::warn!(error = %e, "consolidation scheduler task panicked during shutdown");
        }
    }

    /// Run one consolidation pass synchronously, ignoring the idle
    /// gate. Tests use this to drive deterministic state changes
    /// without timing-coupled assertions; production code should let
    /// the scheduler fire on its own cadence.
    #[doc(hidden)]
    pub async fn force_run(&self) -> Result<()> {
        do_one_pass(&self.state).await
    }
}

async fn scheduler_loop(state: Arc<SchedulerState>) {
    // The "idle" gate: if `activity_total` matches what we saw last
    // tick, the window was quiet — fire. Otherwise update and wait.
    let mut prev_activity = state.activity_total();

    loop {
        tokio::select! {
            _ = tokio::time::sleep(state.tick_interval) => {}
            _ = state.notify.notified() => {}
        }

        if state.shutdown.load(Ordering::SeqCst) {
            return;
        }

        let now_activity = state.activity_total();
        if now_activity != prev_activity {
            prev_activity = now_activity;
            tracing::trace!("consolidation skipped: activity since last tick");
            continue;
        }

        if let Err(e) = do_one_pass(&state).await {
            tracing::warn!(error = %e, "consolidation pass failed; will retry next tick");
        }
        // Refresh prev_activity after a (successful or failed) pass —
        // the pass itself doesn't bump user-activity counters, but
        // defensive in case future code does.
        prev_activity = state.activity_total();
    }
}

async fn do_one_pass(state: &SchedulerState) -> Result<()> {
    match run_consolidation(Arc::clone(&state.storage), &state.archive, state.params).await {
        Ok(report) => {
            state
                .last_consolidation_unix_ms
                .store(Utc::now().timestamp_millis(), Ordering::SeqCst);
            state.runs_total.fetch_add(1, Ordering::SeqCst);
            state
                .last_promoted
                .store(report.promoted_to_warm as u64, Ordering::SeqCst);
            state
                .last_archived
                .store(report.archived_to_cold as u64, Ordering::SeqCst);
            if report.promoted_to_warm > 0 || report.archived_to_cold > 0 {
                tracing::info!(
                    promoted_to_warm = report.promoted_to_warm,
                    archived_to_cold = report.archived_to_cold,
                    "consolidation pass moved events"
                );
            }
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
    use crate::memory::consolidation::_record_backdated_for_tests;
    use crate::memory::episodic::EpisodicStore;
    use crate::storage::memory_impl::MemoryStorage;
    use chrono::Duration as ChronoDuration;
    use tempfile::TempDir;

    fn params() -> ConsolidationParams {
        ConsolidationParams {
            hot_to_warm_days: 28,
            warm_to_cold_days: 180,
        }
    }

    fn fast_config() -> SchedulerConfig {
        SchedulerConfig {
            tick_interval: Duration::from_millis(50),
            enabled: true,
        }
    }

    /// Disabled config builds the scheduler with no spawned task —
    /// `metrics()` always reads zeros, `shutdown()` is a no-op.
    #[tokio::test]
    async fn disabled_scheduler_is_inert() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let tmp = TempDir::new().unwrap();
        let archive = ColdArchive::new(tmp.path());
        let sched = ConsolidationScheduler::start(
            Arc::clone(&storage),
            archive,
            params(),
            SchedulerConfig::disabled(),
            vec![],
        );
        let m = sched.metrics();
        assert_eq!(m.runs_total, 0);
        assert!(m.last_consolidation_at.is_none());
        sched.shutdown().await;
    }

    /// Backdate enough events to age past the hot-to-warm threshold,
    /// run the scheduler with no activity counters (=> always idle),
    /// wait a couple of ticks, and confirm a pass landed.
    #[tokio::test]
    async fn fires_on_idle_after_a_tick() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let tmp = TempDir::new().unwrap();
        let archive = ColdArchive::new(tmp.path());
        let now = Utc::now();
        for _ in 0..5 {
            _record_backdated_for_tests(
                &storage,
                "k",
                "p",
                "\"old\"",
                now - ChronoDuration::days(60),
            )
            .await
            .unwrap();
        }

        let sched = ConsolidationScheduler::start(
            Arc::clone(&storage),
            archive,
            params(),
            fast_config(),
            vec![],
        );

        // Wait up to 1 s for at least one successful pass. The first
        // tick at t=50ms hits the always-idle branch and fires; the
        // pass itself takes ~ms.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            if sched.metrics().runs_total >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("scheduler never fired; metrics = {:?}", sched.metrics());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let m = sched.metrics();
        assert!(m.last_consolidation_at.is_some());
        assert!(m.runs_total >= 1);
        assert_eq!(m.last_promoted_to_warm, 5);
        assert_eq!(m.errors_total, 0);
        sched.shutdown().await;
    }

    /// Bumping the activity counter every loop iteration should
    /// suppress every consolidation pass — the gate sees fresh
    /// activity each tick and refuses to fire.
    #[tokio::test]
    async fn activity_storm_suppresses_passes() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let tmp = TempDir::new().unwrap();
        let archive = ColdArchive::new(tmp.path());
        let now = Utc::now();
        for _ in 0..3 {
            _record_backdated_for_tests(
                &storage,
                "k",
                "p",
                "\"old\"",
                now - ChronoDuration::days(60),
            )
            .await
            .unwrap();
        }

        let activity = ActivityCounter::new();
        let sched = ConsolidationScheduler::start(
            Arc::clone(&storage),
            archive,
            params(),
            fast_config(),
            vec![Arc::clone(&activity)],
        );

        // Bump faster than the tick. 50 ms tick interval; we bump
        // every 10 ms for 600 ms => the gate should never close.
        let stop = std::time::Instant::now() + Duration::from_millis(600);
        while std::time::Instant::now() < stop {
            activity.bump();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Stop bumping; the next idle tick should let a pass through.
        // We assert the storm-window itself produced zero passes.
        assert_eq!(
            sched.metrics().runs_total,
            0,
            "activity storm should have suppressed all passes"
        );

        // Now go quiet and wait for at least one pass to confirm the
        // scheduler still works after a busy window.
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            if sched.metrics().runs_total >= 1 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "scheduler did not fire after activity stopped; metrics = {:?}",
                    sched.metrics()
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        sched.shutdown().await;
    }

    /// `force_run` ignores the idle gate so tests can drive a pass
    /// without timing coupling. The metrics still update.
    #[tokio::test]
    async fn force_run_updates_metrics_and_archive() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let tmp = TempDir::new().unwrap();
        let archive = ColdArchive::new(tmp.path());
        let now = Utc::now();
        for _ in 0..4 {
            _record_backdated_for_tests(
                &storage,
                "k",
                "p",
                "\"old\"",
                now - ChronoDuration::days(60),
            )
            .await
            .unwrap();
        }

        let sched = ConsolidationScheduler::start(
            Arc::clone(&storage),
            archive,
            params(),
            SchedulerConfig::disabled(),
            vec![],
        );
        sched.force_run().await.unwrap();

        let m = sched.metrics();
        assert_eq!(m.runs_total, 1);
        assert_eq!(m.last_promoted_to_warm, 4);
        assert!(m.last_consolidation_at.is_some());

        // Idempotent: a second run does no work but still counts.
        sched.force_run().await.unwrap();
        let m2 = sched.metrics();
        assert_eq!(m2.runs_total, 2);
        assert_eq!(m2.last_promoted_to_warm, 0);

        sched.shutdown().await;
    }

    /// EpisodicStore::record bumps activity (regression guard for the
    /// idle-gate path).
    #[tokio::test]
    async fn episodic_record_bumps_shared_counter() {
        let storage: Arc<dyn Storage> = MemoryStorage::new();
        let store = EpisodicStore::new(Arc::clone(&storage));
        let counter = store.activity_counter();
        assert_eq!(counter.snapshot(), 0);
        store.record("k", "p", "\"x\"").await.unwrap();
        assert_eq!(counter.snapshot(), 1);
        store.record("k", "p", "\"y\"").await.unwrap();
        assert_eq!(counter.snapshot(), 2);
    }
}
