//! The `acan` command line: serves one repository's world.

use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use acan_llm::{KeyFile, ModelClient};
use acan_server::init_cli::{self, InitArgs};
use acan_server::receipt_cli::{self, Format, ShadowArgs};
use acan_server::release::{self, Redaction};
use acan_server::replay;
use acan_server::{
    DEFAULT_WITNESS_CONCURRENCY, Models, ServerConfig, VisionConfig, default_browser,
    default_data_dir, start_with_workers,
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
    /// Exports the recorded run of one repository as static files for the replay page.
    ExportReplay(ExportReplayArgs),
    /// Runs the contrast between two refs of a clone and prints its shadow receipt.
    ///
    /// The contrast writes the objects of the prepared merges into the clone's object store.
    /// No ref moves, and the clone's working tree is not touched.
    ///
    /// Exit status: 0 when a receipt is printed, even one that blocks; 2 when it blocks and
    /// --fail-on-block is given; 1 when the run is refused, with no receipt.
    ///
    /// With --check-contract, no run happens and --head is not needed: the base's protected
    /// contract is checked, and the command exits 0 only when it passes.
    Receipt(ReceiptArgs),
    /// Detects the protected app contract of a clone, shows it, and commits it to a new branch.
    ///
    /// It reads the base branch's committed files and runs no code from them. The clone's
    /// checkout is not touched unless --working-tree is given. Nothing is written until you
    /// type yes at the prompt.
    Init(InitArgs),
}

#[derive(Debug, Args)]
struct ExportReplayArgs {
    /// The repository whose recorded run to export.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// The folder to write `plant.json`, `events.json` and `manifest.json` into. Created when it
    /// is missing, such as `site/replay` beside the web bundle.
    #[arg(long)]
    out: PathBuf,
}

/// The exit status of a blocked receipt when `--fail-on-block` is given.
const BLOCKED_EXIT: i32 = 2;

#[derive(Debug, Args)]
struct ReceiptArgs {
    #[command(flatten)]
    shadow: ShadowArgs,
    /// The format printed to stdout.
    #[arg(long, value_enum, default_value_t = Format::Json)]
    format: Format,
    /// A directory that also receives `shadow-receipt.json` and `shadow-receipt.md`.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Exits with status 2 when the receipt blocks a land. Without it, a blocked receipt exits 0.
    #[arg(long)]
    fail_on_block: bool,
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
    /// How many workers this server runs in its own process, each taking one task at a time.
    /// Zero runs none. A worker waits, and is reported as having no model key, until a key is set.
    #[arg(long, default_value_t = 1)]
    workers: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(release::LogTee)
        .with_max_level(tracing::Level::INFO)
        .init();
    match Cli::parse().command {
        Command::Serve(args) => serve(args).await,
        Command::ExportReplay(args) => export_replay(args).await,
        Command::Receipt(args) => receipt(args).await,
        Command::Init(args) => {
            println!("{}", init_cli::run(&args)?);
            Ok(())
        }
    }
}

/// Loads the repository's `.env` file. A repository without one is the normal case. A file that
/// exists but does not parse is an error, so the keys it was meant to set are not silently
/// missing.
fn load_dotenv(repo: &std::path::Path) -> Result<()> {
    match dotenvy::from_path(repo.join(".env")) {
        Ok(()) => Ok(()),
        Err(dotenvy::Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).context("loading .env"),
    }
}

/// Exports the recorded run. The keys of `.env` are loaded first, so the privacy scan removes
/// them from the files as it would from a bug report.
async fn export_replay(args: ExportReplayArgs) -> Result<()> {
    load_dotenv(&args.repo)?;
    let redaction = Redaction::for_repo(&args.repo);
    let exported = replay::export(&args.repo, &args.out, &redaction).await?;
    println!(
        "exported {} events to {}",
        exported.events,
        exported.dir.display()
    );
    Ok(())
}

/// Prints the shadow receipt of one run. A refusal is an error with no receipt.
async fn receipt(args: ReceiptArgs) -> Result<()> {
    if args.shadow.check_contract {
        let check = receipt_cli::check_contract(&args.shadow).await?;
        println!("{}", receipt_cli::check_text(&check));
        return Ok(());
    }
    let diagnostics = args.out.as_ref().map(|dir| dir.join("diagnostics"));
    let report = receipt_cli::run(&args.shadow, diagnostics.as_deref()).await?;
    if let Some(dir) = &args.out {
        receipt_cli::write_out(&report, dir)?;
    }
    let text = match args.format {
        Format::Json => receipt_cli::json_text(&report)?,
        Format::Markdown => receipt_cli::markdown_text(&report)?,
    };
    println!("{text}");
    if args.fail_on_block && report.blocked() {
        std::process::exit(BLOCKED_EXIT);
    }
    Ok(())
}

async fn serve(args: ServeArgs) -> Result<()> {
    load_dotenv(&args.repo)?;

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
    let models = Models::open(&llm, KeyFile::for_user())?;
    let needs_key = !models.key_status().configured;
    let ledger = models.ledger();
    let config = ServerConfig {
        repo: args.repo.clone(),
        addr: SocketAddr::new(args.bind, args.port),
        team: args.team,
        web: args.web.or_else(bundled_web),
        protected_branch: args.protected_branch,
        witness_http_base_url: args.witness_http_base_url,
        witness_concurrency: args.witness_concurrency,
        vision: (!args.no_vision).then(|| VisionConfig {
            browser: args.vision_browser.unwrap_or_else(default_browser),
            data_dir: args.vision_dir.unwrap_or_else(default_data_dir),
        }),
    };
    let mut running = start_with_workers(config, models, ledger, args.workers).await?;
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
    if needs_key {
        // Model jobs get built-in replies until a key is saved, so say where to save one.
        match &running.owner_token {
            Some(token) => println!(
                "no model key is set. Add one at http://{}/setup#token={}",
                running.local_addr,
                token.as_str()
            ),
            None => println!(
                "no model key is set. Add one at http://{}/setup",
                running.local_addr
            ),
        }
    }
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
