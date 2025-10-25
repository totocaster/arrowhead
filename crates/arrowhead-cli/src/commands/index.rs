//! `arrowhead index` command.
//!
//! Indexing is fully managed by the background deamon from Phase 2 onwards.

use anyhow::Result;
use clap::Args;

use super::CommandContext;

/// Parameters for the (now informational) `index` CLI command.
#[derive(Debug, Args, Clone, PartialEq)]
pub struct IndexCommand {
    /// Deprecated: indexing is handled by the background deamon.
    #[arg(long, hide = true)]
    pub force: bool,
    /// Deprecated: indexing is handled by the background deamon.
    #[arg(long, value_name = "NOTE_ID", hide = true)]
    pub note: Option<String>,
    /// Deprecated: indexing is handled by the background deamon.
    #[arg(long, value_name = "N", hide = true)]
    pub parallel: Option<usize>,
    /// Deprecated: indexing is handled by the background deamon.
    #[arg(long, hide = true)]
    pub progress: bool,
}

/// Explain that the deamon now owns indexing duties.
pub async fn run(_ctx: &CommandContext, _command: &IndexCommand) -> Result<()> {
    println!(
        "Arrowhead now relies on the background deamon for indexing.\n\
         Launch it with `arrowhead vault start` and monitor progress via\n\
         `arrowhead vault status`."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[tokio::test]
    async fn index_command_returns_hint() {
        let ctx = CommandContext::new(AppConfig::default(), None, 0);
        let command = IndexCommand {
            force: false,
            note: None,
            parallel: None,
            progress: false,
        };
        assert!(run(&ctx, &command).await.is_ok());
    }
}
