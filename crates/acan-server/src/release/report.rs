//! The bug report: a GitHub new-issue link, filled in with the version, the OS and the last log
//! lines. Everything in it passes through [`Redaction`] first.

use std::fmt::Write as _;

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Serialize;

use super::{Redaction, VersionInfo};

/// Where bug reports are filed. Testers see the pre-filled form and submit it themselves.
const ISSUES_URL: &str = "https://github.com/NDilanka/acan/issues/new";

/// The newest log lines a report carries.
const MAX_LOG_LINES: usize = 40;

/// The longest log line a report carries, in characters. Longer lines are cut.
const MAX_LINE_CHARS: usize = 400;

/// The longest link worth opening. Browsers take longer ones, but GitHub drops the end of a long
/// body, so older log lines are left out until the link fits.
const MAX_URL_LEN: usize = 7000;

/// A bug report ready to open. `body` is always the full text, even when `url` leaves lines out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BugReport {
    pub url: String,
    pub title: String,
    pub body: String,
    /// True when older log lines were left out of `url` to keep it short. `body` still has them.
    pub truncated: bool,
}

/// Builds the report from the build's identity and the log lines, oldest first.
#[must_use]
pub fn build(version: &VersionInfo, log: &[String], redaction: &Redaction) -> BugReport {
    let title = format!(
        "Alpha bug report: acan {} on {}",
        version.version, version.os
    );
    let lines: Vec<String> = log
        .iter()
        .rev()
        .take(MAX_LOG_LINES)
        .rev()
        .map(|line| cut(&redaction.scrub(line), MAX_LINE_CHARS))
        .collect();
    let body = render_body(version, &lines);
    // The link keeps the newest lines that fit. Each step drops the oldest one still in it.
    let mut shown = lines.len();
    let mut url = link(&title, &body);
    while url.len() > MAX_URL_LEN && shown > 0 {
        shown -= 1;
        url = link(&title, &render_body(version, &lines[lines.len() - shown..]));
    }
    BugReport {
        url,
        title,
        truncated: shown < lines.len(),
        body,
    }
}

fn render_body(version: &VersionInfo, lines: &[String]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "## What happened\n");
    let _ = writeln!(
        out,
        "<!-- What you did, what you expected, and what happened instead. -->\n"
    );
    let _ = writeln!(out, "## Build\n");
    let _ = writeln!(
        out,
        "- acan {} (commit {})",
        version.version, version.commit
    );
    let _ = writeln!(out, "- {} {}\n", version.os, version.arch);
    let _ = writeln!(
        out,
        "## Last log lines\n\nKeys, tokens and private paths are removed from these lines.\n"
    );
    out.push_str("```text\n");
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("```\n");
    out
}

fn link(title: &str, body: &str) -> String {
    format!(
        "{ISSUES_URL}?title={}&body={}",
        utf8_percent_encode(title, NON_ALPHANUMERIC),
        utf8_percent_encode(body, NON_ALPHANUMERIC),
    )
}

/// The first `max` characters of `line`, with a marker when it was cut.
fn cut(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let kept: String = line.chars().take(max).collect();
    format!("{kept} [cut]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::VersionInfo;

    fn version() -> VersionInfo {
        VersionInfo {
            name: "acan",
            version: "0.1.0-alpha.1",
            commit: "abc1234",
            os: "linux",
            arch: "x86_64",
        }
    }

    #[test]
    fn a_short_log_fits_whole() {
        let log = vec!["INFO acan serving".to_owned(), "WARN slow call".to_owned()];
        let report = build(&version(), &log, &Redaction::EMPTY);
        assert!(!report.truncated);
        assert!(report.url.starts_with(ISSUES_URL));
        assert!(report.body.contains("INFO acan serving"));
        assert!(report.body.contains("acan 0.1.0-alpha.1 (commit abc1234)"));
    }

    #[test]
    fn a_long_log_is_cut_to_fit_the_link_and_keeps_the_newest_lines() {
        let log: Vec<String> = (0..MAX_LOG_LINES * 2)
            .map(|n| format!("line {n} {}", "x".repeat(MAX_LINE_CHARS)))
            .collect();
        let report = build(&version(), &log, &Redaction::EMPTY);
        assert!(report.truncated);
        assert!(report.url.len() <= MAX_URL_LEN);
        assert!(
            report
                .body
                .contains(&format!("line {}", MAX_LOG_LINES * 2 - 1))
        );
        assert!(!report.body.contains("line 0 "));
    }

    #[test]
    fn secrets_and_private_paths_never_reach_the_link() {
        let redaction = Redaction {
            secrets: vec!["provider-key-value-123".to_owned()],
            roots: vec![("/home/example/proj".to_owned(), "<repo>")],
        };
        let log =
            vec!["call used provider-key-value-123 at /home/example/proj/src/x.rs".to_owned()];
        let report = build(&version(), &log, &redaction);
        assert!(!report.body.contains("provider-key-value-123"));
        assert!(!report.body.contains("/home/example"));
        assert!(!report.url.contains("provider-key-value-123"));
        assert!(report.body.contains("<repo>/src/x.rs"));
    }

    #[test]
    fn a_long_line_is_cut_with_a_marker() {
        let line = "y".repeat(MAX_LINE_CHARS + 10);
        let cut_line = cut(&line, MAX_LINE_CHARS);
        assert!(cut_line.ends_with("[cut]"));
        assert_eq!(cut_line.chars().count(), MAX_LINE_CHARS + " [cut]".len());
    }
}
