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
use crate::memory::episodic::EpisodicStore;
use crate::memory::procedural::ProceduralStore;
use crate::memory::semantic::{SemanticStore, SnapshotConfig};
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

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(MnemeError::Io)?;
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

    let result = runtime
        .block_on(async_main(
            storage_dyn,
            Arc::clone(&semantic),
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        ))
        .map_err(|e| MnemeError::Mcp(format!("server exited with error: {e}")));

    // Ask the scheduler to write a final snapshot before tearing
    // down the runtime — saves the next boot a full WAL replay.
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

async fn async_main(
    storage: Arc<dyn Storage>,
    semantic: Arc<SemanticStore>,
    procedural: Arc<ProceduralStore>,
    episodic: Arc<EpisodicStore>,
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
        Arc::new(ToolRegistry::defaults(
            semantic,
            Arc::clone(&procedural),
            Arc::clone(&episodic),
        )),
        Arc::new(ResourceRegistry::defaults(procedural, episodic)),
        storage,
    );

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
