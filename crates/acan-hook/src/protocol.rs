//! Wire format between the hook and the daemon: one JSON object per line, each way.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::HookError;

/// Protocol version both sides send. A reply with any other version is refused.
pub const PROTOCOL_VERSION: u32 = 1;

/// Why the hook is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// Droid session start: a heartbeat the daemon records for the order.
    SessionStart,
    /// A tool call about to run: the daemon decides whether it may land.
    PreToolUse,
}

/// One question from the hook to the daemon.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HookRequest {
    pub version: u32,
    pub kind: RequestKind,
    pub order: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub tool_name: Option<String>,
    /// `tool_input.file_path`, or `tool_input.path` when the first is absent.
    pub file_path: Option<String>,
    /// `tool_input.command`.
    pub command: Option<String>,
    /// The whole `tool_input` object, as Droid sent it.
    pub raw_tool_input: Option<Value>,
}

/// The daemon's answer to one request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HookResponse {
    pub version: u32,
    pub decision: Decision,
    pub reason: Option<String>,
}

/// The daemon's decision. Any other value makes the reply malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
}

impl HookResponse {
    /// Turns the decision into the hook result: `Ok` for allow.
    ///
    /// # Errors
    ///
    /// Returns [`HookError::Blocked`] for deny, with the daemon's reason or `no reason given`.
    pub fn into_verdict(self) -> Result<(), HookError> {
        match self.decision {
            Decision::Allow => Ok(()),
            Decision::Deny => {
                let reason = self
                    .reason
                    .as_deref()
                    .map(collapse_whitespace)
                    .filter(|text| !text.is_empty())
                    .unwrap_or_else(|| "no reason given".to_owned());
                Err(HookError::Blocked(reason))
            }
        }
    }
}

/// Encodes a request as one JSON line, newline included.
///
/// # Errors
///
/// Returns [`HookError::Internal`] if the request cannot be serialized.
pub fn encode_request(request: &HookRequest) -> Result<String, HookError> {
    let mut line = serde_json::to_string(request)
        .map_err(|err| HookError::Internal(format!("cannot encode request: {err}")))?;
    line.push('\n');
    Ok(line)
}

/// Parses the daemon's reply line. The line must end in a newline and be one JSON object.
///
/// # Errors
///
/// Returns [`HookError::BadReply`] when the line is not newline-terminated, is not a JSON
/// object with a known `decision`, or carries a protocol version other than
/// [`PROTOCOL_VERSION`].
pub fn parse_reply(line: &str) -> Result<HookResponse, HookError> {
    let body = line
        .strip_suffix('\n')
        .ok_or_else(|| HookError::BadReply("the reply is not newline-terminated".to_owned()))?;
    let response: HookResponse = crate::parse_object(body).map_err(HookError::BadReply)?;
    if response.version != PROTOCOL_VERSION {
        return Err(HookError::BadReply(format!(
            "protocol version {} (expected {PROTOCOL_VERSION})",
            response.version
        )));
    }
    Ok(response)
}

/// Joins a multi-line reason into one line, so stderr stays a single message.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses one reply line the way the transport delivers it: with its newline.
    fn verdict(line: &str) -> Result<(), HookError> {
        parse_reply(&format!("{line}\n"))
            .expect("reply parses")
            .into_verdict()
    }

    #[test]
    fn allow_passes() {
        assert!(verdict(r#"{"version":1,"decision":"allow"}"#).is_ok());
    }

    #[test]
    fn deny_carries_a_one_line_reason() {
        let err =
            verdict(r#"{"version":1,"decision":"deny","reason":"outside src/\n  notes/b2.txt"}"#)
                .expect_err("deny blocks");
        assert_eq!(err.to_string(), "blocked: outside src/ notes/b2.txt");
    }

    #[test]
    fn deny_without_a_reason_says_so() {
        let bare = verdict(r#"{"version":1,"decision":"deny"}"#).expect_err("deny blocks");
        assert_eq!(bare.to_string(), "blocked: no reason given");
        let blank =
            verdict(r#"{"version":1,"decision":"deny","reason":"  "}"#).expect_err("deny blocks");
        assert_eq!(blank.to_string(), "blocked: no reason given");
    }

    #[test]
    fn malformed_replies_are_bad_replies() {
        let cases = [
            "",
            "not json",
            r#"{"version":1,"decision":"maybe"}"#,
            r#"{"version":1}"#,
            r#"{"decision":"allow"}"#,
            r#"{"version":2,"decision":"allow"}"#,
            // serde reads a sequence as a struct by position, so arrays are refused by name.
            r#"["1","allow"]"#,
            r#"[1,"allow",null]"#,
        ];
        for case in cases {
            // Each case is complete, so the only fault is the content itself.
            let line = format!("{case}\n");
            assert!(
                matches!(parse_reply(&line), Err(HookError::BadReply(_))),
                "reply {case:?} must be refused"
            );
        }
    }

    #[test]
    fn a_reply_without_a_newline_is_refused() {
        let unterminated = r#"{"version":1,"decision":"allow"}"#;
        assert!(
            matches!(parse_reply(unterminated), Err(HookError::BadReply(_))),
            "a reply cut before its newline must be refused"
        );
    }

    #[test]
    fn repeated_keys_in_a_reply_are_refused() {
        // A repeated key is ambiguous, so no copy is chosen. Each of these is refused.
        for case in [
            r#"{"version":1,"decision":"deny","decision":"allow"}"#,
            r#"{"version":1,"decision":"allow","version":1}"#,
            r#"{"version":1,"decision":"deny","reason":"a","reason":"b"}"#,
            r#"{"version":1,"decision":"allow","extra":1,"extra":2}"#,
        ] {
            let line = format!("{case}\n");
            assert!(
                matches!(parse_reply(&line), Err(HookError::BadReply(_))),
                "reply {case} must be refused"
            );
        }
    }

    #[test]
    fn request_encodes_as_one_line() {
        let request = HookRequest {
            version: PROTOCOL_VERSION,
            kind: RequestKind::SessionStart,
            order: "order-1".to_owned(),
            session_id: None,
            cwd: None,
            tool_name: None,
            file_path: None,
            command: None,
            raw_tool_input: None,
        };
        let line = encode_request(&request).expect("encodes");
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.contains(r#""kind":"session_start""#), "{line}");
    }
}
