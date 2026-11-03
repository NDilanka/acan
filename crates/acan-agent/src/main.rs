//! `acan-worker`: one harness worker. It takes a task from an acan origin over MCP and builds it
//! in its worktree. The step log goes to stderr; the final exit goes to stdout.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use acan_agent::{Exit, WorkerConfig, run};
use acan_llm::{ApiKeys, LlmConfig, OpenAiCompatible};
use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};

/// One worker run against one origin.
// No `Debug`: the struct holds the agent token, and a derived `Debug` would print it.
#[derive(Parser)]
#[command(name = "acan-worker", version, about = "Runs one acan worker over MCP")]
struct Cli {
    /// The origin's base URL, such as `http://127.0.0.1:7311`.
    #[arg(long)]
    server: String,
    /// The project the origin serves to work on, by its id, such as `acan`. Without it the
    /// worker uses the origin's default project.
    #[arg(long)]
    project: Option<String>,
    /// A file that holds the agent's bearer token and nothing else. Its contents are read, never
    /// printed. Use this or the `ACAN_AGENT_TOKEN` variable rather than `--token`.
    #[arg(long, value_name = "PATH", conflicts_with = "token")]
    token_file: Option<PathBuf>,
    /// The agent's bearer token, given on the command line. Prefer `--token-file` or the
    /// `ACAN_AGENT_TOKEN` variable: a flag value can show in the process list.
    #[arg(long, conflicts_with = "token_file")]
    token: Option<String>,
    /// The repository the origin serves. Its tracked files ground each plan.
    #[arg(long)]
    repo: PathBuf,
    /// The role to run as. The worker is the only role.
    #[arg(long, value_enum, default_value_t = Role::Worker)]
    role: Role,
    /// The model routing file.
    #[arg(long, default_value = "crates/acan-llm/llm.toml")]
    llm: PathBuf,
    /// A file of provider keys. A missing file is fine.
    #[arg(long, default_value = ".env")]
    env: PathBuf,
    /// The command `run_tests` runs in the worktree. Without it, `run_tests` reports none is set.
    #[arg(long)]
    test_command: Option<String>,
    /// Seconds one wait may take: for work to appear, a verdict, or build clearance.
    #[arg(long, default_value_t = 600)]
    wait_secs: u64,
    /// Seconds between lease renewals. Keep it well under the origin's 60 second lease.
    #[arg(long, default_value_t = 20)]
    heartbeat_secs: u64,
    /// The most model calls the first build's edit loop may make. A repair round may make twice as many.
    #[arg(long, default_value_t = 30)]
    max_steps: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Role {
    Worker,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_max_level(tracing::Level::INFO)
        .init();
    match run_cli().await {
        Ok(exit) => {
            println!("{exit}");
            if matches!(exit, Exit::Built { .. }) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(err) => {
            eprintln!("acan-worker: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run_cli() -> Result<Exit> {
    let cli = Cli::parse();
    match cli.role {
        Role::Worker => {}
    }
    let file = cli.token_file.as_deref().map(read_token_file).transpose()?;
    let token = agent_token(
        file.flatten().or(cli.token),
        std::env::var("ACAN_AGENT_TOKEN").ok(),
    )?;
    load_env(&cli.env)?;
    let config =
        LlmConfig::load(&cli.llm).with_context(|| format!("loading {}", cli.llm.display()))?;
    let keys = ApiKeys::from_env(&config);
    let model = OpenAiCompatible::new(config, keys).context("building the model client")?;
    let server = project_url(&cli.server, cli.project.as_deref())?;
    let worker = WorkerConfig {
        server,
        token,
        repo: cli.repo,
        test_command: cli.test_command,
        max_steps: cli.max_steps,
        wait: Duration::from_secs(cli.wait_secs),
        receipt_wait: Duration::from_secs(cli.wait_secs),
        heartbeat: Duration::from_secs(cli.heartbeat_secs),
        poll: Duration::from_millis(500),
    };
    Ok(run(&worker, &model).await.exit)
}

/// The agent token: the token from `--token-file` or `--token` when one is given, else the
/// `ACAN_AGENT_TOKEN` variable. An empty value counts as missing. The error names the sources and
/// shows no value.
fn agent_token(given: Option<String>, variable: Option<String>) -> Result<String> {
    given
        .filter(|token| !token.is_empty())
        .or_else(|| variable.filter(|token| !token.is_empty()))
        .context("no agent token: pass --token-file, or set ACAN_AGENT_TOKEN")
}

/// Reads the agent token from a file that holds it alone. Surrounding whitespace is dropped, and
/// an empty file is no token. The error names the file and never its contents.
fn read_token_file(path: &Path) -> Result<Option<String>> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading the token file {}", path.display()))?;
    Ok(Some(text.trim().to_owned()).filter(|token| !token.is_empty()))
}

/// The base URL the worker's MCP calls go to. With a project, that is the project's routes under
/// `/api/p/<id>`. Without one, it is the origin's unprefixed routes, which serve the default
/// project.
fn project_url(server: &str, project: Option<&str>) -> Result<String> {
    let base = server.trim_end_matches('/');
    match project {
        None => Ok(base.to_owned()),
        Some(id) if valid_project_id(id) => Ok(format!("{base}/api/p/{id}")),
        Some(id) => bail!(
            "the project id {id:?} is not valid: lowercase letters, digits and dashes, with no dash at either end"
        ),
    }
}

/// Whether `id` may name a project. The origin applies the same rule when it names projects, so a
/// valid id here is a valid id there.
fn valid_project_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Loads the provider keys from `path`. A missing file is normal; a file that does not parse is
/// an error, so keys are not silently missing.
///
/// The parser's error quotes the line it failed on, and that line can hold a key, so the error
/// names the file and the failure without the parser's text.
fn load_env(path: &Path) -> Result<()> {
    match dotenvy::from_path(path) {
        Ok(()) => Ok(()),
        Err(err) if err.not_found() => Ok(()),
        Err(dotenvy::Error::LineParse(..)) => {
            bail!(
                "the provider key file {} does not parse; check its lines",
                path.display()
            )
        }
        Err(dotenvy::Error::Io(err)) => {
            bail!(
                "the provider key file {} could not be read: {}",
                path.display(),
                err.kind()
            )
        }
        Err(_) => bail!("the provider key file {} could not be read", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use clap::{CommandFactory, Parser};

    use super::{Cli, agent_token, load_env, project_url, read_token_file};

    #[test]
    fn the_flag_wins_and_the_variable_fills_in() {
        let flag = Some("acan_agent_flag".to_owned());
        let variable = Some("acan_agent_variable".to_owned());
        assert_eq!(
            agent_token(flag.clone(), variable.clone()).expect("the flag is a token"),
            "acan_agent_flag"
        );
        assert_eq!(
            agent_token(None, variable).expect("the variable is a token"),
            "acan_agent_variable"
        );
        assert_eq!(
            agent_token(flag, None).expect("the flag alone is a token"),
            "acan_agent_flag"
        );
    }

    #[test]
    fn an_empty_value_counts_as_missing() {
        let token = agent_token(Some(String::new()), Some("acan_agent_variable".to_owned()))
            .expect("the variable is used when the flag is empty");
        assert_eq!(token, "acan_agent_variable");
        agent_token(Some(String::new()), None).expect_err("an empty flag is no token");
    }

    #[test]
    fn no_token_is_an_error_that_names_the_variable() {
        let err = agent_token(None, None).expect_err("no token is an error");
        let text = format!("{err:#}");
        assert!(
            text.contains("ACAN_AGENT_TOKEN"),
            "the message does not name the variable: {text}"
        );
    }

    #[test]
    fn the_token_flag_is_optional() {
        let cli = Cli::try_parse_from([
            "acan-worker",
            "--server",
            "http://127.0.0.1:7311",
            "--repo",
            ".",
        ])
        .expect("the flags parse without --token");
        assert!(cli.token.is_none());
    }

    #[test]
    fn the_project_flag_selects_its_routes_and_none_is_the_default() {
        assert_eq!(
            project_url("http://127.0.0.1:7311", Some("acan")).expect("a valid id"),
            "http://127.0.0.1:7311/api/p/acan"
        );
        assert_eq!(
            project_url("http://127.0.0.1:7311/", None).expect("no project"),
            "http://127.0.0.1:7311",
            "without a project the base is the origin, with its trailing slash dropped"
        );
    }

    #[test]
    fn the_token_file_holds_the_token_alone() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("token");
        fs::write(&path, "  acan_agent_file\r\n").expect("the token file");
        assert_eq!(
            read_token_file(&path).expect("the file reads"),
            Some("acan_agent_file".to_owned())
        );
    }

    #[test]
    fn an_empty_token_file_counts_as_missing() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("token");
        fs::write(&path, " \n").expect("the token file");
        assert_eq!(read_token_file(&path).expect("the file reads"), None);
    }

    #[test]
    fn a_token_file_that_cannot_be_read_names_the_file() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("absent-token");
        let err = read_token_file(&path).expect_err("a missing file is an error");
        let text = format!("{err:#}");
        assert!(
            text.contains("absent-token"),
            "the message does not name the file: {text}"
        );
    }

    #[test]
    fn the_token_file_and_the_flag_cannot_both_be_given() {
        // `Cli` has no `Debug`, so the result is checked as a bool rather than with `expect_err`.
        let parsed = Cli::try_parse_from([
            "acan-worker",
            "--server",
            "http://127.0.0.1:7311",
            "--repo",
            ".",
            "--token-file",
            "token",
            "--token",
            "acan_agent_flag",
        ]);
        assert!(parsed.is_err(), "the two sources of one token conflict");
    }

    #[test]
    fn the_token_file_option_parses_without_the_flag() {
        let cli = Cli::try_parse_from([
            "acan-worker",
            "--server",
            "http://127.0.0.1:7311",
            "--repo",
            ".",
            "--token-file",
            "token",
        ])
        .expect("the file option parses");
        assert_eq!(cli.token_file.as_deref(), Some(Path::new("token")));
        assert!(cli.token.is_none());
    }

    #[test]
    fn the_help_offers_the_file_and_the_variable_before_the_flag() {
        let help = Cli::command().render_help().to_string();
        let help = help.split_whitespace().collect::<Vec<_>>().join(" ");
        let file = help
            .find("--token-file <PATH>")
            .expect("the help names the file option");
        let flag = help
            .find("--token <TOKEN>")
            .expect("the help names the flag");
        assert!(
            file < flag,
            "the help offers the flag before the file: {help}"
        );
        assert!(
            help.contains("ACAN_AGENT_TOKEN"),
            "the help does not name the variable: {help}"
        );
        assert!(
            help.contains("process list"),
            "the help does not say why the flag is the weaker choice: {help}"
        );
    }

    #[test]
    fn a_project_id_that_the_origin_would_not_name_is_refused_before_any_call() {
        for id in ["", "-acan", "acan-", "Acan", "a/b", "a b", "../x", "a_b"] {
            assert!(
                project_url("http://127.0.0.1:7311", Some(id)).is_err(),
                "{id:?} is refused"
            );
        }
    }

    #[test]
    fn a_key_file_that_does_not_parse_is_reported_without_its_text() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join(".env");
        // A line that starts with a quote is not a key, so the parser fails on the line itself,
        // which is where a key would sit.
        fs::write(&path, "\"sk-live-secret-value\" oops\n").expect("the key file");
        let err = load_env(&path).expect_err("a file that does not parse is an error");
        let text = format!("{err:#}");
        assert!(
            !text.contains("sk-live-secret-value"),
            "the message carries the key: {text}"
        );
        assert!(text.contains(".env"), "the message names the file: {text}");
    }

    #[test]
    fn a_missing_key_file_is_normal() {
        let dir = tempfile::tempdir().expect("temporary directory");
        load_env(&dir.path().join("absent.env")).expect("a missing file loads nothing");
    }
}
