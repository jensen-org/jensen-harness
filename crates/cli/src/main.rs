//! `jensen-harness`: a standalone agent harness. `serve` runs a long-lived
//! daemon over a Unix socket; `run` speaks the same protocol headless over
//! stdio. Both share one JSON-RPC handler, so a client sees identical
//! behavior on either transport.

mod engine;
mod rpc;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use jensen_harness_core::providers::anthropic::AnthropicProvider;
use jensen_harness_core::providers::require_api_key;
use tokio::io::{BufReader, stdin, stdout};
use tokio::net::UnixListener;

use engine::Engine;

#[derive(Parser)]
#[command(
    name = "jensen-harness",
    about = "A standalone, model-provider-direct agent harness"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a long-lived daemon, listening on a Unix socket.
    Serve {
        #[arg(long)]
        socket: PathBuf,
    },
    /// Run headless, speaking JSON-RPC over stdio until it closes.
    Run,
}

fn build_engine() -> anyhow::Result<Engine> {
    let api_key = require_api_key("anthropic", "ANTHROPIC_API_KEY")?;
    let provider = AnthropicProvider::new(
        "anthropic",
        "https://api.anthropic.com",
        api_key,
        "claude-haiku-5-5",
    );
    Ok(Engine::new(Box::new(provider)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let engine = std::sync::Arc::new(build_engine()?);
    match cli.command {
        Command::Run => {
            let reader = BufReader::new(stdin());
            rpc::serve_connection(reader, stdout(), engine).await?;
        }
        Command::Serve { socket } => {
            if socket.exists() {
                std::fs::remove_file(&socket)?;
            }
            let listener = UnixListener::bind(&socket)?;
            tracing::info!(socket = %socket.display(), "listening");
            loop {
                let (stream, _addr) = listener.accept().await?;
                let engine = std::sync::Arc::clone(&engine);
                tokio::spawn(async move {
                    let (read_half, write_half) = stream.into_split();
                    let reader = BufReader::new(read_half);
                    if let Err(error) = rpc::serve_connection(reader, write_half, engine).await {
                        tracing::warn!(%error, "a connection ended with an error");
                    }
                });
            }
        }
    }
    Ok(())
}
