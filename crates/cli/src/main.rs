//! `jensen-harness`: a standalone agent harness. `serve` runs a long-lived
//! daemon over a Unix socket; `run` speaks the same protocol headless over
//! stdio. Both share one JSON-RPC handler, so a client sees identical
//! behavior on either transport. The real logic lives in `lib.rs`; this is
//! the thin process entry point.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Parser, Subcommand};
use jensen_harness::engine::Engine;
use jensen_harness::rpc;
use jensen_harness::socket::{current_uid, remove_stale_socket};
use jensen_harness_core::providers::config::ProviderRegistry;
use tokio::io::{BufReader, stdin, stdout};
use tokio::net::UnixListener;

#[derive(Parser)]
#[command(
    name = "jensen-harness",
    about = "A standalone, model-provider-direct agent harness"
)]
struct Cli {
    /// Path to providers.toml; defaults to the user config directory.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Model as provider_id/model_id; defaults to `default_model` in the config.
    #[arg(long, global = true)]
    model: Option<String>,
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

fn config_path(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let base = dirs::config_dir().context("no user config directory exists on this platform")?;
    Ok(base.join("jensen-harness").join("providers.toml"))
}

fn build_engine(config: Option<PathBuf>, model: Option<String>) -> anyhow::Result<Engine> {
    let path = config_path(config)?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("the provider config {} could not be read", path.display()))?;
    let registry = ProviderRegistry::from_toml(&text)?;
    let address = model
        .or_else(|| registry.default_model().map(str::to_string))
        .context("no model was chosen: pass --model or set default_model in the config")?;
    let (provider, model_id) = registry.resolve(&address, &|name| std::env::var(name).ok())?;
    Ok(Engine::new(provider, model_id))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let engine = std::sync::Arc::new(build_engine(cli.config, cli.model)?);
    match cli.command {
        Command::Run => {
            let reader = BufReader::new(stdin());
            rpc::serve_connection(reader, stdout(), engine).await?;
        }
        Command::Serve { socket } => {
            remove_stale_socket(&socket).map_err(anyhow::Error::msg)?;
            let listener = UnixListener::bind(&socket)?;
            // A fresh Unix socket is world-accessible by default on most
            // systems, and this one carries live model credentials and
            // runs the agent on whoever connects to it: restrict it to
            // this user immediately, and verify every connecting peer's
            // uid matches, rather than trusting filesystem permissions
            // alone to still hold by the time a client connects.
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
            let my_uid = current_uid();
            tracing::info!(socket = %socket.display(), "listening");
            loop {
                let (stream, _addr) = listener.accept().await?;
                match stream.peer_cred() {
                    Ok(credentials) if credentials.uid() == my_uid => {}
                    Ok(credentials) => {
                        tracing::warn!(
                            peer_uid = credentials.uid(),
                            "rejected a connection from a different user"
                        );
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "a connection's identity could not be verified; rejected");
                        continue;
                    }
                }
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
