//! The `digest` command (stub).

use std::path::Path;

/// Builds a digest immediately. With `dry` it is printed; otherwise it is appended to the
/// mailbox. `account` limits the run to one configured account.
pub async fn run(_config: &Path, _account: Option<&str>, _dry: bool) -> anyhow::Result<()> {
    anyhow::bail!("`digest` is not implemented yet")
}
