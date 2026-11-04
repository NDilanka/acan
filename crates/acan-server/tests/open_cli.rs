//! `acan open` as the installer runs it: the real binary, a detached server, and a sign-in link.
//!
//! Each test gives the binary its own home, config, state and data folders in a temporary
//! directory, a free port, and a stand-in browser that records the address it was given. Every
//! server a test starts is stopped by its process id when the test ends, even when it fails.
#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::{Value, json};

/// One user's isolated folders, and the server it may start.
struct User {
    root: tempfile::TempDir,
    port: u16,
}

impl User {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let port = free_port();
        let user = Self { root, port };
        for dir in ["home", "config", "state", "data", "appdata", "localappdata"] {
            fs::create_dir_all(user.root.path().join(dir)).expect("a folder");
        }
        let browser = user.root.path().join("browser.sh");
        fs::write(
            &browser,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$1\" >> '{}'\n",
                user.browser_record().display()
            ),
        )
        .expect("the stand-in browser");
        fs::set_permissions(&browser, fs::Permissions::from_mode(0o755)).expect("executable");
        user
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// The user's folder: the keys, settings and tokens.
    fn user_dir(&self) -> PathBuf {
        self.path("config").join("acan")
    }

    /// The run folder: the lock, log, port and pid files.
    fn run_dir(&self) -> PathBuf {
        self.path("state").join("acan")
    }

    fn browser_record(&self) -> PathBuf {
        self.path("browser.txt")
    }

    /// The addresses the stand-in browser was given, in order.
    fn browsed(&self) -> Vec<String> {
        fs::read_to_string(self.browser_record())
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// Runs `acan open` with `args` and the stand-in browser, and waits for the launcher to exit.
    fn open(&self, args: &[&str]) -> Output {
        self.open_with_browser(args, &self.path("browser.sh").display().to_string())
    }

    fn open_with_browser(&self, args: &[&str], browser: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_acan"))
            .arg("open")
            .args(["--no-vision", "--workers", "0"])
            .args(args)
            .env("HOME", self.path("home"))
            .env("XDG_CONFIG_HOME", self.path("config"))
            .env("XDG_STATE_HOME", self.path("state"))
            .env("XDG_DATA_HOME", self.path("data"))
            .env("APPDATA", self.path("appdata"))
            .env("LOCALAPPDATA", self.path("localappdata"))
            .env("ACAN_BROWSER", browser)
            .env("ACAN_UPDATE_CHECK", "0")
            .env_remove("ACAN_KEYS_FILE")
            .current_dir(self.root.path())
            .output()
            .expect("acan open runs")
    }

    fn port_arg(&self) -> String {
        self.port.to_string()
    }

    fn pid(&self) -> Option<u32> {
        read_number(&self.run_dir().join("server.pid"))
    }

    fn served_port(&self) -> Option<u16> {
        read_number(&self.run_dir().join("server.port"))
    }

    fn owner_token(&self) -> String {
        let text = fs::read_to_string(self.user_dir().join("tokens.json")).expect("tokens.json");
        let entries: Value = serde_json::from_str(&text).expect("tokens.json parses");
        entries
            .as_array()
            .expect("a list of tokens")
            .iter()
            .find(|entry| entry["principal"].to_string().contains("owner"))
            .and_then(|entry| entry["token"].as_str())
            .expect("the owner's token")
            .to_owned()
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// Stops the server of a user, by the pid it wrote, when the test ends.
struct Stop<'a>(&'a User);

impl Drop for Stop<'_> {
    fn drop(&mut self) {
        if let Some(pid) = self.0.pid() {
            terminate(pid);
            if !wait_until(Duration::from_secs(15), || !alive(pid)) {
                let _ = Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
    }
}

fn terminate(pid: u32) {
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(status.success(), "the server {pid} takes SIGTERM");
}

fn alive(pid: u32) -> bool {
    // A stopped server that was never waited for lingers as a zombie, which kill -0 still finds.
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|stat| !stat.contains(") Z "))
        .unwrap_or(false)
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    done()
}

fn read_number<T: std::str::FromStr>(file: &Path) -> Option<T> {
    fs::read_to_string(file).ok()?.trim().parse().ok()
}

/// A port nothing listens on now. Never the ports a developer's own servers use.
fn free_port() -> u16 {
    loop {
        let listener = TcpListener::bind("127.0.0.1:0").expect("an ephemeral port");
        let port = listener.local_addr().expect("its address").port();
        if port != 7311 && port != 8841 {
            return port;
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("a client")
}

/// Checks that no output or log file holds `secret`.
fn assert_nowhere(user: &User, outputs: &[&Output], secret: &str) {
    for output in outputs {
        assert!(
            !text(&output.stdout).contains(secret),
            "stdout holds a secret"
        );
        assert!(
            !text(&output.stderr).contains(secret),
            "stderr holds a secret"
        );
    }
    for file in ["server.log", "open.log"] {
        let log = fs::read_to_string(user.run_dir().join(file)).unwrap_or_default();
        assert!(!log.contains(secret), "{file} holds a secret: {log}");
    }
}

#[tokio::test]
async fn open_starts_a_detached_server_and_a_second_open_reuses_it() {
    let user = User::new();
    let _stop = Stop(&user);

    let first = user.open(&["--port", &user.port_arg(), "--no-browser"]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "the first open succeeds: {}{}",
        text(&first.stdout),
        text(&first.stderr)
    );
    assert_eq!(
        text(&first.stdout).trim(),
        format!("acan is open at http://127.0.0.1:{}/", user.port)
    );
    assert_eq!(
        user.served_port(),
        Some(user.port),
        "server.port names the port"
    );
    let pid = user.pid().expect("server.pid names the server");
    assert!(alive(pid), "the server outlives the launcher");

    let version: Value = client()
        .get(format!("{}/version", user.base()))
        .send()
        .await
        .expect("the server answers after the launcher exited")
        .json()
        .await
        .expect("the version is JSON");
    assert_eq!(version["name"], "acan");
    let token = user.owner_token();
    let projects: Value = client()
        .get(format!("{}/api/projects", user.base()))
        .bearer_auth(&token)
        .send()
        .await
        .expect("the projects answer")
        .json()
        .await
        .expect("the projects are JSON");
    assert_eq!(projects, json!([]), "no project runs yet");

    let second = user.open(&["--port", &user.port_arg(), "--no-browser"]);
    assert_eq!(second.status.code(), Some(0), "the second open succeeds");
    assert_eq!(
        user.pid(),
        Some(pid),
        "the second open uses the same server"
    );
    assert_eq!(
        text(&second.stdout).trim(),
        format!("acan is open at http://127.0.0.1:{}/", user.port)
    );

    let other = free_port();
    let third = user.open(&["--port", &other.to_string(), "--no-browser"]);
    assert_eq!(
        third.status.code(),
        Some(0),
        "an open on another port succeeds"
    );
    assert_eq!(user.pid(), Some(pid), "and still uses the one server");
    let said = text(&third.stdout);
    assert!(
        said.contains(&format!("already running on port {}", user.port)),
        "it says which server it used: {said}"
    );
    assert!(said.contains(&format!("acan is open at http://127.0.0.1:{}/", user.port)));
    assert!(user.browsed().is_empty(), "--no-browser opens no browser");
    assert_nowhere(&user, &[&first, &second, &third], &token);

    terminate(pid);
    assert!(
        wait_until(Duration::from_secs(15), || !alive(pid)),
        "the server stops on SIGTERM"
    );
    assert!(
        !user.run_dir().join("server.port").exists(),
        "server.port is removed"
    );
    assert!(
        !user.run_dir().join("server.pid").exists(),
        "server.pid is removed"
    );
}

#[tokio::test]
async fn the_browser_gets_a_sign_in_code_that_works_once() {
    let user = User::new();
    let _stop = Stop(&user);

    let opened = user.open(&["--port", &user.port_arg()]);
    assert_eq!(
        opened.status.code(),
        Some(0),
        "the open succeeds: {}{}",
        text(&opened.stdout),
        text(&opened.stderr)
    );
    let browsed = user.browsed();
    assert_eq!(browsed.len(), 1, "the browser is opened once: {browsed:?}");
    let prefix = format!("http://127.0.0.1:{}/#signin=", user.port);
    let code = browsed[0]
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("the address carries a sign-in code: {}", browsed[0]))
        .to_owned();
    assert!(!code.is_empty());

    let exchange = |code: &str| {
        client()
            .post(format!("{}/api/signin/exchange", user.base()))
            .json(&json!({ "code": code }))
            .send()
    };
    let response = exchange(&code).await.expect("the exchange answers");
    assert_eq!(response.status(), StatusCode::OK, "the code is exchanged");
    let body: Value = response.json().await.expect("the exchange is JSON");
    let token = user.owner_token();
    assert_eq!(
        body["token"].as_str(),
        Some(token.as_str()),
        "it gives the owner's token"
    );
    let again = exchange(&code).await.expect("the exchange answers");
    assert_eq!(
        again.status(),
        StatusCode::UNAUTHORIZED,
        "a code works once"
    );

    let quiet = user.open(&["--no-browser"]);
    assert_eq!(quiet.status.code(), Some(0));
    let silent = user.open_with_browser(&[], "");
    assert_eq!(silent.status.code(), Some(0));
    assert_eq!(
        user.browsed().len(),
        1,
        "no browser opened the second and third time"
    );

    assert_nowhere(&user, &[&opened, &quiet, &silent], &token);
    assert_nowhere(&user, &[&opened, &quiet, &silent], &code);
}

#[tokio::test]
async fn a_busy_port_fails_and_says_why_in_open_log() {
    let user = User::new();
    let _stop = Stop(&user);
    let _busy = TcpListener::bind(("127.0.0.1", user.port)).expect("the port is held");

    let failed = user.open(&["--port", &user.port_arg()]);
    assert_eq!(failed.status.code(), Some(1), "the open fails");
    let open_log = user.run_dir().join("open.log");
    let stderr = text(&failed.stderr);
    assert_eq!(
        stderr.trim().lines().count(),
        1,
        "one line on stderr: {stderr}"
    );
    assert!(
        stderr.contains(&open_log.display().to_string()),
        "stderr names open.log: {stderr}"
    );
    assert!(text(&failed.stdout).is_empty(), "nothing on stdout");
    let log = fs::read_to_string(&open_log).expect("open.log is written");
    assert!(
        log.contains(&format!("port {} on 127.0.0.1 is in use", user.port)),
        "open.log names the problem: {log}"
    );
    assert!(user.pid().is_none(), "no server runs");
    assert!(user.browsed().is_empty(), "no browser opens");
    if let Ok(tokens) = fs::read_to_string(user.user_dir().join("tokens.json")) {
        assert!(!log.contains(&tokens), "open.log holds no token");
    }
}

#[tokio::test]
async fn a_server_that_stops_at_start_fails_with_its_log_lines() {
    let user = User::new();
    let _stop = Stop(&user);
    let missing = user.path("no-such-web-folder").display().to_string();

    let failed = user.open(&["--port", &user.port_arg(), "--web", &missing]);
    assert_eq!(failed.status.code(), Some(1), "the open fails");
    let log = fs::read_to_string(user.run_dir().join("open.log")).expect("open.log is written");
    assert!(
        log.contains("stopped before it answered"),
        "open.log says the server stopped: {log}"
    );
    assert!(
        log.contains("no-such-web-folder"),
        "open.log holds the server's reason: {log}"
    );
    assert!(user.pid().is_none(), "no server runs");
    assert!(
        !user.run_dir().join("server.lock").exists() || {
            let lock = fs::File::open(user.run_dir().join("server.lock")).expect("the lock opens");
            lock.try_lock().is_ok()
        },
        "the lock is free"
    );
}
