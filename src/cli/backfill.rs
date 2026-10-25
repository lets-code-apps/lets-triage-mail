//! The `backfill` command (stub).

use std::path::Path;

/// Classifies and flags existing inbox mail from the last `days` days.
///
/// `account` limits the run to one configured account; `None` means all of them.
pub async fn run(_config: &Path, _days: u32, _account: Option<&str>) -> anyhow::Result<()> {
    anyhow::bail!("`backfill` is not implemented yet")
}
