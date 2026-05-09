//! `mneme auth` — daemon auth-token administration (ADR-0012 D3 / D4).
//!
//! Today: `rotate` (regenerate the token file) and `show-path`
//! (print the path agents reference). Daemon-side enforcement on
//! the connect handshake lands in the next A.M4 commit.

use crate::daemon::auth;
use crate::storage::layout;
use crate::{MnemeError, Result};

pub fn rotate() -> Result<()> {
    let root = layout::default_root()
        .ok_or_else(|| MnemeError::Config("could not resolve ~/.mneme".into()))?;
    let path = auth::rotate_token(&root).map_err(|e| MnemeError::Config(e.to_string()))?;
    eprintln!("rotated daemon auth token at {}", path.display());
    eprintln!(
        "agents that reference the file by path keep working; \
         existing daemon connections stay valid (token check \
         fires only at the next handshake)."
    );
    Ok(())
}

pub fn show_path() -> Result<()> {
    let root = layout::default_root()
        .ok_or_else(|| MnemeError::Config("could not resolve ~/.mneme".into()))?;
    let path = auth::token_path(&root);
    println!("{}", path.display());
    Ok(())
}
