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

pub fn execute() -> Result<()> {
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
) -> anyhow::Result<()> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        protocol = crate::mcp::PROTOCOL_VERSION,
        embed_dim = semantic.dim(),
        "mneme MCP server starting on stdio"
    );

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let transport = StdioTransport::new(stdin, stdout);
    let mut server = Server::new(
        transport,
        Arc::new(ToolRegistry::defaults_with_schedulers(
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
            Arc::clone(&storage),
            cold.clone(),
            schema_version,
            Some(Arc::clone(&consolidation)),
            Some(Arc::clone(&checkpoint_scheduler)),
            Arc::clone(&scope_state),
        )),
        Arc::new(ResourceRegistry::defaults_with_schedulers(
            semantic,
            procedural,
            episodic,
            orchestrator,
            cold,
            schema_version,
            auto_context_budget,
            Some(consolidation),
            Some(Arc::clone(&checkpoint_scheduler)),
            Some(Arc::clone(&active_session)),
            Some(sessions_dir),
            Some(scope_state),
        )),
        storage,
    )
    .with_session(active_session, checkpoint_scheduler);

    tokio::select! {
        result = server.run() => result?,
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal received");
        }
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
