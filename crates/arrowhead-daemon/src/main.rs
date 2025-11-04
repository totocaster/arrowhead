//! Arrowhead daemon binary entrypoint.
use anyhow::Result;
use arrowhead_daemon::cli_main;

#[tokio::main]
async fn main() -> Result<()> {
    cli_main().await
}
