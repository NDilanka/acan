//! Local transports from the hook to the daemon.
//!
//! Windows uses a named pipe and Unix a domain socket. Loopback TCP serves tests and
//! diagnostics. Each exchange is one request line and one reply line. The caller enforces the
//! deadline, not this module.
//!
//! An [`Endpoint`] can only come from [`Endpoint::parse`], so its checks cover every value that
//! [`exchange`] receives. A plain file path is refused and never opened.

#[cfg(windows)]
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};

use crate::{ENDPOINT_ENV, HookError};

/// Largest reply the hook accepts, in bytes. A reply is one short JSON line.
pub const MAX_REPLY_BYTES: u64 = 64 * 1024;

/// The Windows named pipe namespace. A local endpoint on Windows must start with it.
#[cfg(windows)]
const PIPE_PREFIX: &str = r"\\.\pipe\";

/// Where the daemon listens. Only [`Endpoint::parse`] builds one, so every value is checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    target: Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// A Windows named pipe path, or a Unix domain socket path.
    Local(PathBuf),
    /// Loopback TCP.
    Tcp(SocketAddr),
}

impl Endpoint {
    /// Reads [`ENDPOINT_ENV`].
    ///
    /// # Errors
    ///
    /// Returns [`HookError::MissingEndpoint`] when the variable is unset or empty, and
    /// [`HookError::BadEndpoint`] when it is not text or not a valid endpoint.
    pub fn from_env() -> Result<Self, HookError> {
        let raw = std::env::var_os(ENDPOINT_ENV)
            .filter(|value| !value.is_empty())
            .ok_or(HookError::MissingEndpoint)?;
        let text = raw
            .into_string()
            .map_err(|_| bad_endpoint("endpoint is not valid UTF-8"))?;
        Self::parse(&text)
    }

    /// Parses an endpoint string. The accepted forms are:
    ///
    /// - `tcp://<ip>:<port>`, with a loopback address;
    /// - on Windows, `\\.\pipe\<name>`, where the name is one non-empty segment;
    /// - on Unix, an absolute socket path.
    ///
    /// # Errors
    ///
    /// Returns [`HookError::BadEndpoint`] for any other value. That covers plain file paths,
    /// UNC paths, relative paths, control characters and non-loopback TCP.
    pub fn parse(raw: &str) -> Result<Self, HookError> {
        if raw.chars().any(char::is_control) {
            return Err(bad_endpoint("it contains a control character"));
        }
        let target = match raw.strip_prefix("tcp://") {
            Some(address) => Target::Tcp(loopback(address)?),
            None => Target::Local(local_path(raw)?),
        };
        Ok(Self { target })
    }
}

fn bad_endpoint(why: impl Into<String>) -> HookError {
    HookError::BadEndpoint(why.into())
}

/// Parses `ip:port` and refuses any address that is not loopback.
fn loopback(address: &str) -> Result<SocketAddr, HookError> {
    let addr: SocketAddr = address
        .parse()
        .map_err(|_| bad_endpoint(format!("{address:?} is not an ip:port address")))?;
    if !addr.ip().is_loopback() {
        return Err(bad_endpoint("tcp endpoints must be loopback"));
    }
    Ok(addr)
}

/// Checks a local endpoint on Windows. Only the named pipe namespace is accepted, so a plain
/// file such as `C:\...` or a UNC share is never opened.
#[cfg(windows)]
fn local_path(raw: &str) -> Result<PathBuf, HookError> {
    let name = match raw.split_at_checked(PIPE_PREFIX.len()) {
        Some((head, rest)) if head.eq_ignore_ascii_case(PIPE_PREFIX) => rest,
        _ => {
            return Err(bad_endpoint(
                r"a local endpoint must be a named pipe path such as \\.\pipe\acan",
            ));
        }
    };
    if name.is_empty() || name.contains(['\\', '/']) || name.contains("..") {
        return Err(bad_endpoint(
            "the pipe name must be one non-empty segment with no slashes or '..'",
        ));
    }
    Ok(PathBuf::from(raw))
}

/// Checks a local endpoint on Unix: it must be an absolute socket path.
#[cfg(unix)]
fn local_path(raw: &str) -> Result<PathBuf, HookError> {
    if !Path::new(raw).is_absolute() {
        return Err(bad_endpoint(
            "a Unix socket endpoint must be an absolute path",
        ));
    }
    Ok(PathBuf::from(raw))
}

#[cfg(not(any(unix, windows)))]
fn local_path(_raw: &str) -> Result<PathBuf, HookError> {
    Err(bad_endpoint("no local transport on this platform"))
}

/// Sends one request line to the daemon and reads one reply line.
///
/// # Errors
///
/// Returns [`HookError::Daemon`] when connecting or I/O fails. Returns [`HookError::BadReply`]
/// when the reply is empty, longer than [`MAX_REPLY_BYTES`], or not ended by a newline. A reply
/// cut short by a closed connection is therefore refused.
pub fn exchange(endpoint: &Endpoint, request: &[u8]) -> Result<String, HookError> {
    let line = match &endpoint.target {
        Target::Tcp(addr) => {
            TcpStream::connect(*addr).and_then(|stream| exchange_stream(stream, request))
        }
        Target::Local(path) => exchange_local(path, request),
    }
    .map_err(HookError::Daemon)?;
    check_reply(line)
}

fn check_reply(line: String) -> Result<String, HookError> {
    if line.len() as u64 > MAX_REPLY_BYTES {
        return Err(HookError::BadReply(format!(
            "reply is longer than {MAX_REPLY_BYTES} bytes"
        )));
    }
    if line.is_empty() {
        return Err(HookError::BadReply(
            "the daemon closed the connection without a reply".to_owned(),
        ));
    }
    if !line.ends_with('\n') {
        return Err(HookError::BadReply(
            "the reply is not newline-terminated".to_owned(),
        ));
    }
    Ok(line)
}

#[cfg(windows)]
fn exchange_local(path: &Path, request: &[u8]) -> io::Result<String> {
    // One read-write handle serves both directions; the daemon writes its reply to the same
    // pipe instance.
    let pipe = OpenOptions::new().read(true).write(true).open(path)?;
    exchange_stream(pipe, request)
}

#[cfg(unix)]
fn exchange_local(path: &Path, request: &[u8]) -> io::Result<String> {
    exchange_stream(std::os::unix::net::UnixStream::connect(path)?, request)
}

#[cfg(not(any(unix, windows)))]
fn exchange_local(_path: &Path, _request: &[u8]) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no local transport on this platform",
    ))
}

/// Writes the request, then reads the reply up to its first newline. The read stops one byte
/// past [`MAX_REPLY_BYTES`], so a daemon cannot make the hook buffer without bound.
fn exchange_stream<S: Read + Write>(mut stream: S, request: &[u8]) -> io::Result<String> {
    stream.write_all(request)?;
    stream.flush()?;
    let mut limited = BufReader::new((&mut stream).take(MAX_REPLY_BYTES + 1));
    let mut reply = String::new();
    limited.read_line(&mut reply)?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_tcp_is_accepted() {
        let endpoint = Endpoint::parse("tcp://127.0.0.1:47777").expect("loopback endpoint");
        assert_eq!(
            endpoint.target,
            Target::Tcp("127.0.0.1:47777".parse().expect("socket address"))
        );
        assert!(Endpoint::parse("tcp://[::1]:9").is_ok());
    }

    #[test]
    fn non_loopback_and_malformed_tcp_are_refused() {
        for raw in [
            "tcp://192.0.2.1:9",
            "tcp://0.0.0.0:9",
            "tcp://localhost:9",
            "tcp://127.0.0.1:notaport",
            "tcp://127.0.0.1:9/x",
        ] {
            assert!(
                matches!(Endpoint::parse(raw), Err(HookError::BadEndpoint(_))),
                "{raw} must be refused"
            );
        }
    }

    #[test]
    fn control_characters_are_refused() {
        assert!(matches!(
            Endpoint::parse("tcp://127.0.0.1:9\u{7}"),
            Err(HookError::BadEndpoint(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn only_canonical_pipe_names_are_accepted() {
        for raw in [r"\\.\pipe\acan-hook-test", r"\\.\PIPE\acan"] {
            assert_eq!(
                Endpoint::parse(raw).expect("pipe name").target,
                Target::Local(PathBuf::from(raw))
            );
        }
        for raw in [
            r"C:\temp\hook.txt",
            r"\\server\share\acan",
            r"\\.\pipe\..\acan",
            r"\\.\pipe\a\b",
            r"\\.\pipe\a/b",
            r"\\.\pipe\",
            "acan-hook",
        ] {
            assert!(
                matches!(Endpoint::parse(raw), Err(HookError::BadEndpoint(_))),
                "{raw} must be refused"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn only_absolute_socket_paths_are_accepted() {
        assert!(Endpoint::parse("/tmp/acan-hook-test.sock").is_ok());
        for raw in ["acan-hook.sock", "relative/hook.sock", ""] {
            assert!(
                matches!(Endpoint::parse(raw), Err(HookError::BadEndpoint(_))),
                "{raw:?} must be refused"
            );
        }
    }
}
