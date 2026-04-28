//! v0 → v1 migration: a no-op transformation that simply writes the
//! schema_version sentinel. Future v2 migrations will inherit this file
//! and convert data formats as needed.

use crate::Result;
use std::path::Path;

pub fn run(root: &Path) -> Result<()> {
    super::write_schema_version(root, 1)
}
