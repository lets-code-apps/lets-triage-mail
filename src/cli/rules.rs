//! The `rules` command and its nested subcommands (stub).

use std::path::Path;

use clap::Subcommand;

/// What to do with the sender rules.
#[derive(Debug, Subcommand)]
pub enum RulesCommand {
    /// List all rules
    List,
    /// Always treat senders matching PATTERN as known
    Allow {
        /// An address or a domain such as `@example.com`
        pattern: String,
    },
    /// Never flag senders matching PATTERN as important
    Block {
        /// An address or a domain such as `@example.com`
        pattern: String,
    },
    /// Remove the rule for PATTERN
    Remove {
        /// The pattern exactly as it was added
        pattern: String,
    },
}

/// Lists, adds or removes sender rules.
pub async fn run(_config: &Path, command: RulesCommand) -> anyhow::Result<()> {
    let name = match command {
        RulesCommand::List => "rules list",
        RulesCommand::Allow { .. } => "rules allow",
        RulesCommand::Block { .. } => "rules block",
        RulesCommand::Remove { .. } => "rules remove",
    };
    anyhow::bail!("`{name}` is not implemented yet")
}
