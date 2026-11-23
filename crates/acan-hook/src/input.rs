//! Droid's hook stdin, and the plan the hook makes from it.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::HookError;
use crate::protocol::{HookRequest, PROTOCOL_VERSION, RequestKind};

/// Droid's outer batch wrapper. Each inner call fires its own `PreToolUse`, so the wrapper
/// passes. Only a wrapper whose input carries a non-blank `script` string passes. Any other
/// `Script` call goes to the daemon like an ordinary tool call.
pub const SCRIPT_TOOL: &str = "Script";

/// The fields of Droid's hook stdin that the hook reads. Other fields, such as `source`, are
/// ignored.
#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub hook_event_name: String,
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Field<String>,
    #[serde(default)]
    pub tool_input: Field<Value>,
}

/// A field that is absent, present as JSON `null`, or holds a value. An `Option` reads the
/// last two alike, and a `SessionStart` must refuse both.
#[derive(Debug, Default)]
pub enum Field<T> {
    /// The key is not in the object.
    #[default]
    Absent,
    /// The key is present with `null`.
    Null,
    /// The key is present with a value.
    Value(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Option::<T>::deserialize(deserializer)? {
            Some(value) => Self::Value(value),
            None => Self::Null,
        })
    }
}

impl<T> Field<T> {
    /// True when the key is in the object, whether its value is `null` or not.
    fn is_present(&self) -> bool {
        !matches!(self, Self::Absent)
    }

    fn value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Null => None,
        }
    }

    fn into_value(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Null => None,
        }
    }
}

/// What the hook does with one event.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Exit 0 without contacting the daemon.
    Pass,
    /// Ask the daemon.
    Ask(HookRequest),
}

/// Parses hook stdin. It must be one JSON object.
///
/// # Errors
///
/// Returns [`HookError::BadInput`] when stdin is not a JSON object with a `hook_event_name`
/// string.
pub fn parse(raw: &str) -> Result<HookInput, HookError> {
    crate::parse_object(raw).map_err(HookError::BadInput)
}

/// Decides what to ask the daemon for one event.
///
/// A `SessionStart` must omit `tool_name` and `tool_input`, even when they are `null`. A
/// `PreToolUse` must carry a non-blank `tool_name` and an object `tool_input`.
///
/// # Errors
///
/// Returns [`HookError::BadInput`] for an event the hook does not handle, or for a shape Droid
/// does not send.
pub fn plan(input: HookInput, order: &str) -> Result<Plan, HookError> {
    match input.hook_event_name.as_str() {
        "SessionStart" => {
            // A `null` counts as present, so an explicit null is refused like a value.
            if input.tool_name.is_present() || input.tool_input.is_present() {
                return Err(bad_input("SessionStart carries tool fields"));
            }
            Ok(Plan::Ask(request(RequestKind::SessionStart, input, order)))
        }
        "PreToolUse" => {
            let tool_name = match &input.tool_name {
                Field::Value(name) if !name.trim().is_empty() => name.as_str(),
                _ => return Err(bad_input("PreToolUse without a tool_name")),
            };
            let tool_input = match &input.tool_input {
                Field::Value(value) if value.is_object() => value,
                _ => return Err(bad_input("PreToolUse without a tool_input object")),
            };
            if is_batch_wrapper(tool_name, tool_input) {
                return Ok(Plan::Pass);
            }
            Ok(Plan::Ask(request(RequestKind::PreToolUse, input, order)))
        }
        other => Err(bad_input(format!("unsupported hook event {other:?}"))),
    }
}

/// True for the outer `Script` call: the tool is `Script` and its input carries a script.
fn is_batch_wrapper(tool_name: &str, tool_input: &Value) -> bool {
    tool_name == SCRIPT_TOOL
        && str_field(tool_input, "script").is_some_and(|script| !script.trim().is_empty())
}

fn bad_input(why: impl Into<String>) -> HookError {
    HookError::BadInput(why.into())
}

fn request(kind: RequestKind, input: HookInput, order: &str) -> HookRequest {
    let file_path = input
        .tool_input
        .value()
        .and_then(|tool_input| {
            str_field(tool_input, "file_path").or_else(|| str_field(tool_input, "path"))
        })
        .map(str::to_owned);
    let command = input
        .tool_input
        .value()
        .and_then(|tool_input| str_field(tool_input, "command"))
        .map(str::to_owned);
    HookRequest {
        version: PROTOCOL_VERSION,
        kind,
        order: order.to_owned(),
        session_id: input.session_id,
        cwd: input.cwd,
        tool_name: input.tool_name.into_value(),
        file_path,
        command,
        raw_tool_input: input.tool_input.into_value(),
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

    fn refused(raw: &str) -> HookError {
        match parse(raw).and_then(|input| plan(input, "order-1")) {
            Err(err) => err,
            Ok(plan) => panic!("{raw} must be refused, got {plan:?}"),
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
    fn script_without_a_script_string_goes_to_the_daemon() {
        for raw in [
            r#"{"hook_event_name":"PreToolUse","tool_name":"Script","tool_input":{}}"#,
            r#"{"hook_event_name":"PreToolUse","tool_name":"Script","tool_input":{"script":"  "}}"#,
            r#"{"hook_event_name":"PreToolUse","tool_name":"Script","tool_input":{"script":3}}"#,
        ] {
            let request = ask(raw);
            assert_eq!(request.kind, RequestKind::PreToolUse, "{raw}");
            assert_eq!(request.tool_name.as_deref(), Some("Script"), "{raw}");
        }
    }

    #[test]
    fn unusable_events_are_bad_input() {
        assert!(matches!(
            refused(r#"{"hook_event_name":"PreToolUse"}"#),
            HookError::BadInput(_)
        ));
        assert!(matches!(
            refused(r#"{"hook_event_name":"Stop"}"#),
            HookError::BadInput(_)
        ));
        assert!(matches!(parse("{not json"), Err(HookError::BadInput(_))));
    }

    #[test]
    fn non_object_stdin_is_bad_input() {
        for raw in [
            r#"["PreToolUse","Create"]"#,
            r#""PreToolUse""#,
            "null",
            "[]",
        ] {
            assert!(
                matches!(parse(raw), Err(HookError::BadInput(_))),
                "{raw} must be refused"
            );
        }
    }

    #[test]
    fn pre_tool_use_needs_a_name_and_an_object_input() {
        for raw in [
            r#"{"hook_event_name":"PreToolUse","tool_name":"  ","tool_input":{}}"#,
            r#"{"hook_event_name":"PreToolUse","tool_name":"Create"}"#,
            r#"{"hook_event_name":"PreToolUse","tool_name":"Create","tool_input":"x"}"#,
            r#"{"hook_event_name":"PreToolUse","tool_name":"Create","tool_input":["x"]}"#,
        ] {
            assert!(
                matches!(refused(raw), HookError::BadInput(_)),
                "{raw} must be refused"
            );
        }
    }

    #[test]
    fn session_start_with_tool_fields_is_bad_input() {
        assert!(matches!(
            refused(r#"{"hook_event_name":"SessionStart","tool_name":"Create"}"#),
            HookError::BadInput(_)
        ));
        assert!(matches!(
            refused(r#"{"hook_event_name":"SessionStart","tool_input":{}}"#),
            HookError::BadInput(_)
        ));
    }

    #[test]
    fn explicit_null_tool_fields_are_refused_on_session_start() {
        for raw in [
            r#"{"hook_event_name":"SessionStart","tool_name":null}"#,
            r#"{"hook_event_name":"SessionStart","tool_input":null}"#,
            r#"{"hook_event_name":"SessionStart","tool_name":null,"tool_input":null}"#,
        ] {
            assert!(
                matches!(refused(raw), HookError::BadInput(_)),
                "{raw} must be refused"
            );
        }
    }

    #[test]
    fn explicit_null_tool_fields_are_refused_on_pre_tool_use() {
        for raw in [
            r#"{"hook_event_name":"PreToolUse","tool_name":null,"tool_input":{}}"#,
            r#"{"hook_event_name":"PreToolUse","tool_name":"Create","tool_input":null}"#,
        ] {
            assert!(
                matches!(refused(raw), HookError::BadInput(_)),
                "{raw} must be refused"
            );
        }
    }

    #[test]
    fn repeated_keys_in_hook_input_are_refused() {
        for raw in [
            r#"{"hook_event_name":"PreToolUse","tool_name":"Create","tool_name":"Execute","tool_input":{}}"#,
            r#"{"hook_event_name":"PreToolUse","tool_name":"Create","tool_input":{"file_path":"notes/x.txt","file_path":"src/a.txt"}}"#,
            r#"{"hook_event_name":"SessionStart","source":"startup","source":"resume"}"#,
        ] {
            assert!(
                matches!(parse(raw), Err(HookError::BadInput(_))),
                "{raw} must be refused"
            );
        }
    }
}
