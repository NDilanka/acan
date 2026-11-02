//! The `acan` command line: serves one repository's world.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use acan_server::release::{self, Redaction};
use acan_server::{DEFAULT_WITNESS_CONCURRENCY, Models, ServerConfig, start};
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
    /// A directory served at `/`, such as the renderer page. Without it, the `web` folder beside
    /// the binary is served, when there is one.
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
    /// Does not check GitHub Releases for a newer build. The check is also off when the
    /// ACAN_UPDATE_CHECK variable is 0, false, no or off.
    #[arg(long)]
    no_update_check: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(release::LogTee)
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

    // The bug report removes the keys this process was given, so it is set after `.env` is loaded.
    release::configure(Redaction::for_repo(&args.repo));
    tracing::info!(
        version = release::version(),
        commit = release::commit(),
        "acan starting"
    );
    if update_check_enabled(args.no_update_check) {
        release::spawn_update_check();
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
        web: args.web.or_else(bundled_web),
        protected_branch: args.protected_branch,
        witness_http_base_url: args.witness_http_base_url,
        witness_concurrency: args.witness_concurrency,
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

/// Whether the update check runs. It is on unless `--no-update-check` or `ACAN_UPDATE_CHECK`
/// turns it off.
fn update_check_enabled(flag_off: bool) -> bool {
    if flag_off {
        return false;
    }
    match std::env::var("ACAN_UPDATE_CHECK") {
        Ok(value) => !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

/// The web UI shipped beside this binary: a `web` folder next to it, or `../share/acan/web` for the
/// layout the install scripts create. `None` when neither holds an `index.html`.
fn bundled_web() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    [
        dir.join("web"),
        dir.join("..").join("share").join("acan").join("web"),
    ]
    .into_iter()
    .find(|candidate| candidate.join("index.html").is_file())
}
