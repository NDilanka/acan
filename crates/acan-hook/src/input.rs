//! Droid's hook stdin, and the plan the hook makes from it.

use serde::Deserialize;
use serde_json::Value;

use crate::HookError;
use crate::protocol::{HookRequest, PROTOCOL_VERSION, RequestKind};

/// Droid's outer batch wrapper. Each inner call fires its own `PreToolUse`, so the wrapper passes.
pub const SCRIPT_TOOL: &str = "Script";

/// The fields of Droid's hook stdin that the hook reads. Other fields, such as `source`, are ignored.
#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub hook_event_name: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
}

/// What the hook does with one event.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Exit 0 without contacting the daemon.
    Pass,
    /// Ask the daemon.
    Ask(HookRequest),
}

/// Parses hook stdin.
///
/// # Errors
///
/// Returns [`HookError::BadInput`] when stdin is not a JSON object with `hook_event_name`.
pub fn parse(raw: &str) -> Result<HookInput, HookError> {
    serde_json::from_str(raw).map_err(|err| HookError::BadInput(err.to_string()))
}

/// Decides what to ask the daemon for one event.
///
/// # Errors
///
/// Returns [`HookError::BadInput`] for an event the hook does not handle, or for a
/// `PreToolUse` that has no `tool_name`.
pub fn plan(input: HookInput, order: &str) -> Result<Plan, HookError> {
    match input.hook_event_name.as_str() {
        "SessionStart" => Ok(Plan::Ask(request(RequestKind::SessionStart, input, order))),
        "PreToolUse" => {
            let tool_name = input
                .tool_name
                .as_deref()
                .ok_or_else(|| HookError::BadInput("PreToolUse without tool_name".to_owned()))?;
            if tool_name == SCRIPT_TOOL {
                return Ok(Plan::Pass);
            }
            Ok(Plan::Ask(request(RequestKind::PreToolUse, input, order)))
        }
        other => Err(HookError::BadInput(format!(
            "unsupported hook event {other:?}"
        ))),
    }
}

fn request(kind: RequestKind, input: HookInput, order: &str) -> HookRequest {
    let file_path = input
        .tool_input
        .as_ref()
        .and_then(|tool_input| {
            str_field(tool_input, "file_path").or_else(|| str_field(tool_input, "path"))
        })
        .map(str::to_owned);
    let command = input
        .tool_input
        .as_ref()
        .and_then(|tool_input| str_field(tool_input, "command"))
        .map(str::to_owned);
    HookRequest {
        version: PROTOCOL_VERSION,
        kind,
        order: order.to_owned(),
        session_id: input.session_id,
        cwd: input.cwd,
        tool_name: input.tool_name,
        file_path,
        command,
        raw_tool_input: input.tool_input,
    }
}

fn str_field<'v>(tool_input: &'v Value, key: &str) -> Option<&'v str> {
    tool_input.get(key)?.as_str()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ask(raw: &str) -> HookRequest {
        match plan(parse(raw).expect("valid hook input"), "order-1") {
            Ok(Plan::Ask(request)) => request,
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn write_call_keeps_path_and_raw_input() {
        let request = ask(
            r#"{"hook_event_name":"PreToolUse","session_id":"sess-1","cwd":"/work/repo",
                "tool_name":"Create","tool_input":{"file_path":"src/a.txt","content":"x"}}"#,
        );
        assert_eq!(request.kind, RequestKind::PreToolUse);
        assert_eq!(request.order, "order-1");
        assert_eq!(request.session_id.as_deref(), Some("sess-1"));
        assert_eq!(request.cwd.as_deref(), Some("/work/repo"));
        assert_eq!(request.tool_name.as_deref(), Some("Create"));
        assert_eq!(request.file_path.as_deref(), Some("src/a.txt"));
        assert_eq!(request.command, None);
        assert_eq!(
            request.raw_tool_input,
            Some(json!({"file_path": "src/a.txt", "content": "x"}))
        );
    }

    #[test]
    fn path_key_is_used_when_file_path_is_absent() {
        let request = ask(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Edit","tool_input":{"path":"src/b.txt"}}"#,
        );
        assert_eq!(request.file_path.as_deref(), Some("src/b.txt"));
    }

    #[test]
    fn shell_call_keeps_the_command() {
        let request = ask(
            r#"{"hook_event_name":"PreToolUse","tool_name":"Execute","tool_input":{"command":"git status"}}"#,
        );
        assert_eq!(request.command.as_deref(), Some("git status"));
        assert_eq!(request.file_path, None);
    }

    #[test]
    fn session_start_is_a_heartbeat_request() {
        let request =
            ask(r#"{"hook_event_name":"SessionStart","session_id":"sess-1","source":"startup"}"#);
        assert_eq!(request.kind, RequestKind::SessionStart);
        assert_eq!(request.tool_name, None);
    }

    #[test]
    fn outer_script_batch_passes_without_a_request() {
        let raw =
            r#"{"hook_event_name":"PreToolUse","tool_name":"Script","tool_input":{"script":"x"}}"#;
        assert_eq!(
            plan(parse(raw).expect("valid hook input"), "order-1").expect("plans"),
            Plan::Pass
        );
    }

    #[test]
    fn unusable_events_are_bad_input() {
        let no_tool = plan(
            parse(r#"{"hook_event_name":"PreToolUse"}"#).expect("valid hook input"),
            "order-1",
        );
        assert!(
            matches!(no_tool, Err(HookError::BadInput(_))),
            "{no_tool:?}"
        );
        let other_event = plan(
            parse(r#"{"hook_event_name":"Stop"}"#).expect("valid hook input"),
            "order-1",
        );
        assert!(
            matches!(other_event, Err(HookError::BadInput(_))),
            "{other_event:?}"
        );
        assert!(matches!(parse("{not json"), Err(HookError::BadInput(_))));
    }
}
