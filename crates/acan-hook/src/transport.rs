//! Local transports from the hook to the daemon.
//!
//! Windows uses a named pipe and Unix a domain socket. Loopback TCP serves tests and
//! diagnostics. Each exchange is one request line and one reply line. The caller enforces the
//! deadline, not this module.

#[cfg(windows)]
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};

use crate::{ENDPOINT_ENV, HookError};

/// Largest reply the hook accepts, in bytes. A reply is one short JSON line.
pub const MAX_REPLY_BYTES: u64 = 64 * 1024;

/// Where the daemon listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A Windows named pipe (`\\.\pipe\name`) or a Unix domain socket path.
    LocalPath(PathBuf),
    /// Loopback TCP. Non-loopback addresses are refused.
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
            .map_err(|_| HookError::BadEndpoint("endpoint is not valid UTF-8".to_owned()))?;
        Self::parse(&text)
    }

    /// Parses an endpoint string.
    ///
    /// # Errors
    ///
    /// Returns [`HookError::BadEndpoint`] for a `tcp://` value that is not an `ip:port` pair,
    /// or whose address is not loopback.
    pub fn parse(raw: &str) -> Result<Self, HookError> {
        let Some(address) = raw.strip_prefix("tcp://") else {
            return Ok(Self::LocalPath(PathBuf::from(raw)));
        };
        let addr: SocketAddr = address.parse().map_err(|_| {
            HookError::BadEndpoint(format!("{address:?} is not an ip:port address"))
        })?;
        if !addr.ip().is_loopback() {
            return Err(HookError::BadEndpoint(
                "tcp endpoints must be loopback".to_owned(),
            ));
        }
        Ok(Self::Tcp(addr))
    }
}

/// Sends one request line to the daemon and reads one reply line.
///
/// # Errors
///
/// Returns the I/O error from connecting, writing or reading. A reply longer than
/// [`MAX_REPLY_BYTES`] is an error too.
pub fn exchange(endpoint: &Endpoint, request: &[u8]) -> io::Result<String> {
    match endpoint {
        Endpoint::Tcp(addr) => exchange_stream(TcpStream::connect(*addr)?, request),
        Endpoint::LocalPath(path) => exchange_local(path, request),
    }
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

fn exchange_stream<S: Read + Write>(mut stream: S, request: &[u8]) -> io::Result<String> {
    stream.write_all(request)?;
    stream.flush()?;
    // Reading through `take` caps the reply, so a daemon cannot make the hook buffer without bound.
    let mut limited = BufReader::new((&mut stream).take(MAX_REPLY_BYTES + 1));
    let mut reply = String::new();
    limited.read_line(&mut reply)?;
    if reply.len() as u64 > MAX_REPLY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon reply exceeds the size limit",
        ));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_tcp_is_accepted() {
        let endpoint = Endpoint::parse("tcp://127.0.0.1:47777").expect("loopback endpoint");
        assert_eq!(
            endpoint,
            Endpoint::Tcp("127.0.0.1:47777".parse().expect("socket address"))
        );
        assert!(Endpoint::parse("tcp://[::1]:9").is_ok());
    }

    #[test]
    fn non_loopback_and_malformed_tcp_are_refused() {
        assert!(matches!(
            Endpoint::parse("tcp://192.0.2.1:9"),
            Err(HookError::BadEndpoint(_))
        ));
        assert!(matches!(
            Endpoint::parse("tcp://localhost:9"),
            Err(HookError::BadEndpoint(_))
        ));
    }

    #[test]
    fn other_values_are_local_paths() {
        let pipe = r"\\.\pipe\acan-hook-test";
        assert_eq!(
            Endpoint::parse(pipe).expect("local path"),
            Endpoint::LocalPath(PathBuf::from(pipe))
        );
    }
}
