use std::process::ExitStatus;
use std::time::Duration;

use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::DroidError;

const JSON_RPC: &str = "2.0";
const FACTORY_API: &str = "1.0.0";

/// The integer or string identifier of a JSON-RPC request.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(untagged)]
pub enum RequestId {
    /// Numeric identifier.
    Number(i64),
    /// Opaque string identifier.
    Text(String),
}

/// A request sent to Droid. The factory envelope fields are set by the constructor.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundRequest<'a, P> {
    jsonrpc: &'static str,
    factory_api_version: &'static str,
    r#type: &'static str,
    id: &'a RequestId,
    method: &'a str,
    params: P,
}

impl<'a, P> OutboundRequest<'a, P> {
    /// Wraps method parameters in a request using the supported wire version.
    pub fn new(id: &'a RequestId, method: &'a str, params: P) -> Self {
        Self {
            jsonrpc: JSON_RPC,
            factory_api_version: FACTORY_API,
            r#type: "request",
            id,
            method,
            params,
        }
    }
}

/// A successful reply to a server request from Droid.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboundResponse<'a, R> {
    jsonrpc: &'static str,
    factory_api_version: &'static str,
    r#type: &'static str,
    id: &'a RequestId,
    result: R,
}

impl<'a, R> OutboundResponse<'a, R> {
    /// Echoes the server request ID and wraps the typed reply.
    pub fn new(id: &'a RequestId, result: R) -> Self {
        Self {
            jsonrpc: JSON_RPC,
            factory_api_version: FACTORY_API,
            r#type: "response",
            id,
            result,
        }
    }
}

/// A frame received from Droid after validating its JSON-RPC and factory envelope.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum InboundMessage {
    /// A request for the client to answer.
    Request {
        /// The server's request ID.
        id: RequestId,
        /// The server method.
        method: String,
        /// Method-specific parameters.
        #[serde(default)]
        params: Value,
    },
    /// The result (or error) of a client request.
    #[serde(alias = "error")]
    Response {
        /// The client request ID, if associated with a request.
        id: Option<RequestId>,
        /// Method-specific result.
        #[serde(default)]
        result: Value,
        /// Error information, if Droid rejected the request.
        error: Option<RpcError>,
    },
    /// An unsolicited notification, including methods not known to this client.
    Notification {
        /// The notification method.
        method: String,
        /// Method-specific notification data.
        #[serde(default)]
        params: Value,
    },
}

impl InboundMessage {
    /// Decodes a single frame and validates the wire versions; extra fields are ignored.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed JSON, an invalid envelope, or unsupported wire versions.
    pub fn from_json(line: &[u8]) -> Result<Self, DroidError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Envelope<'a> {
            jsonrpc: &'a str,
            factory_api_version: &'a str,
            #[serde(flatten)]
            body: InboundMessage,
        }

        let frame: Envelope<'_> = serde_json::from_slice(line)?;
        if frame.jsonrpc != JSON_RPC || frame.factory_api_version != FACTORY_API {
            return Err(DroidError::Protocol("unsupported wire version"));
        }
        Ok(frame.body)
    }
}

/// The error object in a failed JSON-RPC response.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct RpcError {
    /// The JSON-RPC error code.
    pub code: i64,
    /// The error message.
    pub message: String,
    /// Optional additional error data.
    #[serde(default)]
    pub data: Value,
}

/// The settings requested before the first turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSettings {
    /// The managed Droid model name.
    pub model_id: String,
    /// The configured reasoning effort.
    pub reasoning_effort: String,
    /// The configured autonomy level.
    pub autonomy_level: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SettingsSnapshot<'a> {
    model_id: Option<&'a str>,
    autonomy_level: Option<&'a str>,
}

impl<'a> SettingsSnapshot<'a> {
    pub(crate) fn from_value(value: &'a Value) -> Result<Option<Self>, DroidError> {
        let Value::Object(fields) = value else {
            return Ok(None);
        };
        let settings = fields.get("settings").unwrap_or(value);
        Ok(Self::deserialize(settings).map(Some)?)
    }

    pub(crate) fn confirms(&self, desired: &SessionSettings) -> Result<bool, DroidError> {
        if let Some(model) = self.model_id
            && model != desired.model_id
        {
            return Err(DroidError::SettingsMismatch("model"));
        }
        if let Some(level) = self.autonomy_level
            && level != desired.autonomy_level
        {
            return Err(DroidError::SettingsMismatch("autonomy"));
        }
        Ok(self.model_id.is_some() && self.autonomy_level.is_some())
    }
}

/// Tool calls and the choices Droid offers the developer.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionRequest {
    /// All tool uses covered by the same decision.
    pub tool_uses: Vec<PermissionToolUse>,
    /// Top-level confirmation type, when supplied.
    pub confirmation_type: Option<String>,
    /// Top-level details, when supplied.
    pub details: Option<PermissionDetails>,
    /// Choices the developer can select.
    pub options: Vec<PermissionOption>,
}

impl PermissionRequest {
    /// Lists the inner calls of a Script permission, in the order Droid reports them.
    pub fn script_calls(&self) -> impl Iterator<Item = &ScriptCall> {
        self.details
            .iter()
            .chain(
                self.tool_uses
                    .iter()
                    .filter_map(|item| item.details.as_ref()),
            )
            .flat_map(|details| &details.calls)
    }
}

/// A single tool use covered by a permission request.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionToolUse {
    /// The call that needs a decision.
    pub tool_use: ToolUse,
    /// Droid's confirmation type, such as `script`, `exec`, or `mcp_tool`.
    pub confirmation_type: Option<String>,
    /// Confirmation details; Script details include the inner calls.
    pub details: Option<PermissionDetails>,
}

/// A Droid tool call and its method-specific input.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct ToolUse {
    /// The tool call ID.
    pub id: String,
    /// The tool name.
    pub name: String,
    /// Tool arguments, when supplied.
    #[serde(default)]
    pub input: Value,
}

/// The permission details, with Script calls separated from other type-specific fields.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct PermissionDetails {
    /// The inner calls listed for a Script.
    #[serde(default)]
    pub calls: Vec<ScriptCall>,
    /// Other details specific to the confirmation type.
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

/// An inner call reported by Droid for a Script permission.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ScriptCall {
    /// The name of the inner tool.
    pub tool_name: String,
    /// Input to the inner tool, when supplied.
    #[serde(default)]
    pub tool_input: Value,
    /// The inner call's confirmation details.
    pub confirmation: ScriptConfirmation,
}

/// The confirmation type and details of an inner Script call.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[non_exhaustive]
pub struct ScriptConfirmation {
    /// The confirmation type, such as `mcp_tool` or `exec`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Other confirmation fields, such as `serverName` for MCP calls.
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

/// A choice presented by a permission request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[non_exhaustive]
pub struct PermissionOption {
    /// User-facing label.
    pub label: String,
    /// Opaque value to echo in the reply.
    pub value: String,
}

/// The choice the developer made in response to a permission request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionReply {
    /// The `value` from one of the offered options.
    pub selected_option: String,
}

/// Questions the developer must answer before the turn can continue.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AskUserRequest {
    /// The call that raised the questions.
    pub tool_call_id: String,
    /// Questions in Droid's original order.
    pub questions: Vec<AskQuestion>,
}

/// A single question in an ask-user request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AskQuestion {
    /// Droid's question index, to echo with the answer.
    pub index: u32,
    /// Short title, when supplied.
    pub topic: Option<String>,
    /// Question text, to echo with the answer.
    pub question: String,
    /// Choices Droid presents alongside its free-text input.
    #[serde(default)]
    pub options: Vec<String>,
    /// Descriptions of choices, when supplied.
    pub option_descriptions: Option<Vec<String>>,
}

/// The developer's answer to one question.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AskAnswer {
    /// The index supplied in the request.
    pub index: u32,
    /// The question as supplied in the request.
    pub question: String,
    /// The developer's choice or free-text answer.
    pub answer: String,
}

/// The developer's answers or explicit refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AskUserReply {
    /// Answer the questions.
    Answers(Vec<AskAnswer>),
    /// Decline the request.
    Declined,
}

impl Serialize for AskUserReply {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut result = serializer.serialize_struct(
            "AskUserReply",
            if matches!(self, Self::Declined) { 2 } else { 1 },
        )?;
        match self {
            Self::Answers(answers) => result.serialize_field("answers", answers)?,
            Self::Declined => {
                result.serialize_field("cancelled", &true)?;
                result.serialize_field("answers", &[] as &[AskAnswer])?;
            }
        }
        result.end()
    }
}

/// Remaining quota as reported by Droid. Missing fields do not imply zero quota.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Headroom {
    /// Droid supplied both the remaining amount and its total limit.
    Known {
        /// Remaining quota.
        remaining: u64,
        /// Total quota in the same units.
        limit: u64,
    },
    /// Droid did not supply enough information to know the remaining quota.
    Unknown,
}

/// An event surfaced by a Droid session.
#[derive(Debug, PartialEq)]
#[non_exhaustive]
pub enum DroidEvent {
    /// A settings snapshot, including unsolicited snapshots.
    SettingsUpdated {
        /// The triggering request, when Droid supplied it.
        request_id: Option<RequestId>,
        /// The original notification payload.
        raw: Value,
    },
    /// Assistant output.
    AssistantMessage(Value),
    /// A tool call was made.
    ToolUse(Value),
    /// A tool call returned.
    ToolResult(Value),
    /// A hook started, produced output, or completed.
    HookExecution {
        /// The notification name.
        method: String,
        /// Its original payload.
        raw: Value,
    },
    /// Droid is waiting for a decision about one or more tool calls.
    PermissionRequested {
        /// The response must echo this ID.
        id: RequestId,
        /// The tools and choices Droid presented.
        request: PermissionRequest,
    },
    /// Droid is waiting for the developer's answers.
    AskUserRequested {
        /// The response must echo this ID.
        id: RequestId,
        /// The questions Droid presented.
        request: AskUserRequest,
    },
    /// Droid explicitly ended a turn; a cancelled or failed turn is not a finished run.
    TurnCompleted {
        /// The reported turn ID, when supplied.
        turn_id: Option<String>,
        /// Droid's completion reason, when supplied.
        reason: Option<String>,
    },
    /// A request or turn hit a quota or rate limit. The client never retries automatically.
    RateLimited {
        /// Droid's error message.
        message: String,
        /// Suggested wait, only when Droid provides one.
        retry_after: Option<Duration>,
        /// Quota information, only when Droid provides it.
        headroom: Headroom,
    },
    /// An error in response to a non-startup request.
    RequestFailed {
        /// The request ID, when supplied.
        id: Option<RequestId>,
        /// The error returned by Droid.
        error: RpcError,
    },
    /// A newer or unsupported server request.
    UnknownRequest {
        /// The server's request ID.
        id: RequestId,
        /// The method name.
        method: String,
        /// The unrecognized parameters.
        raw: Value,
    },
    /// A newer or unsupported notification, which does not stop the stream.
    Unknown {
        /// The notification name.
        method: String,
        /// The unrecognized payload.
        raw: Value,
    },
    /// The process exited or the attached streams closed.
    Exited {
        /// The process exit status, absent for a stream with no owned child.
        status: Option<ExitStatus>,
    },
}

impl DroidEvent {
    pub(crate) fn request(
        id: RequestId,
        method: String,
        params: Value,
    ) -> Result<Self, DroidError> {
        Ok(match method.as_str() {
            "droid.request_permission" => Self::PermissionRequested {
                id,
                request: serde_json::from_value(params)?,
            },
            "droid.ask_user" => Self::AskUserRequested {
                id,
                request: serde_json::from_value(params)?,
            },
            _ => Self::UnknownRequest {
                id,
                method,
                raw: params,
            },
        })
    }

    pub(crate) fn notification(method: String, mut raw: Value) -> Self {
        let method = if method == "droid.session_notification" {
            if let Some(notification) = raw.get_mut("notification") {
                raw = notification.take();
            }
            raw.get("type")
                .and_then(Value::as_str)
                .unwrap_or(&method)
                .to_owned()
        } else {
            method
        };
        match method.strip_prefix("droid.").unwrap_or(&method) {
            "settings_updated" => Self::SettingsUpdated {
                request_id: raw
                    .get("requestId")
                    .and_then(|value| RequestId::deserialize(value).ok()),
                raw,
            },
            "turn_completed" | "agent_turn_completed"
                if raw.get("reason").is_none_or(Value::is_string) =>
            {
                Self::TurnCompleted {
                    turn_id: string_field(&raw, "turnId"),
                    reason: string_field(&raw, "reason"),
                }
            }
            "assistant_message" => Self::AssistantMessage(raw),
            "create_message"
                if raw.pointer("/message/role").and_then(Value::as_str) == Some("assistant") =>
            {
                Self::AssistantMessage(raw)
            }
            "tool_use" | "tool_call" => Self::ToolUse(raw),
            "tool_result" | "tool_call_result" => Self::ToolResult(raw),
            name if name.starts_with("hook_execution_") => Self::HookExecution { method, raw },
            name if is_rate_limit_label(name) => rate_limited(&raw, None),
            "error" | "session_error" | "agent_error" | "error_message"
                if is_rate_limit_error(&raw) =>
            {
                rate_limited(&raw, None)
            }
            _ => {
                tracing::debug!(%method, "unknown droid notification");
                Self::Unknown { method, raw }
            }
        }
    }

    pub(crate) fn response_error(id: Option<RequestId>, error: RpcError) -> Self {
        if error.code == 429
            || is_rate_limit_error(&error.data)
            || is_rate_limit_text(&error.message)
        {
            rate_limited(&error.data, Some(&error.message))
        } else {
            Self::RequestFailed { id, error }
        }
    }
}

fn string_field(value: &Value, name: &str) -> Option<String> {
    value.get(name)?.as_str().map(str::to_owned)
}

fn is_rate_limit_label(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "rate_limit"
            | "rate_limited"
            | "rate_limit_error"
            | "rate_limit_exceeded"
            | "too_many_requests"
    )
}

fn is_rate_limit_text(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("rate limit") || lower.contains("too many requests")
}

fn is_rate_limit_error(raw: &Value) -> bool {
    let payload = raw.get("error").unwrap_or(raw);
    ["status", "statusCode", "code"]
        .iter()
        .any(|key| payload.get(key).and_then(Value::as_u64) == Some(429))
        || ["code", "type", "name", "errorType"]
            .iter()
            .filter_map(|key| payload.get(key).and_then(Value::as_str))
            .any(is_rate_limit_label)
        || payload
            .as_str()
            .or_else(|| payload.get("message").and_then(Value::as_str))
            .is_some_and(is_rate_limit_text)
}

fn rate_limited(raw: &Value, message: Option<&str>) -> DroidEvent {
    let payload = raw.get("error").unwrap_or(raw);
    let field = |name: &str| {
        payload
            .get(name)
            .or_else(|| raw.get(name))
            .or_else(|| payload.get("data")?.get(name))
    };
    let message = message
        .or_else(|| payload.as_str())
        .or_else(|| field("message").and_then(Value::as_str))
        .unwrap_or("Droid rate limit")
        .to_owned();
    let retry_after = field("retryAfterMs")
        .and_then(Value::as_u64)
        .map(Duration::from_millis)
        .or_else(|| {
            field("retryAfter")
                .and_then(Value::as_u64)
                .map(Duration::from_secs)
        });
    let headroom = field("headroom")
        .and_then(|fields| {
            Some(Headroom::Known {
                remaining: fields.get("remaining")?.as_u64()?,
                limit: fields.get("limit")?.as_u64()?,
            })
        })
        .unwrap_or(Headroom::Unknown);
    DroidEvent::RateLimited {
        message,
        retry_after,
        headroom,
    }
}
