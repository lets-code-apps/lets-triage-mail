//! The `stats` command (stub).

use std::path::Path;

/// Shows what the service did in the last `days` days.
pub async fn run(_config: &Path, _days: u32) -> anyhow::Result<()> {
    anyhow::bail!("`stats` is not implemented yet")
}
