//! `jensen-harness`: a standalone agent harness. `serve` runs a long-lived
//! daemon over a Unix socket; `run` speaks the same protocol headless over
//! stdio. Both share one JSON-RPC handler, so a client sees identical
//! behavior on either transport. The real logic lives in `lib.rs`; this is
//! the thin process entry point.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use clap::{Parser, Subcommand};
use jensen_harness::engine::{ConfiguredModels, Engine, ModelSource};
use jensen_harness::rpc;
use jensen_harness::socket::{current_uid, remove_stale_socket};
use jensen_harness_core::providers::config::ProviderRegistry;
use jensen_harness_core::secrets::{SystemSecrets, store_in_keychain};
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
    /// Manage provider API keys held in the OS keychain.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Store a provider's API key, read from one line of stdin.
    Set { provider: String },
}

fn config_path(explicit: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let base = dirs::config_dir().context("no user config directory exists on this platform")?;
    Ok(base.join("jensen-harness").join("providers.toml"))
}

fn load_registry(config: Option<PathBuf>) -> anyhow::Result<ProviderRegistry> {
    let path = config_path(config)?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("the provider config {} could not be read", path.display()))?;
    Ok(ProviderRegistry::from_toml(&text)?)
}

fn auth_set(registry: &ProviderRegistry, provider: &str) -> anyhow::Result<()> {
    let (service, user) = registry.keychain_location(provider)?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .context("the key could not be read from stdin")?;
    let secret = line.trim();
    anyhow::ensure!(!secret.is_empty(), "stdin held no key");
    store_in_keychain(&service, &user, secret)?;
    writeln!(
        std::io::stderr(),
        "stored the key for {provider} in the keychain"
    )?;
    Ok(())
}

fn build_engine(registry: ProviderRegistry, model: Option<String>) -> anyhow::Result<Engine> {
    let models = ConfiguredModels {
        default_model: model.or_else(|| registry.default_model().map(str::to_string)),
        registry,
        secrets: Arc::new(SystemSecrets),
    };
    if let Some(address) = models.default_model.as_deref() {
        models.resolve(Some(address))?;
    }
    Ok(Engine::new(Box::new(models)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let registry = load_registry(cli.config)?;
    if let Command::Auth {
        action: AuthAction::Set { provider },
    } = &cli.command
    {
        return auth_set(&registry, provider);
    }
    let engine = Arc::new(build_engine(registry, cli.model)?);
    match cli.command {
        Command::Auth { .. } => {}
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
                let engine = Arc::clone(&engine);
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
