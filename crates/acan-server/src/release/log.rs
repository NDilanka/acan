//! The last log lines, kept in memory for a bug report. Every line still goes to stderr.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Mutex, MutexGuard, PoisonError};

use tracing_subscriber::fmt::MakeWriter;

/// How many log lines are kept. A bug report sends fewer than this.
const KEEP: usize = 200;

static TAIL: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

fn tail() -> MutexGuard<'static, VecDeque<String>> {
    TAIL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A `tracing` writer that sends each event to stderr and keeps its lines for a bug report.
///
/// Install it with `tracing_subscriber::fmt().with_writer(LogTee)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogTee;

impl<'a> MakeWriter<'a> for LogTee {
    type Writer = Event;

    fn make_writer(&'a self) -> Self::Writer {
        Event(Vec::new())
    }
}

/// One event's bytes. The formatter writes the whole event, then drops this, and the drop sends
/// the bytes to stderr and keeps their lines.
#[derive(Debug)]
pub struct Event(Vec<u8>);

impl Write for Event {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // Losing a line on a broken stderr must not fail the event, so the write error is ignored.
        let _ = io::stderr().write_all(&self.0);
        let text = String::from_utf8_lossy(&self.0);
        let mut tail = tail();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            if tail.len() == KEEP {
                tail.pop_front();
            }
            tail.push_back(line.to_owned());
        }
    }
}

/// The kept lines, oldest first.
#[must_use]
pub fn recent() -> Vec<String> {
    tail().iter().cloned().collect()
}
