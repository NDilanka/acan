//! The `acan` command line: serves one or more repositories' worlds.

use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use acan_llm::{KeyFile, ModelClient};
use acan_server::init_cli::{self, InitArgs};
use acan_server::open::{self, Browser, RunDir};
use acan_server::projects::{ProjectSet, ProjectSpec};
use acan_server::receipt_cli::{self, Format, ShadowArgs};
use acan_server::release::{self, Redaction};
use acan_server::replay;
use acan_server::{
    ClientFactory, DEFAULT_WITNESS_CONCURRENCY, Models, ServeConfig, UserFolder, VisionConfig,
    apply_project_changes, default_browser, default_data_dir, default_settings_file,
    settings_file_beside, start_open, start_projects_adding, tokens_file,
};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

/// Acan's local server: one world per repository, one or more repositories per server.
#[derive(Debug, Parser)]
#[command(name = "acan", version = release::version())]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Serves the worlds of one or more repositories.
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
    /// Opens acan in the browser, signed in. The server runs in the background, one per user, and
    /// is started when none runs. It keeps running after this command returns.
    ///
    /// Exit status: 0 when the server answers; 1 when it does not, with the details in open.log in
    /// the run folder.
    Open(OpenArgs),
    /// Runs the background server that `acan open` starts.
    #[command(name = "open-server", hide = true)]
    OpenServer(OpenServerArgs),
}

#[derive(Debug, Args)]
struct OpenArgs {
    /// The port a new server listens on. Defaults to 7311. A server already running keeps its port.
    #[arg(long)]
    port: Option<u16>,
    /// Starts or finds the server, but opens no browser.
    #[arg(long)]
    no_browser: bool,
    #[command(flatten)]
    server: OpenServerOptions,
}

#[derive(Debug, Args)]
struct OpenServerArgs {
    /// The port to listen on, on 127.0.0.1.
    #[arg(long, default_value_t = open::DEFAULT_PORT)]
    port: u16,
    #[command(flatten)]
    server: OpenServerOptions,
}

/// The options `acan open` passes on to the server it starts.
#[derive(Debug, Args)]
struct OpenServerOptions {
    /// How many workers the server runs in its own process for each project. Zero runs none.
    #[arg(long, default_value_t = 1)]
    workers: usize,
    /// Runs no advisory vision checks.
    #[arg(long)]
    no_vision: bool,
    /// A directory served at `/`, such as the renderer page. Without it, the `web` folder beside
    /// the binary is served, when there is one.
    #[arg(long)]
    web: Option<PathBuf>,
}

impl OpenServerOptions {
    /// These options as the server's arguments. The server runs in the run folder, so a relative
    /// web folder is made absolute first.
    fn to_args(&self) -> Result<Vec<OsString>> {
        let mut args: Vec<OsString> = vec!["--workers".into(), self.workers.to_string().into()];
        if self.no_vision {
            args.push("--no-vision".into());
        }
        if let Some(web) = &self.web {
            args.push("--web".into());
            args.push(std::path::absolute(web).context("resolving --web")?.into());
        }
        Ok(args)
    }
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
    /// A repository to serve as a project. Repeat it to serve several. The first is the default
    /// project, which also answers the unprefixed routes. Its id is its folder name.
    #[arg(long = "repo")]
    repos: Vec<PathBuf>,
    /// A projects file with pinned ids, names and settings. Replaces --repo.
    #[arg(long, conflicts_with = "repos")]
    projects: Option<PathBuf>,
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
    /// The model routing file of the one project. Defaults to `<repo>/.acan/llm.toml`. When
    /// serving several projects, set it per project in a projects file.
    #[arg(long)]
    llm: Option<PathBuf>,
    /// The branch finished work lands on. Defaults to the branch checked out in the repository.
    /// Names one project; set it per project in a projects file when serving several.
    #[arg(long)]
    protected_branch: Option<String>,
    /// The base URL HTTP witness checks target, such as `http://127.0.0.1:3000`. Without it,
    /// every HTTP check reports an error. Names one project, like --protected-branch.
    #[arg(long)]
    witness_http_base_url: Option<String>,
    /// How many witness runs may run at once, across every project.
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
    /// and must not be inside any repository.
    #[arg(long)]
    vision_dir: Option<PathBuf>,
    /// Runs no advisory vision checks.
    #[arg(long)]
    no_vision: bool,
    /// How many workers this server runs in its own process for each project, each taking one
    /// task at a time. Zero runs none. A worker waits, and is reported as having no model key,
    /// until a key is set.
    #[arg(long, default_value_t = 1)]
    workers: usize,
    /// Maps each project's codebase when its first page connects, not as the server starts.
    ///
    /// Until a page connects, the project's map stays blank, runs no scan and takes no writes. The
    /// first page's stream starts the scan, so the map builds while that page watches. Without the
    /// flag, the codebase maps as the server starts, which is the default.
    #[arg(long)]
    map_on_first_view: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // The background server writes to a log file, where colour codes are only noise.
    let ansi = !matches!(cli.command, Command::OpenServer(_));
    tracing_subscriber::fmt()
        .with_writer(release::LogTee)
        .with_max_level(tracing::Level::INFO)
        .with_ansi(ansi)
        .init();
    match cli.command {
        Command::Serve(args) => serve(args).await,
        Command::Open(args) => open(args).await,
        Command::OpenServer(args) => open_server(args).await,
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
    let mut projects = match &args.projects {
        Some(file) => ProjectSet::from_file(file)?,
        None => {
            let repos = if args.repos.is_empty() {
                vec![PathBuf::from(".")]
            } else {
                args.repos.clone()
            };
            ProjectSet::from_repos(&repos)
        }
    };
    if let [project] = projects.projects.as_mut_slice() {
        if let Some(branch) = &args.protected_branch {
            project.protected_branch = Some(branch.clone());
        }
        if let Some(url) = &args.witness_http_base_url {
            project.witness_http_base_url = Some(url.clone());
        }
        if let Some(llm) = &args.llm {
            project.llm = Some(llm.clone());
        }
    } else if args.protected_branch.is_some()
        || args.witness_http_base_url.is_some()
        || args.llm.is_some()
    {
        bail!(
            "--protected-branch, --witness-http-base-url and --llm name one project; set them per project in a --projects file"
        );
    }
    projects.validate()?;
    // The settings sit beside the model keys, so every project this user runs shares them. The
    // folders settings added and removed are applied before the clients are built.
    let settings = KeyFile::for_user().map_or_else(
        || default_settings_file(&projects),
        |keys| settings_file_beside(&keys),
    );
    for line in apply_project_changes(&mut projects, &settings)? {
        println!("acan did not add a project: {line}");
    }

    // The keys of every project's `.env` are loaded before the bug report's redaction is built.
    for project in &projects.projects {
        load_dotenv(&project.repo)?;
    }
    // The bug report removes what every project holds: its secrets and its repository's paths.
    release::configure(projects.redaction());
    tracing::info!(
        version = release::version(),
        commit = release::commit(),
        "acan starting"
    );
    if update_check_enabled(args.no_update_check) {
        release::spawn_update_check();
    }

    let mut clients = Vec::with_capacity(projects.projects.len());
    let mut needs_key = false;
    for project in &projects.projects {
        let llm = project
            .llm
            .clone()
            .unwrap_or_else(|| project.repo.join(".acan").join("llm.toml"));
        let models = Models::open(&llm, KeyFile::for_user())?;
        // Model keys are user-wide, so every project reports the same key status.
        needs_key = !models.key_status().configured;
        let ledger = models.ledger();
        clients.push((models, ledger));
    }
    let config = ServeConfig {
        projects,
        addr: SocketAddr::new(args.bind, args.port),
        team: args.team,
        web: args.web.or_else(bundled_web),
        witness_concurrency: args.witness_concurrency,
        vision: (!args.no_vision).then(|| VisionConfig {
            browser: args.vision_browser.unwrap_or_else(default_browser),
            data_dir: args.vision_dir.unwrap_or_else(default_data_dir),
        }),
        local_workers: args.workers,
        map_on_first_view: args.map_on_first_view,
    };
    let tokens = config
        .projects
        .projects
        .first()
        .map(|project| tokens_file(&project.repo))
        .unwrap_or_else(|| tokens_file(Path::new(".")));
    let tokens = std::path::absolute(&tokens).unwrap_or(tokens);
    let map_on_view = args.map_on_first_view;
    // A project added while the server runs is opened from its routing file, as a project at start
    // is, so the settings can serve a new folder with no restart.
    let opener: ClientFactory<Models> = Box::new(|project: &ProjectSpec| {
        let llm = project
            .llm
            .clone()
            .unwrap_or_else(|| project.repo.join(".acan").join("llm.toml"));
        let models = Models::open(&llm, KeyFile::for_user())?;
        let ledger = models.ledger();
        Ok::<_, anyhow::Error>((models, ledger))
    });
    // A server that maps on first view is listening before its codebases map, so it does not wait
    // for them: the first page is what starts each map. Otherwise the start waits for every map.
    let mut running = start_projects_adding(config, clients, Some(settings), opener).await?;
    if !map_on_view && let Err(err) = running.wait_mapped().await {
        let _ = running.stop().await;
        return Err(err);
    }
    let terminal = io::stdout().is_terminal();
    if let Some(token) = &running.owner_token {
        for line in owner_token_lines(running.local_addr, token.as_str(), &tokens, terminal) {
            println!("{line}");
        }
    }
    println!(
        "acan serving projects {} at http://{}",
        running.project_ids().join(", "),
        running.local_addr
    );
    if map_on_view {
        println!("the codebase maps when the first page connects");
    }
    if needs_key {
        // Model jobs get built-in replies until a key is saved, so say where to save one.
        let token = running.owner_token.as_ref().map(|token| token.as_str());
        println!("{}", key_line(running.local_addr, token, terminal));
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

/// Finds or starts the user's server and opens the browser on it. A failure prints one line to
/// stderr and exits 1; the line names open.log when it was written.
async fn open(args: OpenArgs) -> Result<()> {
    let request = open::Request {
        port: args.port,
        browser: !args.no_browser,
        server_args: args.server.to_args()?,
    };
    match open::open(&request).await {
        Ok(opened) => {
            if opened.reused
                && let Some(asked) = args.port
                && asked != opened.port
            {
                println!(
                    "acan is already running on port {}, so that server is used rather than port {asked}",
                    opened.port
                );
            }
            println!("acan is open at http://127.0.0.1:{}/", opened.port);
            if let Browser::Failed(reason) = &opened.browser {
                println!("the browser did not open: {reason}");
            }
            Ok(())
        }
        Err(failed) => {
            let what = failed.what.replace(['\r', '\n'], " ");
            match &failed.log {
                Some(log) => eprintln!(
                    "acan did not open: {what}. The details are in {}",
                    log.display()
                ),
                None => eprintln!("acan did not open: {what}"),
            }
            std::process::exit(1);
        }
    }
}

/// The background server `acan open` starts. It serves the user's folder, with the projects its
/// settings added, on 127.0.0.1 only. It holds the run folder's lock while it runs, so it is the
/// only one, and never prints the owner's token, since its output goes to a log file.
async fn open_server(args: OpenServerArgs) -> Result<()> {
    let run = open::run_dir().context(
        "there is no run folder; set HOME or XDG_STATE_HOME, or LOCALAPPDATA on Windows",
    )?;
    let run = RunDir::create(run).context("creating the run folder")?;
    let Some(_lock) = run
        .lock_for_server()
        .await
        .context("opening the server lock")?
    else {
        bail!(
            "another acan server is running: it holds {}",
            run.lock_file().display()
        );
    };
    // Files left by a server that did not stop cleanly name a port nothing answers on.
    run.remove_live_files();
    let keys = KeyFile::for_user().context(
        "there is no user folder for the settings and keys; set HOME, or APPDATA on Windows",
    )?;
    let folder = keys
        .path()
        .parent()
        .context("the keys file has no folder")?
        .to_path_buf();
    let settings = settings_file_beside(&keys);
    let mut projects = ProjectSet::from_repos(&[]);
    for line in apply_project_changes(&mut projects, &settings)? {
        println!("acan did not add a project: {line}");
    }
    for project in &projects.projects {
        load_dotenv(&project.repo)?;
    }
    release::configure(projects.redaction());
    tracing::info!(
        version = release::version(),
        commit = release::commit(),
        "acan starting in the background"
    );
    if update_check_enabled(false) {
        release::spawn_update_check();
    }

    let mut clients = Vec::with_capacity(projects.projects.len());
    for project in &projects.projects {
        let models = Models::open(&routing_file(project), KeyFile::for_user())?;
        let ledger = models.ledger();
        clients.push((models, ledger));
    }
    // The user's folder has no routing file, so its client always reads the saved keys. An empty
    // path never exists.
    let user_keys = Models::open(Path::new(""), Some(keys))?;
    let opener: ClientFactory<Models> = Box::new(|project: &ProjectSpec| {
        let models = Models::open(&routing_file(project), KeyFile::for_user())?;
        let ledger = models.ledger();
        Ok::<_, anyhow::Error>((models, ledger))
    });
    let config = ServeConfig {
        projects,
        addr: SocketAddr::from((Ipv4Addr::LOCALHOST, args.port)),
        team: false,
        web: args.server.web.or_else(bundled_web),
        witness_concurrency: DEFAULT_WITNESS_CONCURRENCY,
        vision: (!args.server.no_vision).then(|| VisionConfig {
            browser: default_browser(),
            data_dir: default_data_dir(),
        }),
        local_workers: args.server.workers,
        map_on_first_view: false,
    };
    let user = UserFolder {
        dir: folder,
        keys: user_keys,
    };
    // The codebases map while the server already answers, so the launcher's wait stays short.
    let mut running = start_open(config, clients, user, opener).await?;
    let written = run
        .write_pid(std::process::id())
        .and_then(|()| run.write_port(running.local_addr.port()));
    if let Err(err) = written {
        let _ = running.stop().await;
        run.remove_live_files();
        return Err(err).context("writing the server's port and pid files");
    }
    println!(
        "acan serving projects [{}] at http://{}/ in the background",
        running.project_ids().join(", "),
        running.local_addr
    );
    let outcome = tokio::select! {
        result = running.wait() => result,
        () = interrupted() => {
            tracing::info!("shutting down");
            Ok(())
        }
        () = terminated() => {
            tracing::info!("shutting down");
            Ok(())
        }
    };
    let stopped = running.stop().await;
    run.remove_live_files();
    outcome.and(stopped)
}

/// The routing file of `project`: its own, or `.acan/llm.toml` in its repository.
fn routing_file(project: &ProjectSpec) -> PathBuf {
    project
        .llm
        .clone()
        .unwrap_or_else(|| project.repo.join(".acan").join("llm.toml"))
}

/// Completes on Ctrl-C. A process with no console may be unable to listen for it, and then this
/// never completes, rather than stopping the server at once.
async fn interrupted() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Completes on SIGTERM. Never completes where there is no such signal.
async fn terminated() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            term.recv().await;
            return;
        }
    }
    std::future::pending::<()>().await;
}

/// The lines `acan serve` prints about the owner token on its first start. On a terminal the token
/// is shown once, with the sign-in link. Anywhere else the output may be kept in a log or read by
/// another process, so the token is left out and the tokens file is named instead.
fn owner_token_lines(addr: SocketAddr, token: &str, file: &Path, terminal: bool) -> Vec<String> {
    if terminal {
        vec![
            format!("owner token, shown once and saved in .acan/tokens.json: {token}"),
            format!("http://{addr}/#token={token}"),
        ]
    } else {
        vec![format!(
            "owner token not printed, because stdout is not a terminal. It is saved in {}. Open http://{addr}/ and paste it there.",
            file.display()
        )]
    }
}

/// The line that says where to save a model key. The owner token is part of the link only when
/// it is shown on a terminal.
fn key_line(addr: SocketAddr, token: Option<&str>, terminal: bool) -> String {
    match token.filter(|_| terminal) {
        Some(token) => format!("no model key is set. Add one at http://{addr}/setup#token={token}"),
        None => format!("no model key is set. Add one at http://{addr}/setup"),
    }
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

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::Path;

    use clap::Parser;

    use super::{Cli, Command};

    use super::{key_line, owner_token_lines};

    const TOKEN: &str = "acan_0123456789abcdef0123456789abcdef";

    fn addr() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 7311))
    }

    #[test]
    fn a_terminal_shows_the_owner_token_with_its_sign_in_link() {
        let lines = owner_token_lines(addr(), TOKEN, Path::new("/repo/.acan/tokens.json"), true);
        assert_eq!(lines.len(), 2, "the token line and the link: {lines:?}");
        assert!(lines[0].ends_with(TOKEN), "the token line: {}", lines[0]);
        assert_eq!(lines[1], format!("http://127.0.0.1:7311/#token={TOKEN}"));
    }

    #[test]
    fn output_that_is_not_a_terminal_names_the_file_and_leaves_the_token_out() {
        let lines = owner_token_lines(addr(), TOKEN, Path::new("/repo/.acan/tokens.json"), false);
        let text = lines.join("\n");
        assert!(
            !text.contains(TOKEN),
            "the token reached the output: {text}"
        );
        assert!(!text.contains("#token"), "a link carries the token: {text}");
        assert!(
            text.contains("/repo/.acan/tokens.json"),
            "the tokens file is not named: {text}"
        );
        assert!(
            text.contains("http://127.0.0.1:7311/"),
            "the address is not named: {text}"
        );
    }

    #[test]
    fn map_on_first_view_is_off_unless_the_flag_is_given() {
        let serve = |args: &[&str]| match Cli::try_parse_from(args).expect("the arguments parse") {
            Cli {
                command: Command::Serve(args),
            } => args,
            other => panic!("expected serve, got {other:?}"),
        };
        assert!(!serve(&["acan", "serve"]).map_on_first_view);
        assert!(serve(&["acan", "serve", "--map-on-first-view"]).map_on_first_view);
    }

    #[test]
    fn the_key_line_carries_the_token_only_on_a_terminal() {
        assert_eq!(
            key_line(addr(), Some(TOKEN), true),
            format!("no model key is set. Add one at http://127.0.0.1:7311/setup#token={TOKEN}")
        );
        let piped = key_line(addr(), Some(TOKEN), false);
        assert!(
            !piped.contains(TOKEN),
            "the token reached the output: {piped}"
        );
        assert_eq!(
            piped,
            "no model key is set. Add one at http://127.0.0.1:7311/setup"
        );
        assert_eq!(
            key_line(addr(), None, true),
            "no model key is set. Add one at http://127.0.0.1:7311/setup",
            "a later start has no token to show"
        );
    }
}
