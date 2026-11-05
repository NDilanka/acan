//! Behavior tests for witness runs: commands, timeouts, HTTP checks, and bounded output.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

use acan_core::{Check, WitnessOutcome};
use acan_witness::{Limits, run};
use tokio::sync::Mutex;

/// Held by every test that binds a loopback port. The closed-port test releases its scratch port
/// before connecting, and a parallel fixture server could otherwise be handed that same port.
static PORTS: Mutex<()> = Mutex::const_new(());

fn limits() -> Limits {
    Limits {
        timeout: Duration::from_secs(20),
        tail_bytes: 1024,
        ready_timeout: Duration::from_millis(500),
        request_timeout: Duration::from_secs(5),
        http_base_url: None,
    }
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after the epoch")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("acan-witness-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create the temp dir");
    dir
}

/// Serves exactly one real HTTP request, then exits. Readiness probes connect and close
/// without sending bytes; those connections are skipped.
fn serve_once(status: u16, body: &'static str) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fixture server");
    let addr = listener.local_addr().expect("the fixture address");
    let server = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.expect("accept a connection");
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).expect("read the request");
            if n == 0 {
                continue;
            }
            let reason = if status == 200 {
                "OK"
            } else {
                "Internal Server Error"
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write the response");
            return;
        }
    });
    (format!("http://{addr}"), server)
}

#[tokio::test]
async fn candidate_test_code_never_executes_on_the_host() {
    let host = tempfile::tempdir().expect("host directory");
    let worktree = tempfile::tempdir().expect("worktree");
    let sentinel = host.path().join("compromised");
    let check = Check::Test {
        command: format!("echo compromised > '{}'", sentinel.display()),
    };
    let outcome = run(&check, worktree.path(), &limits()).await;
    assert!(
        !sentinel.exists(),
        "candidate code wrote outside its container"
    );
    assert!(!matches!(outcome, WitnessOutcome::Pass), "{outcome:?}");
}

#[tokio::test]
async fn a_passing_command_passes() {
    let check = Check::Test {
        command: "exit 0".to_string(),
    };
    let outcome = run(&check, Path::new("."), &limits()).await;
    assert!(matches!(outcome, WitnessOutcome::Pass), "{outcome:?}");
}

#[tokio::test]
async fn a_failing_command_fails_with_the_output_tail() {
    let command = "echo hello-fail; echo oops-stderr 1>&2; exit 3".to_string();
    let check = Check::Test { command };
    let outcome = run(&check, Path::new("."), &limits()).await;
    let WitnessOutcome::Fail { detail } = outcome else {
        panic!("expected Fail, got {outcome:?}");
    };
    assert!(detail.contains("exit code 3"), "{detail}");
    assert!(detail.contains("hello-fail"), "{detail}");
    assert!(detail.contains("oops-stderr"), "{detail}");
}

#[tokio::test]
async fn a_timeout_kills_the_process_tree() {
    let dir = temp_dir("timeout");
    let sentinel = dir.join("written");
    // Every platform now runs the same shell inside the container.
    let command = "sleep 4; echo done > /work/written".to_owned();
    let check = Check::Test { command };
    let limits = Limits {
        timeout: Duration::from_secs(2),
        ..limits()
    };
    let outcome = run(&check, &dir, &limits).await;
    let WitnessOutcome::Error { detail } = outcome else {
        panic!("expected Error, got {outcome:?}");
    };
    assert!(detail.contains("timed out"), "{detail}");
    // If the tree survived, the child would write the sentinel about now.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        !sentinel.exists(),
        "the killed child wrote {}",
        sentinel.display()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_output_tail_is_bounded() {
    let command =
        "printf START; head -c 65536 /dev/zero | tr '\\0' x; printf 'END\\n'; exit 1".to_string();
    let check = Check::Test { command };
    let outcome = run(&check, Path::new("."), &limits()).await;
    let WitnessOutcome::Fail { detail } = outcome else {
        panic!("expected Fail, got {outcome:?}");
    };
    assert!(detail.contains("END"), "the tail keeps the end: {detail}");
    assert!(
        !detail.contains("START"),
        "the tail drops the head: {detail}"
    );
    assert!(
        detail.len() < 1024 + 200,
        "the detail stays bounded: {} bytes",
        detail.len()
    );
}

#[tokio::test]
async fn an_http_check_passes_and_the_server_stops_after() {
    let _ports = PORTS.lock().await;
    let (base, server) = serve_once(200, "all good");
    let limits = Limits {
        http_base_url: Some(base),
        ..limits()
    };
    let check = Check::Http {
        method: "GET".to_string(),
        path: "/ok".to_string(),
        expect_status: 200,
        expect_body: Some("all good".to_string()),
    };
    let outcome = run(&check, Path::new("."), &limits).await;
    assert!(matches!(outcome, WitnessOutcome::Pass), "{outcome:?}");
    server.join().expect("the server stops after the check");
}

#[tokio::test]
async fn an_http_check_fails_on_a_wrong_status() {
    let _ports = PORTS.lock().await;
    let (base, server) = serve_once(500, "broken");
    let limits = Limits {
        http_base_url: Some(base),
        ..limits()
    };
    let check = Check::Http {
        method: "GET".to_string(),
        path: "/boom".to_string(),
        expect_status: 200,
        expect_body: None,
    };
    let outcome = run(&check, Path::new("."), &limits).await;
    let WitnessOutcome::Fail { detail } = outcome else {
        panic!("expected Fail, got {outcome:?}");
    };
    assert!(detail.contains("expected status 200, got 500"), "{detail}");
    server.join().expect("the server stops after the check");
}

#[tokio::test]
async fn an_http_check_fails_on_a_missing_body_substring() {
    let _ports = PORTS.lock().await;
    let (base, server) = serve_once(200, "the actual body");
    let limits = Limits {
        http_base_url: Some(base),
        ..limits()
    };
    let check = Check::Http {
        method: "GET".to_string(),
        path: "/ok".to_string(),
        expect_status: 200,
        expect_body: Some("wanted text".to_string()),
    };
    let outcome = run(&check, Path::new("."), &limits).await;
    let WitnessOutcome::Fail { detail } = outcome else {
        panic!("expected Fail, got {outcome:?}");
    };
    assert!(detail.contains("did not contain"), "{detail}");
    server.join().expect("the server stops after the check");
}

#[tokio::test]
async fn an_http_check_errors_when_nothing_listens() {
    let _ports = PORTS.lock().await;
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind a scratch listener")
        .local_addr()
        .expect("the scratch address")
        .port();
    // The scratch listener is dropped here, leaving the port closed.
    let limits = Limits {
        http_base_url: Some(format!("http://127.0.0.1:{port}")),
        ready_timeout: Duration::from_millis(300),
        ..limits()
    };
    let check = Check::Http {
        method: "GET".to_string(),
        path: "/".to_string(),
        expect_status: 200,
        expect_body: None,
    };
    let outcome = run(&check, Path::new("."), &limits).await;
    let WitnessOutcome::Error { detail } = outcome else {
        panic!("expected Error, got {outcome:?}");
    };
    assert!(detail.contains("did not accept connections"), "{detail}");
}

#[tokio::test]
async fn an_http_check_without_a_base_url_errors() {
    let check = Check::Http {
        method: "GET".to_string(),
        path: "/".to_string(),
        expect_status: 200,
        expect_body: None,
    };
    let outcome = run(&check, Path::new("."), &limits()).await;
    let WitnessOutcome::Error { detail } = outcome else {
        panic!("expected Error, got {outcome:?}");
    };
    assert!(detail.contains("base url"), "{detail}");
}

#[tokio::test]
async fn a_screenshot_check_is_explicitly_unsupported() {
    let check = Check::Screenshot {
        route: "/".to_string(),
        region: None,
    };
    let outcome = run(&check, Path::new("."), &limits()).await;
    let WitnessOutcome::Error { detail } = outcome else {
        panic!("expected Error, got {outcome:?}");
    };
    assert_eq!(detail, "unsupported check kind: screenshot");
}
