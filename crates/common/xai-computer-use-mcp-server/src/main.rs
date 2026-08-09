//! Stdio entry point for the computer-use MCP server.
//!
//! Runs an MCP server over stdin/stdout, exposing the
//! [`xai_computer_use`] tool family. The native backend is built from the
//! environment (`COMPUTER_USE_CDP_PORT` / `COMPUTER_USE_BROWSER_PATH` /
//! `COMPUTER_USE_HEADLESS`) via [`ComputerUseService::shared`], exactly as
//! the in-process grok-build integration does.

use std::error::Error;

use xai_computer_use::ComputerUseService;
use xai_computer_use_mcp_server::ComputerUseMcpServer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::info!("starting xai-computer-use MCP server");

    let service = ComputerUseService::shared();
    let server = ComputerUseMcpServer::new(service);
    tracing::info!(tools = server.tool_count(), "computer-use tools registered");

    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let running = rmcp::serve_server(server, transport).await?;
    let reason = running.waiting().await?;

    tracing::info!(?reason, "computer-use MCP server stopped");
    Ok(())
}
