//! The `run` command (stub).

use std::path::Path;

/// Watches the accounts, flags new mail and sends the daily digest.
pub async fn run(_config: &Path) -> anyhow::Result<()> {
    // `anyhow::bail!` returns early with an error; `main` prints it and exits non-zero.
    anyhow::bail!("`run` is not implemented yet")
}
