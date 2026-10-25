//! The `classify` command (stub).

use std::path::Path;

/// Classifies one `.eml` file without touching any mailbox.
pub async fn run(_config: &Path, _file: &Path) -> anyhow::Result<()> {
    anyhow::bail!("`classify` is not implemented yet")
}
