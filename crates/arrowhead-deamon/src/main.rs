//! Arrowhead deamon binary entrypoint.
use anyhow::Result;
use arrowhead_deamon::cli_main;

#[tokio::main]
async fn main() -> Result<()> {
    cli_main().await
}
