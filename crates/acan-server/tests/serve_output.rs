//! What `acan serve` prints on its first start, run as the real binary with its output piped, so
//! stdout is not a terminal.

use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long a test waits for the server to finish starting.
const WAIT: Duration = Duration::from_secs(60);

/// A running `acan serve`, stopped when the test ends.
struct Serving(Child);

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_first_start_with_piped_output_names_the_tokens_file_and_prints_no_token() {
    let repo = tempfile::tempdir().expect("temp dir");
    fs::create_dir(repo.path().join(".acan")).expect("state dir");
    fs::write(
        repo.path().join(".acan/llm.toml"),
        "untrusted invalid routing",
    )
    .expect("repository routing file");
    let child = Command::new(env!("CARGO_BIN_EXE_acan"))
        .arg("serve")
        .arg("--repo")
        .arg(repo.path())
        .args([
            "--port",
            "0",
            "--no-update-check",
            "--no-vision",
            "--workers",
            "0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("acan starts");
    let mut serving = Serving(child);

    let stdout = serving.0.stdout.take().expect("piped stdout");
    let (lines_tx, lines_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let stderr = serving.0.stderr.take().expect("piped stderr");
    let errors = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = BufReader::new(stderr).read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });

    // The server prints its address last, after everything about the owner token.
    let deadline = Instant::now() + WAIT;
    let mut printed = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let line = lines_rx
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("acan did not finish starting; output so far: {printed:?}"));
        let serving_line = line.starts_with("acan serving");
        printed.push(line);
        if serving_line {
            break;
        }
    }
    serving.0.kill().expect("acan stops");
    serving.0.wait().expect("acan exits");
    let stderr = errors.join().expect("the stderr reader finishes");

    let tokens_path = repo.path().join(".acan").join("tokens.json");
    let entries: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&tokens_path).expect("the first start writes the tokens file"),
    )
    .expect("the tokens file is JSON");
    let token = entries[0]["token"]
        .as_str()
        .expect("the owner entry holds a token")
        .to_owned();

    let stdout = printed.join("\n");
    assert!(
        printed.iter().any(|line| line.starts_with("owner token")),
        "the first start says where the owner token is: {stdout}"
    );
    assert!(
        !stdout.contains(&token),
        "the owner token reached stdout: {stdout}"
    );
    assert!(
        !stderr.contains(&token),
        "the owner token reached stderr: {stderr}"
    );
    assert!(
        !stdout.contains("#token="),
        "a sign-in link carries the token: {stdout}"
    );
    assert!(
        stdout.contains(&tokens_path.display().to_string()),
        "the tokens file is not named: {stdout}"
    );
}
