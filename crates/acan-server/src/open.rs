//! `acan open`: one server per user, started in the background and found again.
//!
//! The launcher does not host the server. It starts this same program detached with the hidden
//! [`SERVER_COMMAND`], which serves the user's own folder, and waits until that server answers.
//! When a server already runs for this user, the launcher uses it instead. The state the two share
//! is kept in the run folder, see [`run_dir`]:
//!
//! - `server.lock` is held by the server for as long as it runs, so only one server runs.
//! - `server.port` and `server.pid` are written once the server listens, and removed when it stops.
//! - `server.log` receives the server's output.
//! - `open.log` describes the last failed open.
//!
//! No file here, and no line printed, carries the owner's token or a sign-in code.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use acan_core::MemberId;
use acan_llm::KeyFile;
use serde_json::Value;

use crate::Tokens;

/// The port `acan open` listens on unless `--port` names another.
pub const DEFAULT_PORT: u16 = 7311;

/// The hidden subcommand that runs the detached server.
pub const SERVER_COMMAND: &str = "open-server";

/// The variable that names the program that opens the browser. It is called with the address as
/// its only argument. Set to the empty string, no browser is opened.
pub const BROWSER_VAR: &str = "ACAN_BROWSER";

const SERVER_LOG: &str = "server.log";
const OPEN_LOG: &str = "open.log";
const LOCK_FILE: &str = "server.lock";
const PID_FILE: &str = "server.pid";
const PORT_FILE: &str = "server.port";

/// How long the launcher waits for a server to answer.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the launcher looks again while it waits.
const POLL: Duration = Duration::from_millis(200);
/// How long a starting server keeps trying for the lock. A launcher checks the lock by taking it
/// for a moment, so a server can find it briefly held by a launcher that is only looking.
const LOCK_RETRY: Duration = Duration::from_secs(2);
/// How long one probe request may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the browser program has to report a failure before it is taken as started.
const BROWSER_WAIT: Duration = Duration::from_secs(5);
/// A server log past this size is moved aside to `server.log.1` before the next start.
const ROTATE_AT: u64 = 1024 * 1024;
/// How many of the server's last log lines a failure log keeps.
const TAIL_LINES: usize = 40;
/// How many bytes of the server log are read for that tail, at most.
const TAIL_BYTES: u64 = 64 * 1024;

/// The member whose token is the owner's.
fn founder() -> MemberId {
    MemberId::from_str(crate::OWNER).expect("the owner is a valid member id")
}

/// The user's own folder: the folder that holds the model keys, `settings.json` and
/// `tokens.json`. `%APPDATA%\acan` on Windows. `None` when there is no such folder.
#[must_use]
pub fn user_dir() -> Option<PathBuf> {
    KeyFile::for_user().and_then(|keys| keys.path().parent().map(Path::to_path_buf))
}

/// The owner's token in the user's folder `dir`, or `None` when it holds none or cannot be read.
#[must_use]
pub fn owner_token(dir: &Path) -> Option<String> {
    Tokens::owner_token_in(dir, &founder())
        .ok()
        .flatten()
        .map(|token| token.as_str().to_owned())
}

/// The run folder: `%LOCALAPPDATA%\Acan` on Windows; elsewhere `$XDG_STATE_HOME/acan`, else
/// `~/.local/state/acan`. `None` when the variables it needs are unset or not absolute.
#[must_use]
pub fn run_dir() -> Option<PathBuf> {
    run_dir_from(cfg!(windows), |name| std::env::var_os(name))
}

fn run_dir_from(windows: bool, var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let absolute = |name: &str| var(name).map(PathBuf::from).filter(|dir| dir.is_absolute());
    if windows {
        return absolute("LOCALAPPDATA").map(|dir| dir.join("Acan"));
    }
    absolute("XDG_STATE_HOME")
        .map(|dir| dir.join("acan"))
        .or_else(|| absolute("HOME").map(|home| home.join(".local").join("state").join("acan")))
}

/// The run folder and the files in it.
#[derive(Debug, Clone)]
pub struct RunDir {
    dir: PathBuf,
}

/// The single-server lock. It is released when this is dropped, or when the process ends.
#[derive(Debug)]
pub struct ServerLock {
    _file: File,
}

impl RunDir {
    /// Opens the run folder `dir`, creating it when it is missing.
    ///
    /// # Errors
    ///
    /// Returns an error when the folder cannot be created.
    pub fn create(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// The folder itself.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// The server's output.
    #[must_use]
    pub fn server_log(&self) -> PathBuf {
        self.dir.join(SERVER_LOG)
    }

    /// The description of the last failed open.
    #[must_use]
    pub fn open_log(&self) -> PathBuf {
        self.dir.join(OPEN_LOG)
    }

    /// The file whose lock the running server holds.
    #[must_use]
    pub fn lock_file(&self) -> PathBuf {
        self.dir.join(LOCK_FILE)
    }

    /// The running server's process id.
    #[must_use]
    pub fn pid_file(&self) -> PathBuf {
        self.dir.join(PID_FILE)
    }

    /// The port the running server listens on.
    #[must_use]
    pub fn port_file(&self) -> PathBuf {
        self.dir.join(PORT_FILE)
    }

    /// Takes the single-server lock. `None` when another handle holds it.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock file cannot be opened or locked for another reason.
    pub fn try_lock(&self) -> io::Result<Option<ServerLock>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.lock_file())?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ServerLock { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(err)) => Err(err),
        }
    }

    /// Whether a server holds the lock now.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`try_lock`](Self::try_lock).
    pub fn lock_held(&self) -> io::Result<bool> {
        self.try_lock().map(|lock| lock.is_none())
    }

    /// Takes the lock for a starting server, trying again for a moment while a launcher holds it to
    /// look. `None` when it is still held after that, by another server.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`try_lock`](Self::try_lock).
    pub async fn lock_for_server(&self) -> io::Result<Option<ServerLock>> {
        let deadline = Instant::now() + LOCK_RETRY;
        loop {
            if let Some(lock) = self.try_lock()? {
                return Ok(Some(lock));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The port in `server.port`, or `None` when the file is missing or does not hold one.
    #[must_use]
    pub fn read_port(&self) -> Option<u16> {
        read_number(&self.port_file())
    }

    /// The process id in `server.pid`, or `None` when the file is missing or does not hold one.
    #[must_use]
    pub fn read_pid(&self) -> Option<u32> {
        read_number(&self.pid_file())
    }

    /// Writes `server.port`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written.
    pub fn write_port(&self, port: u16) -> io::Result<()> {
        write_whole(&self.port_file(), &port.to_string())
    }

    /// Writes `server.pid`.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written.
    pub fn write_pid(&self, pid: u32) -> io::Result<()> {
        write_whole(&self.pid_file(), &pid.to_string())
    }

    /// Removes `server.port` and `server.pid`. Files already gone are not an error.
    pub fn remove_live_files(&self) {
        for file in [self.port_file(), self.pid_file()] {
            match fs::remove_file(&file) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => tracing::warn!(file = %file.display(), error = %err, "cannot remove"),
            }
        }
    }

    /// Opens `server.log` for a new server's output, and returns it with its length now, where the
    /// new output starts. A log past [`ROTATE_AT`] is moved to `server.log.1` first.
    ///
    /// # Errors
    ///
    /// Returns an error when the log cannot be opened.
    pub fn open_server_log(&self) -> io::Result<(File, u64)> {
        let log = self.server_log();
        if fs::metadata(&log).is_ok_and(|meta| meta.len() > ROTATE_AT) {
            // A failed move only means the log keeps growing.
            let _ = fs::rename(&log, self.dir.join(format!("{SERVER_LOG}.1")));
        }
        let file = OpenOptions::new().create(true).append(true).open(&log)?;
        let start = file.metadata()?.len();
        Ok((file, start))
    }

    /// The last lines the server wrote to its log from byte `start` on, with every secret taken
    /// out. Empty when the log cannot be read.
    #[must_use]
    pub fn server_log_tail(&self, start: u64, secrets: &[&str]) -> String {
        let Ok(mut file) = File::open(self.server_log()) else {
            return String::new();
        };
        let len = file.metadata().map_or(0, |meta| meta.len());
        let from = start.max(len.saturating_sub(TAIL_BYTES)).min(len);
        let mut bytes = Vec::new();
        if file.seek(SeekFrom::Start(from)).is_err() || file.read_to_end(&mut bytes).is_err() {
            return String::new();
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        let tail = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
        redact(&tail, secrets)
    }

    /// Replaces `open.log` with a description of `failure`, with every secret taken out, and
    /// returns its path.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written.
    pub fn write_failure(&self, failure: &Failure, secrets: &[&str]) -> io::Result<PathBuf> {
        let when = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        let mut text = format!(
            "acan open failed\nwhat: {}\nport: {}\ntime: {when} (seconds since 1970-01-01 UTC)\nuser folder: {}\nrun folder: {}\nserver log: {}\n",
            failure.what,
            failure.port,
            failure.user_dir.display(),
            self.dir.display(),
            self.server_log().display(),
        );
        if !failure.log_tail.is_empty() {
            text.push_str("\nthe server's last log lines:\n");
            text.push_str(&failure.log_tail);
            text.push('\n');
        }
        let path = self.open_log();
        fs::write(&path, redact(&text, secrets))?;
        Ok(path)
    }
}

fn read_number<T: FromStr>(file: &Path) -> Option<T> {
    fs::read_to_string(file).ok()?.trim().parse().ok()
}

/// Writes `text` to `file` through a temporary file beside it, so a reader never sees half of it.
fn write_whole(file: &Path, text: &str) -> io::Result<()> {
    let mut temporary = file.as_os_str().to_owned();
    temporary.push(".tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, file)
}

/// What failed in an open, as `open.log` describes it.
#[derive(Debug, Clone)]
pub struct Failure {
    /// What went wrong, in one sentence.
    pub what: String,
    /// The port the open was for.
    pub port: u16,
    /// The user's folder.
    pub user_dir: PathBuf,
    /// The server's last log lines, already redacted. Empty when there are none.
    pub log_tail: String,
}

/// Takes every secret out of `text`: each of `secrets`, and every word shaped like a token or a
/// sign-in code, so one the caller does not know of is taken out as well.
#[must_use]
pub fn redact(text: &str, secrets: &[&str]) -> String {
    const HIDDEN: &str = "<redacted>";
    let mut text = text.to_owned();
    for secret in secrets.iter().filter(|secret| !secret.is_empty()) {
        text = text.replace(secret, HIDDEN);
    }
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if secret_shaped(word) {
            out.push_str(HIDDEN);
        } else {
            out.push_str(word);
        }
        word.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// Whether `word` looks like an owner or agent token (`acan_` and 32 hex digits) or a sign-in code
/// (32 hex digits).
fn secret_shaped(word: &str) -> bool {
    let hex = word.strip_prefix("acan_").unwrap_or(word);
    hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// What answers on a port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// This user's acan server: it accepts the owner's token.
    Ours,
    /// Something that is not this user's acan server.
    Other,
    /// Nothing answers.
    Silent,
}

/// The HTTP client the probes use. It never goes through a proxy, since it only talks to loopback.
///
/// # Errors
///
/// Returns an error when the client cannot be built.
pub fn http_client() -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(PROBE_TIMEOUT)
        .build()
}

fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Asks what answers on `port`: `GET /version` must name acan, and `POST /api/signin` with the
/// owner's `token` must succeed, which only the server of this user's folder does.
pub async fn probe(client: &reqwest::Client, port: u16, token: Option<&str>) -> Answer {
    let base = base_url(port);
    let version = match client.get(format!("{base}/version")).send().await {
        Ok(response) => response,
        Err(_) => return Answer::Silent,
    };
    if !version.status().is_success() {
        return Answer::Other;
    }
    let named = version
        .json::<Value>()
        .await
        .is_ok_and(|info| info["name"] == "acan");
    let Some(token) = token.filter(|_| named) else {
        return Answer::Other;
    };
    match client
        .post(format!("{base}/api/signin"))
        .bearer_auth(token)
        .send()
        .await
    {
        Ok(response) if response.status() == reqwest::StatusCode::OK => Answer::Ours,
        _ => Answer::Other,
    }
}

/// A new sign-in code from the server on `port`, asked for with the owner's `token`.
///
/// # Errors
///
/// Returns an error when the request fails or the answer holds no code. The error never holds
/// the code or the token.
pub async fn sign_in_code(
    client: &reqwest::Client,
    port: u16,
    token: &str,
) -> anyhow::Result<String> {
    let response = client
        .post(format!("{}/api/signin", base_url(port)))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("the server did not answer the sign-in request"))?;
    let status = response.status();
    anyhow::ensure!(
        status.is_success(),
        "the server refused the sign-in request ({status})"
    );
    let body: Value = response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("the sign-in answer is not JSON"))?;
    body["code"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("the sign-in answer holds no code"))
}

/// What happened to the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Browser {
    /// It was asked to open the page.
    Opened,
    /// It was not asked, since `--no-browser` was given or [`BROWSER_VAR`] is empty.
    Skipped,
    /// It could not be asked. The reason names no address, since the address carries a code.
    Failed(String),
}

/// Opens `url` in the default browser, or with the program [`BROWSER_VAR`] names.
#[must_use]
pub fn open_browser(url: &str) -> Browser {
    let mut command = match std::env::var_os(BROWSER_VAR) {
        Some(program) if program.is_empty() => return Browser::Skipped,
        Some(program) => {
            let mut command = Command::new(program);
            command.arg(url);
            command
        }
        None => default_browser_command(url),
    };
    let program = command.get_program().to_string_lossy().into_owned();
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(err) => return Browser::Failed(format!("{program} did not start: {err}")),
    };
    // Most openers hand the address on and exit at once. A program that keeps running is taken
    // as the browser itself, and is left to run.
    let deadline = Instant::now() + BROWSER_WAIT;
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Browser::Opened,
            Ok(Some(status)) => return Browser::Failed(format!("{program} exited with {status}")),
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => return Browser::Failed(format!("{program}: {err}")),
        }
    }
    Browser::Opened
}

/// The system's own way to open an address.
fn default_browser_command(url: &str) -> Command {
    if cfg!(windows) {
        // rundll32 hands the address to the default browser with no shell in between, so nothing
        // in it is quoted or read as a command.
        let mut command = Command::new("rundll32.exe");
        command.arg("url.dll,FileProtocolHandler").arg(url);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(url);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    }
}

/// Starts `exe` with `args` detached from this process, in `cwd`, with no input and its output
/// appended to `log`. It keeps running after this process exits, and a Ctrl-C in this process's
/// terminal does not reach it.
///
/// # Errors
///
/// Returns an error when the process cannot be started.
pub fn spawn_detached(exe: &Path, args: &[OsString], cwd: &Path, log: &File) -> io::Result<Child> {
    let mut command = Command::new(exe);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log.try_clone()?));
    spawn_detached_command(&mut command)
}

#[cfg(windows)]
fn spawn_detached_command(command: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;

    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    // Leaving the launcher's job keeps the server alive when the job is closed, such as when an
    // installer or a terminal ends. A job that forbids leaving refuses with access denied.
    match command
        .creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
    {
        Err(err) if err.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            command.creation_flags(flags).spawn()
        }
        spawned => spawned,
    }
}

#[cfg(unix)]
fn spawn_detached_command(command: &mut Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;

    // Its own process group keeps the terminal's Ctrl-C, which goes to the foreground group, away
    // from the server.
    command.process_group(0).spawn()
}

#[cfg(not(any(unix, windows)))]
fn spawn_detached_command(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

/// What `acan open` was asked for.
#[derive(Debug, Clone)]
pub struct Request {
    /// The port asked for with `--port`, if any.
    pub port: Option<u16>,
    /// Whether to open the browser.
    pub browser: bool,
    /// The arguments passed on to a new server, after its subcommand and port.
    pub server_args: Vec<OsString>,
}

/// An open that succeeded.
#[derive(Debug, Clone)]
pub struct Opened {
    /// The port the server listens on.
    pub port: u16,
    /// Whether a running server was used rather than a new one started.
    pub reused: bool,
    /// What happened to the browser.
    pub browser: Browser,
}

/// An open that failed.
#[derive(Debug)]
pub struct OpenFailed {
    /// What went wrong, in one sentence, with no secret in it.
    pub what: String,
    /// The `open.log` that describes it, when one could be written.
    pub log: Option<PathBuf>,
}

/// Opens acan: uses the server this user runs, or starts one detached and waits until it answers,
/// then opens the browser on a sign-in link unless asked not to.
///
/// # Errors
///
/// Returns [`OpenFailed`] when no server answers. `open.log` then describes why, whenever the run
/// folder could be made.
pub async fn open(request: &Request) -> Result<Opened, OpenFailed> {
    let early = |what: String| OpenFailed { what, log: None };
    let user = user_dir().ok_or_else(|| {
        early(
            "there is no user folder for the settings and keys; set HOME, or APPDATA on Windows"
                .to_owned(),
        )
    })?;
    let run = run_dir().ok_or_else(|| {
        early(
            "there is no run folder; set HOME or XDG_STATE_HOME, or LOCALAPPDATA on Windows"
                .to_owned(),
        )
    })?;
    let run = RunDir::create(run.clone()).map_err(|err| {
        early(format!(
            "cannot create the run folder {}: {err}",
            run.display()
        ))
    })?;
    let port = request.port.unwrap_or(DEFAULT_PORT);
    let mut launch = Launch {
        run,
        user,
        port,
        log_start: None,
    };
    let client =
        http_client().map_err(|err| launch.fail(format!("cannot build the HTTP client: {err}")))?;

    if let Some(running) = launch.find_running(&client).await? {
        return Ok(launch.opened(&client, running, true, request.browser).await);
    }
    match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))) {
        Ok(listener) => drop(listener),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            return Err(launch.fail(format!(
                "port {port} on 127.0.0.1 is in use by another program, so the acan server cannot listen there ({err})"
            )));
        }
        // Any other bind error is the server's to report, in its own log.
        Err(_) => {}
    }
    let child = launch.spawn(request)?;
    let port = launch.wait_started(&client, child).await?;
    Ok(launch.opened(&client, port, false, request.browser).await)
}

/// One open in progress.
struct Launch {
    run: RunDir,
    user: PathBuf,
    port: u16,
    /// Where this start's output begins in `server.log`, once a server was started.
    log_start: Option<u64>,
}

impl Launch {
    /// Writes `open.log` for `what` and returns the failure.
    fn fail(&self, what: String) -> OpenFailed {
        let token = owner_token(&self.user);
        let secrets: Vec<&str> = token.iter().map(String::as_str).collect();
        let log_tail = self
            .log_start
            .map(|start| self.run.server_log_tail(start, &secrets))
            .unwrap_or_default();
        let failure = Failure {
            what: what.clone(),
            port: self.port,
            user_dir: self.user.clone(),
            log_tail,
        };
        let log = self.run.write_failure(&failure, &secrets).ok();
        OpenFailed { what, log }
    }

    /// The port of this user's running server, if one runs. While a server holds the lock but does
    /// not answer yet, another launcher is starting it, so this waits for it.
    async fn find_running(&self, client: &reqwest::Client) -> Result<Option<u16>, OpenFailed> {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(port) = self.answering(client).await {
                return Ok(Some(port));
            }
            let held = self.run.lock_held().map_err(|err| {
                self.fail(format!(
                    "cannot open {}: {err}",
                    self.run.lock_file().display()
                ))
            })?;
            if !held {
                return Ok(None);
            }
            if Instant::now() >= deadline {
                return Err(self.fail(format!(
                    "another acan server holds {} but did not answer within {} seconds",
                    self.run.lock_file().display(),
                    START_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The port in `server.port`, when this user's server answers on it.
    async fn answering(&self, client: &reqwest::Client) -> Option<u16> {
        let port = self.run.read_port()?;
        let token = owner_token(&self.user);
        (probe(client, port, token.as_deref()).await == Answer::Ours).then_some(port)
    }

    /// Starts the server detached.
    fn spawn(&mut self, request: &Request) -> Result<Child, OpenFailed> {
        let (log, start) = self.run.open_server_log().map_err(|err| {
            self.fail(format!(
                "cannot open {}: {err}",
                self.run.server_log().display()
            ))
        })?;
        self.log_start = Some(start);
        let exe = std::env::current_exe()
            .map_err(|err| self.fail(format!("cannot find this program's path: {err}")))?;
        let mut args: Vec<OsString> = vec![
            SERVER_COMMAND.into(),
            "--port".into(),
            self.port.to_string().into(),
        ];
        args.extend(request.server_args.iter().cloned());
        spawn_detached(&exe, &args, self.run.path(), &log)
            .map_err(|err| self.fail(format!("cannot start {}: {err}", exe.display())))
    }

    /// Waits until a server of this user answers, and returns its port. The started `child` may
    /// stop early because another launcher's server won the lock; that server is then waited for.
    async fn wait_started(
        &self,
        client: &reqwest::Client,
        mut child: Child,
    ) -> Result<u16, OpenFailed> {
        let deadline = Instant::now() + START_TIMEOUT;
        let mut exited: Option<ExitStatus> = None;
        loop {
            if let Some(port) = self.answering(client).await {
                return Ok(port);
            }
            if exited.is_none() {
                exited = child.try_wait().ok().flatten();
            }
            if let Some(status) = exited
                && !self.run.lock_held().unwrap_or(false)
            {
                return Err(self.fail(format!(
                    "the acan server stopped before it answered ({status}); its log lines below say why"
                )));
            }
            if Instant::now() >= deadline {
                if exited.is_none() {
                    // A server that never answered is stopped, so the next open starts afresh.
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(self.fail(format!(
                    "the acan server did not answer on port {} within {} seconds",
                    self.port,
                    START_TIMEOUT.as_secs()
                )));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// The success, once the server on `port` answers: opens the browser when asked.
    async fn opened(
        &self,
        client: &reqwest::Client,
        port: u16,
        reused: bool,
        browser: bool,
    ) -> Opened {
        let browser = if browser {
            self.open_browser(client, port).await
        } else {
            Browser::Skipped
        };
        Opened {
            port,
            reused,
            browser,
        }
    }

    async fn open_browser(&self, client: &reqwest::Client, port: u16) -> Browser {
        if std::env::var_os(BROWSER_VAR).is_some_and(|program| program.is_empty()) {
            return Browser::Skipped;
        }
        let Some(token) = owner_token(&self.user) else {
            return Browser::Failed("the user folder holds no owner token".to_owned());
        };
        match sign_in_code(client, port, &token).await {
            Ok(code) => open_browser(&format!("{}/#signin={code}", base_url(port))),
            Err(err) => Browser::Failed(err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;

    use super::{Failure, RunDir, redact, run_dir_from};

    const TOKEN: &str = "acan_0123456789abcdef0123456789abcdef";
    const CODE: &str = "fedcba9876543210fedcba9876543210";

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(held, _)| held == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn the_run_folder_follows_the_platform_variables() {
        let all = [
            ("LOCALAPPDATA", "/local"),
            ("XDG_STATE_HOME", "/state"),
            ("HOME", "/users/me"),
        ];
        assert_eq!(
            run_dir_from(true, vars(&all)),
            Some(PathBuf::from("/local/Acan"))
        );
        assert_eq!(
            run_dir_from(false, vars(&all)),
            Some(PathBuf::from("/state/acan"))
        );
        assert_eq!(
            run_dir_from(false, vars(&[("HOME", "/users/me")])),
            Some(PathBuf::from("/users/me/.local/state/acan"))
        );
        assert_eq!(
            run_dir_from(
                false,
                vars(&[("XDG_STATE_HOME", "state"), ("HOME", "/users/me")])
            ),
            Some(PathBuf::from("/users/me/.local/state/acan")),
            "a relative XDG_STATE_HOME is ignored"
        );
        assert_eq!(run_dir_from(true, vars(&[("HOME", "/users/me")])), None);
        assert_eq!(run_dir_from(false, vars(&[])), None);
    }

    #[test]
    fn the_port_and_pid_files_round_trip_and_are_removed() {
        let root = tempfile::tempdir().expect("temp dir");
        let run = RunDir::create(root.path().join("run")).expect("the run folder");
        assert_eq!(run.read_port(), None);
        assert_eq!(run.read_pid(), None);
        run.write_port(40123).expect("the port is written");
        run.write_pid(4242).expect("the pid is written");
        assert_eq!(run.read_port(), Some(40123));
        assert_eq!(run.read_pid(), Some(4242));
        fs::write(run.port_file(), "not a port").expect("a broken port file");
        assert_eq!(run.read_port(), None, "a broken file holds no port");
        run.remove_live_files();
        assert!(!run.port_file().exists() && !run.pid_file().exists());
        run.remove_live_files();
    }

    #[test]
    fn only_one_handle_holds_the_lock() {
        let root = tempfile::tempdir().expect("temp dir");
        let run = RunDir::create(root.path().to_path_buf()).expect("the run folder");
        let first = run.try_lock().expect("the lock opens");
        assert!(first.is_some(), "the first handle takes the lock");
        assert!(run.try_lock().expect("the lock opens").is_none());
        assert!(run.lock_held().expect("the lock opens"));
        drop(first);
        assert!(
            !run.lock_held().expect("the lock opens"),
            "dropping releases it"
        );
        assert!(run.try_lock().expect("the lock opens").is_some());
    }

    #[test]
    fn the_failure_log_holds_no_token() {
        let root = tempfile::tempdir().expect("temp dir");
        let run = RunDir::create(root.path().to_path_buf()).expect("the run folder");
        fs::write(
            run.server_log(),
            format!("old line\nowner token: {TOKEN}\nError: binding 127.0.0.1:40123\n"),
        )
        .expect("a server log");
        let tail = run.server_log_tail(0, &[TOKEN]);
        assert!(tail.contains("binding 127.0.0.1:40123"), "the tail: {tail}");
        let failure = Failure {
            what: format!("the server stopped, token {TOKEN}"),
            port: 40123,
            user_dir: root.path().join("user"),
            log_tail: tail,
        };
        let path = run
            .write_failure(&failure, &[TOKEN])
            .expect("open.log is written");
        assert_eq!(path, run.open_log());
        let text = fs::read_to_string(&path).expect("open.log reads");
        assert!(!text.contains(TOKEN), "the token reached open.log: {text}");
        assert!(text.contains("the server stopped"), "{text}");
        assert!(text.contains("port: 40123"), "{text}");
        assert!(
            text.contains(&run.server_log().display().to_string()),
            "{text}"
        );
        assert!(text.contains("binding 127.0.0.1:40123"), "{text}");
    }

    #[test]
    fn the_log_tail_starts_where_this_start_began() {
        let root = tempfile::tempdir().expect("temp dir");
        let run = RunDir::create(root.path().to_path_buf()).expect("the run folder");
        fs::write(run.server_log(), "an earlier start\n").expect("a server log");
        let (_log, start) = run.open_server_log().expect("the log opens");
        fs::write(run.server_log(), "an earlier start\nthis start\n").expect("more output");
        assert_eq!(run.server_log_tail(start, &[]), "this start");
    }

    #[test]
    fn token_shaped_words_are_redacted_even_when_unknown() {
        let text =
            format!("token={TOKEN} #signin={CODE} commit 0123456789abcdef0123456789abcdef01234567");
        let redacted = redact(&text, &[]);
        assert!(
            !redacted.contains(TOKEN) && !redacted.contains(CODE),
            "{redacted}"
        );
        assert!(redacted.contains("token=<redacted>"), "{redacted}");
        assert!(redacted.contains("#signin=<redacted>"), "{redacted}");
        assert!(
            redacted.contains("0123456789abcdef0123456789abcdef01234567"),
            "a commit hash is not a secret: {redacted}"
        );
        assert_eq!(
            redact("key sk-secret-123 here", &["sk-secret-123"]),
            "key <redacted> here"
        );
    }
}
