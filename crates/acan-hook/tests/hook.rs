//! End-to-end tests. Each test runs the real `acan-hook` binary against a fake daemon, so the
//! exit codes and stderr are what Droid would see.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use acan_hook::{ENDPOINT_ENV, ORDER_ENV, TIMEOUT_ENV};
use serde_json::Value;

const ORDER: &str = "order-1";
const ALLOW: &str = r#"{"version":1,"decision":"allow"}"#;
const PRE_TOOL_USE: &str = r#"{"session_id":"sess-1","cwd":"/work/repo","hook_event_name":"PreToolUse","tool_name":"Create","tool_input":{"file_path":"src/a.txt","content":"x"}}"#;
const SESSION_START: &str = r#"{"session_id":"sess-1","cwd":"/work/repo","hook_event_name":"SessionStart","source":"startup"}"#;
const OUTER_SCRIPT: &str = r#"{"session_id":"sess-1","hook_event_name":"PreToolUse","tool_name":"Script","tool_input":{"script":"Create({file_path: 'src/a.txt'})"}}"#;
/// A hook that gave up at its deadline exits well inside this bound, process start included.
const HOOK_EXIT_LIMIT: Duration = Duration::from_millis(1500);

/// What one hook run produced.
struct Run {
    code: Option<i32>,
    stderr: String,
    elapsed: Duration,
}

/// Runs the hook with a clean Acan environment plus `env`, feeding `input` on stdin.
fn run_hook(env: &[(&str, &str)], input: &str) -> Run {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_acan-hook"));
    cmd.env_remove(ORDER_ENV)
        .env_remove(ENDPOINT_ENV)
        .env_remove(TIMEOUT_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        cmd.env(key, value);
    }
    let start = Instant::now();
    let mut child = cmd.spawn().expect("spawn acan-hook");
    if let Some(mut stdin) = child.stdin.take() {
        // The hook may exit without reading stdin (outside an Acan session), so a broken pipe is fine.
        let _ = stdin.write_all(input.as_bytes());
    }
    let output = child.wait_with_output().expect("wait for acan-hook");
    Run {
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        elapsed: start.elapsed(),
    }
}

/// The environment of an Acan session that talks to `endpoint`.
fn acan_env(endpoint: &str) -> Vec<(&'static str, &str)> {
    vec![(ORDER_ENV, ORDER), (ENDPOINT_ENV, endpoint)]
}

/// A loopback listener nobody answers. `assert_no_contact` later proves that nothing connected.
fn untouched_listener() -> (TcpListener, String) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback port");
    let endpoint = format!("tcp://{}", listener.local_addr().expect("local address"));
    (listener, endpoint)
}

fn assert_no_contact(listener: &TcpListener) {
    listener.set_nonblocking(true).expect("set nonblocking");
    let contact = listener.accept();
    assert!(
        matches!(&contact, Err(err) if err.kind() == io::ErrorKind::WouldBlock),
        "the hook contacted the daemon: {contact:?}"
    );
}

/// A loopback port with nothing listening on it.
fn closed_tcp_endpoint() -> String {
    let (listener, endpoint) = untouched_listener();
    drop(listener);
    endpoint
}

/// A local endpoint that does not exist.
#[cfg(windows)]
fn missing_local_endpoint() -> String {
    format!(r"\\.\pipe\acan-hook-missing-{}", std::process::id())
}

/// A local endpoint that does not exist.
#[cfg(unix)]
fn missing_local_endpoint() -> String {
    std::env::temp_dir()
        .join(format!("acan-hook-missing-{}.sock", std::process::id()))
        .display()
        .to_string()
}

/// Serves one connection: reads the request line, sends `reply` plus a newline, and returns the
/// request line.
fn answer<S: Read + Write>(stream: &mut S, reply: &str) -> String {
    let mut request = String::new();
    BufReader::new(&mut *stream)
        .read_line(&mut request)
        .expect("read the request line");
    stream
        .write_all(format!("{reply}\n").as_bytes())
        .expect("write the reply");
    stream.flush().expect("flush the reply");
    request
}

/// Starts a loopback daemon that answers one hook connection with `reply`. Returns the
/// `tcp://` endpoint and a handle that yields the request line.
fn tcp_daemon(reply: &str) -> (String, thread::JoinHandle<String>) {
    let (listener, endpoint) = untouched_listener();
    let reply = reply.to_owned();
    let daemon = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept the hook");
        answer(&mut stream, &reply)
    });
    (endpoint, daemon)
}

/// A loopback daemon that accepts the hook and never replies.
fn silent_daemon() -> String {
    let (listener, endpoint) = untouched_listener();
    thread::spawn(move || {
        let (_connection, _) = listener.accept().expect("accept the hook");
        // Keep the connection open without replying.
        thread::sleep(Duration::from_secs(30));
    });
    endpoint
}

/// Checks the request line a daemon received for the `PRE_TOOL_USE` write call.
fn assert_write_request(line: &str) {
    let request: Value = serde_json::from_str(line).expect("the request is one JSON line");
    assert_eq!(request["version"], 1);
    assert_eq!(request["kind"], "pre_tool_use");
    assert_eq!(request["order"], ORDER);
    assert_eq!(request["session_id"], "sess-1");
    assert_eq!(request["tool_name"], "Create");
    assert_eq!(request["file_path"], "src/a.txt");
    assert!(
        request["command"].is_null(),
        "command: {}",
        request["command"]
    );
    assert_eq!(request["raw_tool_input"]["content"], "x");
}

#[test]
fn outside_an_acan_session_the_hook_passes_without_reading_or_contacting() {
    let (listener, endpoint) = untouched_listener();
    let unset = run_hook(&[(ENDPOINT_ENV, endpoint.as_str())], "not json");
    assert_eq!(unset.code, Some(0), "stderr: {}", unset.stderr);
    let empty = run_hook(
        &[(ORDER_ENV, ""), (ENDPOINT_ENV, endpoint.as_str())],
        "not json",
    );
    assert_eq!(empty.code, Some(0), "stderr: {}", empty.stderr);
    assert_no_contact(&listener);
}

#[test]
fn missing_endpoint_blocks_an_acan_session() {
    let run = run_hook(&[(ORDER_ENV, ORDER)], PRE_TOOL_USE);
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(run.stderr.contains(ENDPOINT_ENV), "stderr: {}", run.stderr);
}

#[test]
fn bad_endpoint_blocks() {
    for endpoint in [
        "tcp://192.0.2.1:9",
        "tcp://localhost:9",
        "tcp://127.0.0.1:notaport",
    ] {
        let run = run_hook(&acan_env(endpoint), PRE_TOOL_USE);
        assert_eq!(
            run.code,
            Some(2),
            "endpoint {endpoint}; stderr: {}",
            run.stderr
        );
        assert!(
            run.stderr.contains("bad endpoint"),
            "endpoint {endpoint}; stderr: {}",
            run.stderr
        );
    }
}

#[test]
fn bad_stdin_blocks() {
    let endpoint = closed_tcp_endpoint();
    for input in [
        "{not json",
        r#"{"hook_event_name":"Stop"}"#,
        r#"{"hook_event_name":"PreToolUse"}"#,
    ] {
        let run = run_hook(&acan_env(&endpoint), input);
        assert_eq!(run.code, Some(2), "input {input}; stderr: {}", run.stderr);
        assert!(
            run.stderr.contains("bad hook input"),
            "input {input}; stderr: {}",
            run.stderr
        );
    }
}

#[test]
fn daemon_down_blocks_within_the_deadline() {
    for endpoint in [closed_tcp_endpoint(), missing_local_endpoint()] {
        let run = run_hook(&acan_env(&endpoint), PRE_TOOL_USE);
        assert_eq!(
            run.code,
            Some(2),
            "endpoint {endpoint}; stderr: {}",
            run.stderr
        );
        assert!(
            run.stderr.starts_with("acan hook: "),
            "endpoint {endpoint}; stderr: {}",
            run.stderr
        );
        assert!(
            run.elapsed < HOOK_EXIT_LIMIT,
            "endpoint {endpoint}: took {:?}",
            run.elapsed
        );
    }
}

#[test]
fn silent_daemon_times_out_shortly_after_the_deadline() {
    let endpoint = silent_daemon();
    let mut env = acan_env(&endpoint);
    env.push((TIMEOUT_ENV, "100"));
    let run = run_hook(&env, PRE_TOOL_USE);
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("timed out after 100 ms"),
        "stderr: {}",
        run.stderr
    );
    assert!(run.elapsed < HOOK_EXIT_LIMIT, "took {:?}", run.elapsed);
}

#[test]
fn malformed_replies_block() {
    let cases = [
        "",
        "not json",
        r#"{"version":1}"#,
        r#"{"version":1,"decision":"maybe"}"#,
        r#"{"version":2,"decision":"allow"}"#,
    ];
    for reply in cases {
        let (endpoint, _daemon) = tcp_daemon(reply);
        let run = run_hook(&acan_env(&endpoint), PRE_TOOL_USE);
        assert_eq!(run.code, Some(2), "reply {reply:?}; stderr: {}", run.stderr);
        assert!(
            run.stderr.contains("bad daemon reply"),
            "reply {reply:?}; stderr: {}",
            run.stderr
        );
    }
}

#[test]
fn allow_reply_exits_zero_and_sends_the_write_call() {
    let (endpoint, daemon) = tcp_daemon(ALLOW);
    let run = run_hook(&acan_env(&endpoint), PRE_TOOL_USE);
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert_write_request(&daemon.join().expect("daemon thread"));
}

#[test]
fn deny_reply_exits_two_with_the_reason() {
    let (endpoint, _daemon) =
        tcp_daemon(r#"{"version":1,"decision":"deny","reason":"outside scope: notes/b2.txt"}"#);
    let run = run_hook(&acan_env(&endpoint), PRE_TOOL_USE);
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("blocked: outside scope: notes/b2.txt"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn session_start_sends_a_heartbeat_for_the_order() {
    let (endpoint, daemon) = tcp_daemon(ALLOW);
    let run = run_hook(&acan_env(&endpoint), SESSION_START);
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    let request: Value =
        serde_json::from_str(&daemon.join().expect("daemon thread")).expect("JSON request");
    assert_eq!(request["kind"], "session_start");
    assert_eq!(request["order"], ORDER);
    assert_eq!(request["session_id"], "sess-1");
    assert!(request["tool_name"].is_null());
}

#[test]
fn rejected_session_start_blocks() {
    let (endpoint, _daemon) =
        tcp_daemon(r#"{"version":1,"decision":"deny","reason":"no such order"}"#);
    let run = run_hook(&acan_env(&endpoint), SESSION_START);
    assert_eq!(run.code, Some(2), "stderr: {}", run.stderr);
    assert!(
        run.stderr.contains("no such order"),
        "stderr: {}",
        run.stderr
    );
}

#[test]
fn outer_script_batch_passes_without_contacting_the_daemon() {
    let (listener, endpoint) = untouched_listener();
    let run = run_hook(&acan_env(&endpoint), OUTER_SCRIPT);
    assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
    assert_no_contact(&listener);
}

#[cfg(unix)]
mod unix_socket {
    use std::os::unix::net::UnixListener;

    use super::*;

    #[test]
    fn unix_socket_round_trip() {
        let path = std::env::temp_dir().join(format!("acan-hook-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).expect("bind unix socket");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept the hook");
            answer(&mut stream, ALLOW)
        });
        let endpoint = path.display().to_string();
        let run = run_hook(&acan_env(&endpoint), PRE_TOOL_USE);
        let _ = std::fs::remove_file(&path);
        // Check the exit code before joining: a hook that never connected would leave the server
        // blocked in accept.
        assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
        assert_write_request(&server.join().expect("socket server thread"));
    }
}

#[cfg(windows)]
mod named_pipe {
    use std::ffi::c_void;
    use std::fs::File;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::ptr;

    use super::*;

    const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
    /// Returned by `ConnectNamedPipe` when the client connected before the server asked.
    const ERROR_PIPE_CONNECTED: i32 = 535;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateNamedPipeW(
            name: *const u16,
            open_mode: u32,
            pipe_mode: u32,
            max_instances: u32,
            out_buffer_size: u32,
            in_buffer_size: u32,
            default_timeout_ms: u32,
            security: *mut c_void,
        ) -> *mut c_void;
        fn ConnectNamedPipe(pipe: *mut c_void, overlapped: *mut c_void) -> i32;
        fn FlushFileBuffers(file: *mut c_void) -> i32;
        fn DisconnectNamedPipe(pipe: *mut c_void) -> i32;
    }

    /// Creates one byte-mode pipe server instance. The hook's client connects to it.
    fn create_pipe(name: &str) -> File {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is NUL-terminated and alive for the call. The other arguments are plain
        // values, and a null security descriptor means the default.
        let handle = unsafe {
            CreateNamedPipeW(
                wide.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                0,
                1,
                4096,
                4096,
                0,
                ptr::null_mut(),
            )
        };
        assert!(
            handle != ptr::without_provenance_mut(usize::MAX),
            "CreateNamedPipeW failed: {}",
            io::Error::last_os_error()
        );
        // SAFETY: `handle` is a valid pipe handle that nothing else owns; `File` closes it on drop.
        unsafe { File::from_raw_handle(handle) }
    }

    /// Serves one client: waits for it, reads its request, sends the allow reply, and waits
    /// until the client has read the reply before disconnecting.
    fn serve_pipe(mut pipe: File) -> String {
        let raw = pipe.as_raw_handle();
        // SAFETY: `raw` is the live handle that `pipe` owns, and `pipe` outlives this call.
        let connected = unsafe { ConnectNamedPipe(raw, ptr::null_mut()) };
        if connected == 0 {
            let err = io::Error::last_os_error();
            assert_eq!(
                err.raw_os_error(),
                Some(ERROR_PIPE_CONNECTED),
                "ConnectNamedPipe failed: {err}"
            );
        }
        let request = answer(&mut pipe, ALLOW);
        // SAFETY: same live handle. FlushFileBuffers returns once the client has read the reply.
        unsafe {
            FlushFileBuffers(raw);
            DisconnectNamedPipe(raw);
        }
        request
    }

    #[test]
    fn named_pipe_round_trip() {
        let name = format!(r"\\.\pipe\acan-hook-test-{}", std::process::id());
        let pipe = create_pipe(&name);
        let server = thread::spawn(move || serve_pipe(pipe));
        let run = run_hook(&acan_env(&name), PRE_TOOL_USE);
        // Check the exit code before joining: a hook that never connected would leave the server
        // blocked in ConnectNamedPipe.
        assert_eq!(run.code, Some(0), "stderr: {}", run.stderr);
        assert_write_request(&server.join().expect("pipe server thread"));
    }
}
