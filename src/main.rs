use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::{EnvFilter, fmt};

mod broker;
mod config;
mod discovery;
mod error;
mod server;
mod state;
mod tools;
mod transport;

use broker::Election;
use server::SshConnectServer;

#[tokio::main]
async fn main() -> Result<()> {
    // MCP uses stdout for JSON-RPC — all logging must go to stderr
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("Starting SSH-Connect MCP Server");

    // Elect a broker role so multiple instances share one set of live sessions.
    let server = match broker::elect().await {
        #[cfg(windows)]
        Election::Owner(pipe) => {
            let owner = SshConnectServer::new();
            tokio::spawn(broker::serve_owner(pipe, owner.clone()));
            owner
        }
        #[cfg(windows)]
        Election::Proxy(peer) => SshConnectServer::new_proxy(peer),
        Election::OwnerLocal => SshConnectServer::new(),
    };

    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("Server error: {:?}", e))?;

    service.waiting().await?;
    Ok(())
}
