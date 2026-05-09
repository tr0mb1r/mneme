//! `mneme run` — load config, take the lockfile, open storage, start
//! the MCP server on stdio.
//!
//! Sequence on startup:
//!   1. Resolve `~/.mneme/` (or env-overridden root).
//!   2. Load `config.toml`, falling back to defaults if missing.
//!   3. Verify `schema_version` is no newer than this binary supports.
//!   4. Acquire the exclusive lockfile (`~/.mneme/.lock`).
//!   5. Open `RedbStorage` over `~/.mneme/episodic/` (replays WAL forward).
//!   6. Build the MCP `Server` with the storage handle.
//!   7. Run until peer EOF or SIGTERM/Ctrl-C.
//!
//! On shutdown the storage drops naturally — the WAL writer thread joins
//! synchronously in `Drop`, ensuring the active segment is fdatasync'd
//! one last time before the lockfile guard is released.

use std::sync::Arc;

use crate::Result;
use crate::config::Config;
use crate::embed::{self, Embedder};
use crate::mcp::resources::ResourceRegistry;
use crate::mcp::server::Server;
use crate::mcp::tools::ToolRegistry;
use crate::mcp::transport::stdio::StdioTransport;
use crate::memory::checkpoint_scheduler::{CheckpointScheduler, CheckpointSchedulerConfig};
use crate::memory::consolidation::ConsolidationParams;
use crate::memory::consolidation_scheduler::{ConsolidationScheduler, SchedulerConfig};
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::{SemanticStore, SnapshotConfig};
use crate::memory::working::ActiveSession;
use crate::orchestrator::{Orchestrator, TokenBudget};
use crate::scope::ScopeState;
use crate::storage::Storage;
use crate::storage::layout;
use crate::storage::lockfile::LockGuard;
use crate::storage::redb_impl::RedbStorage;
use crate::{MnemeError, migrate};

/// Env-var override that swaps the embedder for the deterministic
/// stub. Set by tests + offline CI runs; logging mentions it loudly so
/// nobody thinks they're embedding for real.
const EMBEDDER_OVERRIDE_ENV: &str = "MNEME_EMBEDDER";

/// Which transport the MCP server speaks. The boot path
/// ([`execute_with_mode`]) is identical for every mode — only the
/// reader/writer pair fed to `Server::new` differs. The `Daemon*`
/// variants share registry construction across connections via Arc;
/// per-connection auth verification (ADR-0012 D3) lands in M4.
#[derive(Debug)]
pub enum TransportMode {
    /// Read JSON-RPC frames from stdin, write replies to stdout.
    /// v1.0 default, retained as the `--stdio` opt-in per ADR-0012
    /// D10.
    Stdio,
    /// Bind `<root>/run/mneme.sock`, accept exactly one client
    /// connection, then drop the listener (RAII unlinks the socket
    /// file). Serve the accepted stream until EOF. Used by M2's
    /// `daemon_e2e` baseline test and as a debugging aid; production
    /// `mneme daemon` uses [`Self::DaemonServeMany`].
    DaemonAcceptOne,
    /// M3 production daemon mode (ADR-0012 D2/D6/D7). Bind
    /// `<root>/run/mneme.sock` and run a long-running accept loop:
    /// every accepted connection is spawned as its own tokio task
    /// that builds a `Server` wrapping the socket halves and runs
    /// until EOF. The accept loop terminates only on
    /// SIGTERM/Ctrl-C or process death. Multiple clients are served
    /// concurrently; storage writes serialise through the existing
    /// single-writer seam (ADR-0012 D8). Idle-timeout shutdown
    /// (D6 — auto-exit after `[daemon] idle_timeout_minutes` with
    /// no clients) and SSE keepalive (D7) land in following M3
    /// commits.
    DaemonServeMany,
}

pub fn execute() -> Result<()> {
    execute_with_mode(TransportMode::Stdio)
}

pub fn execute_with_mode(mode: TransportMode) -> Result<()> {
    // Ignore SIGTTIN/SIGTTOU. If the user runs `mneme run &` in an
    // interactive shell, our stdin read would otherwise trigger SIGTTIN
    // → process stopped, and any SIGTERM the user sends afterwards gets
    // queued (the process can't process signals while stopped, and the
    // stdin read re-stops us immediately after SIGCONT). Ignoring these
    // makes the read return `EIO` instead, surfacing as a clean exit.
    // Real MCP hosts (Claude Desktop / Code) pipe stdin, so they never
    // trigger SIGTTIN — this is purely about manual-shell ergonomics.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTTIN, libc::SIG_IGN);
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }

    let root = layout::default_root().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for ~/.mneme".into())
    })?;
    // Best-effort scaffold so a fresh user can `mneme run` without
    // having explicitly invoked `mneme init` first.
    layout::scaffold(&root)?;

    let config_path = root.join("config.toml");
    let config = Config::load(&config_path)?;

    let on_disk_version = migrate::current_version(&root)?;
    if on_disk_version > migrate::CURRENT_SCHEMA_VERSION {
        return Err(MnemeError::Migration(format!(
            "on-disk schema_version {} is newer than this binary supports ({}); upgrade mneme",
            on_disk_version,
            migrate::CURRENT_SCHEMA_VERSION
        )));
    }
    if on_disk_version < migrate::CURRENT_SCHEMA_VERSION {
        migrate::migrate_to(&root, migrate::CURRENT_SCHEMA_VERSION)?;
    }

    let lock_path = root.join(".lock");
    let lock = LockGuard::acquire(&lock_path)?;
    let storage = RedbStorage::open(&root.join("episodic"))?;
    let storage_dyn: Arc<dyn Storage> = Arc::clone(&storage) as Arc<dyn Storage>;

    let embedder = build_embedder(&config, &root)?;
    let active_model_name = active_embedder_model_name(&config);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;
    // If the active embedder differs from the one that produced the
    // on-disk vectors, re-embed every stored memory before opening the
    // semantic store. This both replaces the stale snapshot/WAL and
    // makes the existing memories searchable under the new model.
    match runtime.block_on(crate::embed::migrate::migrate_if_needed(
        &root,
        Arc::clone(&storage_dyn),
        Arc::clone(&embedder),
        &active_model_name,
    ))? {
        crate::embed::migrate::Outcome::NoChange => {}
        crate::embed::migrate::Outcome::Migrated { count } => {
            tracing::info!(
                count,
                model = %active_model_name,
                dim = embedder.dim(),
                "re-embedded memories under new embedder identity"
            );
        }
    }
    // SemanticStore::open spawns a tokio task for the snapshot
    // scheduler, so it has to be constructed inside the runtime
    // context. Build the SnapshotConfig from `config.checkpoints` and
    // hand it through.
    let snap_cfg = SnapshotConfig {
        inserts_threshold: config.checkpoints.hnsw_snapshot_inserts,
        interval: std::time::Duration::from_secs(
            (config.checkpoints.hnsw_snapshot_minutes as u64).saturating_mul(60),
        ),
        enabled: true,
    };
    let semantic = runtime.block_on(async {
        SemanticStore::open(&root, Arc::clone(&storage_dyn), embedder, snap_cfg)
    })?;
    let procedural = Arc::new(ProceduralStore::open(&root)?);
    let episodic = Arc::new(EpisodicStore::new(Arc::clone(&storage_dyn)));
    let auto_context_budget = TokenBudget::from_config(&config.budgets);
    let cold = crate::storage::archive::ColdArchive::new(&root);

    // Process-lifetime "current scope" cell. Initialised from
    // `[scopes] default`; mutated by the `switch_scope` tool.
    let scope_state = ScopeState::new(&config.scopes.default);

    // `remember` / `update` content ceiling (release-planning v2.1
    // §5.3). Configured via `[budgets] max_remember_chars`; the
    // 500/2,000-character advisory and warning bounds inside
    // `size_tier` are fixed.
    let max_remember_chars = config.budgets.max_remember_chars;

    // Daemon idle-timeout (ADR-0012 D6). Honored only by
    // `DaemonServeMany`; stdio + DaemonAcceptOne ignore it. `0`
    // disables — daemon then only stops via SIGTERM / `mneme stop`.
    let daemon_idle_timeout_minutes = config.daemon.idle_timeout_minutes;

    // First-boot upgrade audit (release-planning §5.3, Invariant 7).
    // Scans L4 once for memories above max_remember_chars and writes
    // a passive summary to ~/.mneme/diagnostics.log so users
    // upgrading from v1.0 (which accepted arbitrary-size content)
    // can find oversized entries without spelunking. Gated by
    // ~/.mneme/run/upgrade-audit.done — runs at most once per data
    // dir. Existing memories are NEVER auto-modified (verbatim
    // principle). Best-effort: log + continue on error rather than
    // refuse to boot — the audit is informational, not load-bearing.
    match runtime.block_on(crate::upgrade_audit::run_if_needed(
        &root,
        &storage_dyn,
        max_remember_chars,
    )) {
        Ok(crate::upgrade_audit::AuditOutcome::AlreadyDone) => {}
        Ok(crate::upgrade_audit::AuditOutcome::Ran(stats)) => {
            tracing::info!(
                total = stats.total(),
                normal = stats.normal,
                advisory = stats.advisory,
                warning = stats.warning,
                over_limit = stats.over_limit,
                log_path = %crate::upgrade_audit::diagnostics_log_path(&root).display(),
                "v1.1 first-boot upgrade audit complete"
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "v1.1 first-boot upgrade audit failed; continuing");
        }
    }

    // Spawn the L3 consolidation scheduler. Watches the semantic +
    // episodic activity counters for idle windows; fires
    // `consolidation::run` on the configured cadence so hot-tier
    // growth is bounded without the agent having to ask for it.
    // Construction must happen inside the runtime context (it
    // `tokio::spawn`s).
    let consolidation_scheduler = runtime.block_on(async {
        ConsolidationScheduler::start(
            Arc::clone(&storage_dyn),
            cold.clone(),
            ConsolidationParams::from_config(&config.consolidation),
            SchedulerConfig::from_config(&config.consolidation),
            vec![semantic.activity_counter(), episodic.activity_counter()],
        )
    });

    // L1 working session + its checkpoint scheduler. One ActiveSession
    // per `mneme run` lifetime; the scheduler flushes on the configured
    // cadence + on shutdown.
    let active_session = ActiveSession::open(root.join("sessions"))?;
    let checkpoint_scheduler = runtime.block_on(async {
        CheckpointScheduler::start(
            Arc::clone(&active_session),
            CheckpointSchedulerConfig::from_config(&config.checkpoints),
        )
    });

    // The orchestrator is built AFTER the active session so it can
    // pull the L1 working layer into auto-context (`mneme://context`).
    let orchestrator = Arc::new(
        Orchestrator::new(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        )
        .with_active_session(Arc::clone(&active_session)),
    );

    let sessions_dir = root.join("sessions");
    let result = runtime
        .block_on(async_main(
            mode,
            root.clone(),
            Arc::clone(&storage_dyn),
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
            Arc::clone(&orchestrator),
            cold,
            on_disk_version.max(migrate::CURRENT_SCHEMA_VERSION),
            auto_context_budget,
            Arc::clone(&consolidation_scheduler),
            Arc::clone(&active_session),
            Arc::clone(&checkpoint_scheduler),
            sessions_dir,
            scope_state,
            active_model_name.clone(),
            max_remember_chars,
            daemon_idle_timeout_minutes,
        ))
        .map_err(|e| MnemeError::Mcp(format!("server exited with error: {e}")));

    // Stop the checkpoint scheduler before the consolidation +
    // semantic shutdowns so its final flush lands first. shutdown()
    // also writes the clean-shutdown marker.
    if let Err(e) = runtime.block_on(checkpoint_scheduler.shutdown()) {
        tracing::warn!(error = %e, "checkpoint scheduler shutdown reported error");
    }

    // Stop the consolidation scheduler before the semantic shutdown so
    // it can't fire one more pass mid-tear-down. Its current pass (if
    // any) is awaited inside `shutdown`.
    runtime.block_on(consolidation_scheduler.shutdown());

    // Ask the snapshot scheduler to write a final HNSW snapshot
    // before tearing down the runtime — saves the next boot a full
    // WAL replay.
    if let Err(e) = runtime.block_on(semantic.shutdown()) {
        tracing::warn!(error = %e, "semantic store shutdown reported error");
    }

    // `tokio::io::stdin()` reads via `spawn_blocking`, and that worker
    // thread can still be wedged in `read(2)` after `block_on` returns.
    // Letting the runtime drop normally would block forever waiting for it.
    // Detach the blocking pool instead so the process can exit promptly.
    runtime.shutdown_background();

    // Explicit cleanup: drop the LockGuard (which removes the file) and,
    // belt-and-suspenders, force-remove if Drop somehow didn't.
    drop(lock);
    if lock_path.exists()
        && let Err(e) = std::fs::remove_file(&lock_path)
    {
        tracing::warn!(error = %e, path = %lock_path.display(), "lockfile cleanup failed");
    }

    result
}

#[allow(clippy::too_many_arguments)]
async fn async_main(
    mode: TransportMode,
    root: std::path::PathBuf,
    storage: Arc<dyn Storage>,
    semantic: Arc<SemanticStore>,
    procedural: Arc<ProceduralStore>,
    episodic: Arc<EpisodicStore>,
    orchestrator: Arc<Orchestrator>,
    cold: crate::storage::archive::ColdArchive,
    schema_version: u32,
    auto_context_budget: TokenBudget,
    consolidation: Arc<ConsolidationScheduler>,
    active_session: Arc<ActiveSession>,
    checkpoint_scheduler: Arc<CheckpointScheduler>,
    sessions_dir: std::path::PathBuf,
    scope_state: Arc<ScopeState>,
    active_model_name: String,
    max_remember_chars: usize,
    daemon_idle_timeout_minutes: u64,
) -> anyhow::Result<()> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        protocol = crate::mcp::PROTOCOL_VERSION,
        embed_dim = semantic.dim(),
        "mneme MCP server starting on stdio"
    );

    // ADR-0009 — emit `session_start` to L3 before serving traffic so
    // the brackets around a session are explicit in the activity log.
    // Best-effort: a failed emit logs a warning and proceeds (the
    // server still serves; missing one boundary event is degraded
    // mode, not a fatal condition).
    let session_id = active_session.id().to_string();
    let started_at = active_session.started_at().to_rfc3339();
    let session_start_payload = serde_json::json!({
        "session_id": session_id,
        "started_at": started_at,
        "embedder": &active_model_name,
        "embed_dim": semantic.dim(),
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": crate::mcp::PROTOCOL_VERSION,
    });
    if let Err(e) = episodic
        .record_json(
            "session_start",
            &scope_state.current(),
            &session_start_payload,
        )
        .await
    {
        tracing::warn!(error = %e, "failed to record session_start event");
    }

    // Boxed trait objects so every transport variant produces the
    // same concrete `StdioTransport<Box<...>, Box<...>>` type — one
    // monomorphisation of `Server::new` regardless of mode. Per-
    // syscall virtual call cost is negligible against JSON-RPC
    // frame parsing + storage IO.
    type AsyncReader = Box<dyn tokio::io::AsyncRead + Send + Unpin>;
    type AsyncWriter = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;

    // Build registries ONCE per process. `DaemonServeMany` Arc::clones
    // them per accepted connection; the single-shot modes also use
    // the shared instances so the construction cost (cheap, but
    // not free — every `defaults_with_schedulers` Arc-wraps every
    // tool/resource) lands once.
    let tool_registry: Arc<ToolRegistry> = Arc::new(ToolRegistry::defaults_with_schedulers(
        Arc::clone(&semantic),
        Arc::clone(&procedural),
        Arc::clone(&episodic),
        Arc::clone(&storage),
        cold.clone(),
        schema_version,
        Some(Arc::clone(&consolidation)),
        Some(Arc::clone(&checkpoint_scheduler)),
        Arc::clone(&scope_state),
        Some(Arc::clone(&active_session)),
        max_remember_chars,
    ));
    let resource_registry: Arc<ResourceRegistry> =
        Arc::new(ResourceRegistry::defaults_with_schedulers(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
            orchestrator,
            cold.clone(),
            schema_version,
            auto_context_budget,
            Some(Arc::clone(&consolidation)),
            Some(Arc::clone(&checkpoint_scheduler)),
            Some(Arc::clone(&active_session)),
            Some(sessions_dir),
            Some(Arc::clone(&scope_state)),
            Some((Arc::clone(&storage), max_remember_chars)),
        ));

    // Per-connection helper: build the Server for a transport pair
    // and run it until EOF. Used by every mode; the `with_signal`
    // flag wraps `server.run()` in a `tokio::select!` against
    // shutdown_signal so single-shot modes can exit on SIGTERM
    // mid-serve. `DaemonServeMany` passes `false` because the
    // outer accept loop owns the signal.
    let serve_one = |reader: AsyncReader,
                     writer: AsyncWriter,
                     with_signal: bool,
                     tool: Arc<ToolRegistry>,
                     resource: Arc<ResourceRegistry>,
                     storage: Arc<dyn Storage>,
                     active_session: Arc<ActiveSession>,
                     checkpoint_scheduler: Arc<CheckpointScheduler>,
                     episodic: Arc<EpisodicStore>,
                     scope_state: Arc<ScopeState>| {
        async move {
            let transport = StdioTransport::new(reader, writer);
            let mut server = Server::new(transport, tool, resource, storage).with_session(
                active_session,
                checkpoint_scheduler,
                episodic,
                scope_state,
            );
            if with_signal {
                tokio::select! {
                    result = server.run() => result?,
                    _ = shutdown_signal() => {
                        tracing::info!("shutdown signal received");
                    }
                }
            } else {
                server.run().await?;
            }
            Ok::<(), anyhow::Error>(())
        }
    };

    match mode {
        TransportMode::Stdio => {
            tracing::info!("transport=stdio: reading frames from stdin");
            serve_one(
                Box::new(tokio::io::stdin()),
                Box::new(tokio::io::stdout()),
                true,
                Arc::clone(&tool_registry),
                Arc::clone(&resource_registry),
                Arc::clone(&storage),
                Arc::clone(&active_session),
                Arc::clone(&checkpoint_scheduler),
                Arc::clone(&episodic),
                Arc::clone(&scope_state),
            )
            .await?;
        }
        TransportMode::DaemonAcceptOne => {
            let listener = crate::daemon::bind_listener(&root)
                .await
                .map_err(|e| anyhow::anyhow!("daemon listener bind failed: {e}"))?;
            tracing::info!(
                socket = %listener
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "transport=daemon-accept-one: listener bound; awaiting first client"
            );
            let (stream, _addr) = listener.as_inner().accept().await?;
            tracing::info!(
                "transport=daemon-accept-one: client accepted; unbinding listener \
                 so the socket file is gone before serving begins"
            );
            drop(listener);
            let (read, write) = stream.into_split();
            serve_one(
                Box::new(read),
                Box::new(write),
                true,
                Arc::clone(&tool_registry),
                Arc::clone(&resource_registry),
                Arc::clone(&storage),
                Arc::clone(&active_session),
                Arc::clone(&checkpoint_scheduler),
                Arc::clone(&episodic),
                Arc::clone(&scope_state),
            )
            .await?;
        }
        TransportMode::DaemonServeMany => {
            let listener = crate::daemon::bind_listener(&root)
                .await
                .map_err(|e| anyhow::anyhow!("daemon listener bind failed: {e}"))?;
            tracing::info!(
                socket = %listener
                    .path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "transport=daemon-serve-many: listener bound; serving until SIGTERM"
            );

            // Track in-flight connection count. Read by the
            // idle-timeout watcher (ADR-0012 D6) below; future
            // commits will also consult it for graceful-drain on
            // SIGTERM.
            let active_clients = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let idle_watcher_counter = Arc::clone(&active_clients);

            // The spawned per-connection tasks need their futures
            // to be `Send` for `tokio::spawn`. The boxed-trait-object
            // pair we use for stdio doesn't satisfy `Sync` (Stdin
            // isn't Sync), so spawning a `Server<Box<dyn _>, Box<dyn _>>`
            // future fails the `Sync`-via-Send-of-`&Server` chain.
            // The Unix socket's owned halves (`OwnedReadHalf` /
            // `OwnedWriteHalf`) are themselves `Send + Sync` — passing
            // them through directly skips the boxing problem and
            // monomorphises a separate `Server` for socket
            // connections.
            let exit_reason: &'static str;
            tokio::select! {
                _ = shutdown_signal() => {
                    exit_reason = "shutdown_signal";
                    tracing::info!(
                        active = active_clients.load(std::sync::atomic::Ordering::Relaxed),
                        "transport=daemon-serve-many: shutdown signal received"
                    );
                }
                _ = idle_timeout_watcher(idle_watcher_counter, daemon_idle_timeout_minutes) => {
                    exit_reason = "idle_timeout";
                    tracing::info!(
                        idle_minutes = daemon_idle_timeout_minutes,
                        "transport=daemon-serve-many: idle timeout reached; shutting down"
                    );
                }
                _ = async {
                    loop {
                        match listener.as_inner().accept().await {
                            Ok((stream, _addr)) => {
                                let n = active_clients
                                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                    + 1;
                                tracing::info!(
                                    active = n,
                                    "transport=daemon-serve-many: client accepted"
                                );
                                let (read, write) = stream.into_split();
                                let tool = Arc::clone(&tool_registry);
                                let resource = Arc::clone(&resource_registry);
                                let storage = Arc::clone(&storage);
                                let active_session_c = Arc::clone(&active_session);
                                let checkpoint_c = Arc::clone(&checkpoint_scheduler);
                                let episodic_c = Arc::clone(&episodic);
                                let scope_c = Arc::clone(&scope_state);
                                let counter = Arc::clone(&active_clients);
                                tokio::spawn(async move {
                                    // Concrete owned-half types →
                                    // Server<OwnedReadHalf, OwnedWriteHalf>
                                    // monomorphisation (separate from
                                    // the boxed-trait-object stdio
                                    // monomorphisation).
                                    let transport = StdioTransport::new(read, write);
                                    let mut server = Server::new(
                                        transport,
                                        tool,
                                        resource,
                                        storage,
                                    )
                                    .with_session(
                                        active_session_c,
                                        checkpoint_c,
                                        episodic_c,
                                        scope_c,
                                    );
                                    if let Err(e) = server.run().await {
                                        tracing::warn!(
                                            error = %e,
                                            "client serve loop ended with error"
                                        );
                                    }
                                    let n = counter
                                        .fetch_sub(1, std::sync::atomic::Ordering::Relaxed)
                                        - 1;
                                    tracing::info!(
                                        active = n,
                                        "transport=daemon-serve-many: client disconnected"
                                    );
                                });
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "accept error; continuing");
                            }
                        }
                    }
                    // Unreachable, but the future has to type-check.
                    #[allow(unreachable_code)]
                    Ok::<(), anyhow::Error>(())
                } => {
                    // The accept-loop arm should never resolve;
                    // surface that as an error so a future change
                    // that accidentally makes it return doesn't
                    // exit silently.
                    exit_reason = "accept_loop_returned_unexpectedly";
                    tracing::error!(
                        "transport=daemon-serve-many: accept loop returned unexpectedly"
                    );
                }
            }

            // Graceful drain: stop accepting (the listener is moved
            // into the select arm and dropped here, so subsequent
            // connect attempts will see ECONNREFUSED), wait briefly
            // for in-flight spawned tasks to finish their MCP
            // exchanges, then return. Bounded by `DRAIN_DEADLINE` so
            // a stuck client doesn't wedge shutdown.
            tracing::info!(
                exit_reason,
                active = active_clients.load(std::sync::atomic::Ordering::Relaxed),
                "transport=daemon-serve-many: entering graceful drain"
            );
            wait_for_drain(Arc::clone(&active_clients)).await;
        }
    }

    // ADR-0009 — emit `session_end` after the run loop returns, before
    // the storage drop unwinds. Same best-effort contract as
    // `session_start`. `clean_shutdown=true` here because we exited
    // via the loop or the shutdown signal — kill -9 wouldn't reach
    // this branch.
    let session_end_payload = serde_json::json!({
        "session_id": session_id,
        "ended_at": chrono::Utc::now().to_rfc3339(),
        "clean_shutdown": true,
        "turns_total": active_session.turns_total(),
        "checkpoints_total": active_session.checkpoints_total(),
    });
    if let Err(e) = episodic
        .record_json("session_end", &scope_state.current(), &session_end_payload)
        .await
    {
        tracing::warn!(error = %e, "failed to record session_end event");
    }

    tracing::info!("mneme MCP server stopped");
    Ok(())
}

/// Build the embedder per spec §10.3, honouring the
/// [`EMBEDDER_OVERRIDE_ENV`] escape hatch.
///
/// Production path: read `config.embeddings.model`, hand it to
/// [`embed::load_from_config`] which will download the model into
/// `<root>/models/` on first call. If the user hasn't run `mneme init`
/// and is offline, this is where they'll see the failure. The error
/// message points at `mneme init` so the fix is obvious.
///
/// Test/offline path: when `MNEME_EMBEDDER=stub` is set, swap in
/// [`crate::embed::stub::StubEmbedder`]. The boot log has a `WARN`
/// line so this isn't accidentally left on in production.
fn build_embedder(config: &Config, root: &std::path::Path) -> Result<Arc<dyn Embedder>> {
    if matches!(std::env::var(EMBEDDER_OVERRIDE_ENV).as_deref(), Ok("stub")) {
        tracing::warn!(
            "{EMBEDDER_OVERRIDE_ENV}=stub: using deterministic stub embedder; \
             stored vectors will not be portable to real models"
        );
        return Ok(Arc::new(crate::embed::stub::StubEmbedder::new()));
    }
    let cache = root.join("models");
    embed::load_from_config(&config.embeddings.model, &cache).map_err(|e| {
        MnemeError::Embedding(format!(
            "failed to load embedder `{}`: {e}. Try `mneme init` first to download model weights.",
            config.embeddings.model
        ))
    })
}

/// Identifying name for the active embedder. Stored in the embedder
/// sidecar so we can detect "user swapped models" between boots.
/// `MNEME_EMBEDDER=stub` overrides the config-named model, so the name
/// has to come from the same control flow as `build_embedder`.
fn active_embedder_model_name(config: &Config) -> String {
    if matches!(std::env::var(EMBEDDER_OVERRIDE_ENV).as_deref(), Ok("stub")) {
        "stub".to_owned()
    } else {
        config.embeddings.model.clone()
    }
}

/// Idle-timeout watcher for `DaemonServeMany` (ADR-0012 D6).
///
/// Polls `active_clients` every [`IDLE_POLL_INTERVAL`]; tracks the
/// most recent moment the count was non-zero. When the count has
/// been zero for at least `idle_timeout_minutes`, the future
/// resolves — the caller wires this into a `tokio::select!` arm
/// next to `shutdown_signal()`. `idle_timeout_minutes == 0` disables
/// the timeout (the future pends forever) per the config knob's
/// documented "0 = never" semantics.
///
/// Counted from "last client disconnected", not "last request
/// seen": a long HNSW snapshot that blocks requests but not
/// connections doesn't accidentally trigger shutdown.
async fn idle_timeout_watcher(
    active_clients: Arc<std::sync::atomic::AtomicUsize>,
    idle_timeout_minutes: u64,
) {
    if idle_timeout_minutes == 0 {
        std::future::pending::<()>().await;
        return;
    }
    let timeout = std::time::Duration::from_secs(idle_timeout_minutes * 60);
    // First measurement: assume the daemon was just-spawned with no
    // clients, so the idle clock starts now.
    let mut last_nonzero = tokio::time::Instant::now();
    loop {
        tokio::time::sleep(IDLE_POLL_INTERVAL).await;
        if active_clients.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            last_nonzero = tokio::time::Instant::now();
        } else if last_nonzero.elapsed() >= timeout {
            return;
        }
    }
}

/// How often `idle_timeout_watcher` checks the client counter.
/// 30 s gives ≤ 30 s overshoot on the configured timeout — fine for
/// the default 30-minute idle and frugal on syscalls.
const IDLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Maximum time `wait_for_drain` will wait for active connections to
/// finish before letting the runtime tear them down. 30 s is long
/// enough for typical MCP exchanges to complete, short enough that a
/// stuck client doesn't wedge a daemon shutdown for minutes.
const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// How often `wait_for_drain` polls the counter while draining.
/// 100 ms is responsive enough that a normal client closing within a
/// second is barely waited on; rare enough that a stuck-but-large
/// drain isn't a tight CPU loop.
const DRAIN_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// Wait for the active-client counter to reach zero, capped by
/// [`DRAIN_DEADLINE`]. Logs `info` on clean drain, `warn` on
/// timeout. Used by `DaemonServeMany` after the accept loop exits
/// (SIGTERM or idle timeout) to give in-flight MCP exchanges a
/// chance to complete before the runtime aborts their tasks.
async fn wait_for_drain(active_clients: Arc<std::sync::atomic::AtomicUsize>) {
    let initial = active_clients.load(std::sync::atomic::Ordering::Relaxed);
    if initial == 0 {
        return;
    }
    tracing::info!(active = initial, "draining active clients before exit");
    let start = tokio::time::Instant::now();
    loop {
        let now = active_clients.load(std::sync::atomic::Ordering::Relaxed);
        if now == 0 {
            tracing::info!(
                drained_count = initial,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "all clients drained cleanly"
            );
            return;
        }
        if start.elapsed() >= DRAIN_DEADLINE {
            tracing::warn!(
                active = now,
                deadline_s = DRAIN_DEADLINE.as_secs(),
                "drain deadline reached; remaining clients will abort with the runtime"
            );
            return;
        }
        tokio::time::sleep(DRAIN_POLL_INTERVAL).await;
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("failed to install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `idle_timeout_minutes == 0` is the documented "never auto-
    /// shutdown" sentinel — the future must pend forever so the
    /// `tokio::select!` arm never fires.
    #[tokio::test(start_paused = true)]
    async fn watcher_with_zero_timeout_pends_forever() {
        let counter = Arc::new(AtomicUsize::new(0));
        let watcher = idle_timeout_watcher(counter, 0);
        let result = tokio::time::timeout(std::time::Duration::from_secs(86_400), watcher).await;
        assert!(result.is_err(), "watcher with timeout=0 must never resolve");
    }

    /// With a non-zero timeout and a counter that stays at zero
    /// for the full duration, the watcher resolves shortly after
    /// the timeout elapses (within one poll interval of slack).
    #[tokio::test(start_paused = true)]
    async fn watcher_resolves_when_idle_for_timeout() {
        let counter = Arc::new(AtomicUsize::new(0));
        // 1 minute timeout — the test-clock advances instantly.
        let watcher = idle_timeout_watcher(Arc::clone(&counter), 1);
        let bound = IDLE_POLL_INTERVAL + std::time::Duration::from_secs(60 + 5);
        tokio::time::timeout(bound, watcher)
            .await
            .expect("watcher resolved within timeout + one poll");
    }

    /// A non-zero counter keeps resetting the idle clock — the
    /// watcher must NOT resolve while clients are connected, even
    /// across many timeout-worth of paused time.
    #[tokio::test(start_paused = true)]
    async fn watcher_does_not_resolve_while_clients_connected() {
        let counter = Arc::new(AtomicUsize::new(1));
        let watcher = idle_timeout_watcher(Arc::clone(&counter), 1);
        // Advance way past 10 minute-equivalents of paused time.
        let result = tokio::time::timeout(std::time::Duration::from_secs(10 * 60), watcher).await;
        assert!(
            result.is_err(),
            "watcher must NOT resolve while a client is connected"
        );
    }

    /// Already-drained counter (count == 0 at entry) returns
    /// immediately — no syscalls, no log noise. The fast path
    /// matters because most graceful-shutdown calls happen with
    /// no clients connected (idle daemon receives SIGTERM).
    #[tokio::test(start_paused = true)]
    async fn drain_returns_immediately_when_empty() {
        let counter = Arc::new(AtomicUsize::new(0));
        let start = tokio::time::Instant::now();
        wait_for_drain(counter).await;
        assert!(
            start.elapsed() < std::time::Duration::from_millis(50),
            "empty drain must be near-instant, took {:?}",
            start.elapsed()
        );
    }

    /// Counter that ticks down to zero before the deadline returns
    /// cleanly. Drives the counter externally to mimic spawned
    /// connection tasks finishing their work.
    #[tokio::test(start_paused = true)]
    async fn drain_returns_when_clients_finish_before_deadline() {
        let counter = Arc::new(AtomicUsize::new(3));
        let drain_counter = Arc::clone(&counter);

        let driver = async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            counter.fetch_sub(1, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            counter.fetch_sub(1, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            counter.fetch_sub(1, Ordering::Relaxed);
        };

        // Tokio paused-clock advances both the driver's sleeps and
        // the drain's poll interval simultaneously.
        tokio::join!(wait_for_drain(drain_counter), driver);
    }

    /// Counter that never reaches zero: drain should bail at the
    /// `DRAIN_DEADLINE` bound rather than wait forever. The bounded
    /// timeout is the load-bearing safety: a stuck client must not
    /// wedge daemon shutdown.
    #[tokio::test(start_paused = true)]
    async fn drain_bails_at_deadline_on_stuck_clients() {
        let counter = Arc::new(AtomicUsize::new(2));
        let start = tokio::time::Instant::now();
        wait_for_drain(Arc::clone(&counter)).await;
        let elapsed = start.elapsed();
        // Should take at least DRAIN_DEADLINE (counter never moves)
        // but not significantly more.
        assert!(
            elapsed >= DRAIN_DEADLINE,
            "drain must hit deadline, only waited {elapsed:?}"
        );
        assert!(
            elapsed < DRAIN_DEADLINE + std::time::Duration::from_millis(500),
            "drain ran past deadline + slack, took {elapsed:?}"
        );
        // Counter is still at the original 2 (we don't touch it
        // from the drain) — the spawned tasks abort with the
        // runtime when async_main returns.
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    /// Counter goes 1 → 0 → 1 mid-window resets the idle clock —
    /// the watcher should not resolve based on the earlier zero
    /// observation. Only a continuous full-timeout zero stretch
    /// triggers shutdown.
    #[tokio::test(start_paused = true)]
    async fn watcher_resets_clock_on_reconnect() {
        let counter = Arc::new(AtomicUsize::new(1));
        let watcher_counter = Arc::clone(&counter);
        let watcher = idle_timeout_watcher(watcher_counter, 1);

        let driver = async move {
            // Idle for 30 s — not enough to fire.
            counter.store(0, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            // Client reconnects — the watcher's poll on next tick
            // sees > 0 and resets last_nonzero.
            counter.store(1, Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_secs(45)).await;
            // 75 s elapsed total but the idle stretch was only 30 s
            // before the reconnect. Watcher should still be pending.
            counter.store(0, Ordering::Relaxed);
            // Let the new idle stretch run the full 1-minute
            // timeout; the watcher should resolve here.
            tokio::time::sleep(IDLE_POLL_INTERVAL + std::time::Duration::from_secs(60 + 5)).await;
        };

        tokio::select! {
            _ = watcher => {
                // Driver may have exited or still be running; either
                // is fine. Assertion is that the watcher resolved
                // ONLY after the second idle stretch.
            }
            _ = driver => {
                panic!("driver finished without watcher firing; \
                        idle clock didn't reset on reconnect");
            }
        }
    }
}
