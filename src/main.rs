use clap::Parser;
use file_rotate::compression::Compression;
use file_rotate::suffix::AppendCount;
use file_rotate::{ContentLimit, FileRotate};
use mneme::cli::{Cli, dispatch};
use mneme::config::{Config, LoggingConfig};
use mneme::storage::layout;
use std::path::PathBuf;
use std::sync::OnceLock;
use tracing_appender::non_blocking;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry, fmt};

// Keeps the non-blocking writer's worker thread alive for the
// process lifetime. Dropping the guard shuts the worker down and
// drops in-flight log lines, so we park it here.
static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

fn main() -> anyhow::Result<()> {
    init_logging();
    tracing::debug!("mneme {} starting", env!("CARGO_PKG_VERSION"));
    let cli = Cli::parse();
    dispatch(cli).map_err(Into::into)
}

fn init_logging() {
    let logging = load_logging_config();

    let stderr_layer = fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter())
        .boxed();

    let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync>> = vec![stderr_layer];
    if let Some(file_layer) = build_file_layer(&logging) {
        layers.push(file_layer);
    }

    tracing_subscriber::registry().with(layers).init();
}

// Read `<root>/config.toml` for the `[logging]` section. Silently
// returns defaults on any failure — the binary must boot even if
// the config is missing/malformed (see also the silent-default
// behaviour of `Config::load` itself, which the troubleshooting
// docs flag as a known UX gap).
fn load_logging_config() -> LoggingConfig {
    let Some(root) = layout::default_root() else {
        return LoggingConfig::default();
    };
    Config::load(&root.join("config.toml"))
        .map(|c| c.logging)
        .unwrap_or_default()
}

// Build the rotating file-appender layer if the configured log
// path's parent directory is reachable. Returns `None` on any
// resolution / mkdir failure so the binary still boots with
// stderr-only logging instead of crashing.
fn build_file_layer(logging: &LoggingConfig) -> Option<Box<dyn Layer<Registry> + Send + Sync>> {
    // v1.0-shaped configs ship `file = ""` as a "use the default"
    // sentinel (the field was unread pre-v1.1.x logging fix); honor
    // that by falling back to the same path `LoggingConfig::default()`
    // would resolve. file-rotate panics on an empty path, so this
    // branch must run before `FileRotate::new` sees `path`.
    let path: PathBuf = if logging.file.as_os_str().is_empty() {
        LoggingConfig::default().file
    } else {
        logging.file.clone()
    };
    if path.as_os_str().is_empty() {
        return None;
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).ok()?;
    }

    // file-rotate honors max_size_mb (size-based rotation) and
    // max_files (retention count). Rotated files land alongside the
    // base file with `.1`, `.2`, ... suffixes. tracing-appender's
    // built-in rolling only does time-based rotation, which is why
    // we use file-rotate here.
    let max_bytes = (logging.max_size_mb as usize).saturating_mul(1024 * 1024);
    let max_files = logging.max_files.max(1) as usize;
    let rotator = FileRotate::new(
        path,
        AppendCount::new(max_files),
        ContentLimit::Bytes(max_bytes),
        Compression::None,
        None,
    );

    let (writer, guard) = non_blocking(rotator);
    let _ = LOG_GUARD.set(guard);
    Some(
        fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_filter(file_filter(&logging.level))
            .boxed(),
    )
}

// Stderr filter: honor MNEME_LOG if set, otherwise WARN+ for the
// world and INFO+ for the mneme crate. The rationale matches the
// pre-existing default — stderr is a low-noise stream the user
// looks at when something is wrong.
fn stderr_filter() -> EnvFilter {
    EnvFilter::try_from_env("MNEME_LOG").unwrap_or_else(|_| EnvFilter::new("warn,mneme=info"))
}

// File filter: honor MNEME_LOG, otherwise use `[logging] level`
// from config. The file is the post-hoc diagnostic surface, so it
// captures more than stderr by default.
fn file_filter(config_level: &str) -> EnvFilter {
    if let Ok(env) = EnvFilter::try_from_env("MNEME_LOG") {
        return env;
    }
    let level = config_level.trim().to_ascii_lowercase();
    let directive = match level.as_str() {
        "trace" => "trace",
        "debug" => "warn,mneme=debug",
        "info" | "" | "default" => "warn,mneme=info",
        "warn" | "warning" => "warn",
        "error" => "error",
        _ => "warn,mneme=info",
    };
    EnvFilter::new(directive)
}
