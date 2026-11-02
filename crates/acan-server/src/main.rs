//! The `acan` command line: serves one repository's world.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use acan_server::{
    DEFAULT_WITNESS_CONCURRENCY, Models, ServerConfig, VisionConfig, default_browser,
    default_data_dir, start,
};
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

/// Acan's local server: one world per repository.
#[derive(Debug, Parser)]
#[command(name = "acan", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serves the world of one repository.
    Serve(ServeArgs),
}

#[derive(Debug, Args)]
struct ServeArgs {
    /// The repository to serve.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// The port to listen on.
    #[arg(long, default_value_t = 7311)]
    port: u16,
    /// The address to bind. Loopback by default.
    #[arg(long, default_value = "127.0.0.1")]
    bind: IpAddr,
    /// Allows a non-loopback address, for a shared team origin.
    #[arg(long)]
    team: bool,
    /// A directory served at `/`, such as the renderer page.
    #[arg(long)]
    web: Option<PathBuf>,
    /// The model routing file. Defaults to `<repo>/.acan/llm.toml`.
    #[arg(long)]
    llm: Option<PathBuf>,
    /// The branch finished work lands on. Defaults to the branch checked out in the repository.
    #[arg(long)]
    protected_branch: Option<String>,
    /// The base URL HTTP witness checks target, such as `http://127.0.0.1:3000`. Without it,
    /// every HTTP check reports an error.
    #[arg(long)]
    witness_http_base_url: Option<String>,
    /// How many witness runs may run at once.
    #[arg(long, default_value_t = DEFAULT_WITNESS_CONCURRENCY)]
    witness_concurrency: usize,
    /// The headless browser the vision witness renders pages with. Defaults to Edge, then Chrome.
    #[arg(long)]
    vision_browser: Option<PathBuf>,
    /// Where vision records and screenshots are kept. Defaults to the per-user data directory,
    /// and must not be inside the repository.
    #[arg(long)]
    vision_dir: Option<PathBuf>,
    /// Runs no advisory vision checks.
    #[arg(long)]
    no_vision: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();
    let Command::Serve(args) = Cli::parse().command;
    serve(args).await
}

async fn serve(args: ServeArgs) -> Result<()> {
    // A repository without a .env file is the normal case. A file that exists but does not parse
    // is an error, so the keys it was meant to set are not silently missing.
    match dotenvy::from_path(args.repo.join(".env")) {
        Ok(()) => {}
        Err(dotenvy::Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("loading .env"),
    }

    let llm = args
        .llm
        .unwrap_or_else(|| args.repo.join(".acan").join("llm.toml"));
    let models = Models::load(&llm)?;
    let ledger = models.ledger();
    let config = ServerConfig {
        repo: args.repo.clone(),
        addr: SocketAddr::new(args.bind, args.port),
        team: args.team,
        web: args.web,
        protected_branch: args.protected_branch,
        witness_http_base_url: args.witness_http_base_url,
        witness_concurrency: args.witness_concurrency,
        vision: (!args.no_vision).then(|| VisionConfig {
            browser: args.vision_browser.unwrap_or_else(default_browser),
            data_dir: args.vision_dir.unwrap_or_else(default_data_dir),
        }),
    };
    let mut running = start(config, models, ledger).await?;
    if let Some(token) = &running.owner_token {
        println!(
            "owner token, shown once and saved in .acan/tokens.json: {}",
            token.as_str()
        );
        println!("http://{}/#token={}", running.local_addr, token.as_str());
    }
    println!(
        "acan serving {} at http://{}",
        args.repo.display(),
        running.local_addr
    );
    // Either way the same barrier runs: no write may still be in flight when this returns.
    let outcome = tokio::select! {
        result = running.wait() => result,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
    };
    let stopped = running.stop().await;
    outcome.and(stopped)
}
