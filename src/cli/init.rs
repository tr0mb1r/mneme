//! `mneme init` — scaffold the on-disk layout, write a default
//! `config.toml`, and run the schema migration to the current binary's
//! version. Idempotent: rerunning on an existing install only fills in
//! missing pieces.

use crate::config::Config;
use crate::storage::layout;
use crate::{MnemeError, Result, migrate};
use std::path::Path;

pub fn execute() -> Result<()> {
    let root = layout::default_root().ok_or_else(|| {
        MnemeError::Config("could not resolve home directory for ~/.mneme".into())
    })?;
    init_at(&root)
}

pub fn init_at(root: &Path) -> Result<()> {
    layout::scaffold(root)?;
    migrate::migrate_to(root, migrate::CURRENT_SCHEMA_VERSION)?;
    let config_path = root.join("config.toml");
    if !config_path.exists() {
        Config::default().write(&config_path)?;
        eprintln!("wrote default config to {}", config_path.display());
    } else {
        eprintln!("config already present at {}", config_path.display());
    }
    eprintln!(
        "mneme initialized at {} (schema_version={})",
        root.display(),
        migrate::CURRENT_SCHEMA_VERSION
    );
    Ok(())
}
