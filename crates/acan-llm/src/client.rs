use std::future::Future;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::chat::{ChatRequest, ChatResponse, JsonSchemaFormat};
use crate::error::LlmError;
use crate::role::Role;
use crate::setup::{KeyOutcome, KeyStatus, SetupError};
use crate::usage::CallRecord;

/// The one interface the rest of Acan uses for model calls.
///
/// Use a concrete client or a `C: ModelClient` bound. The trait is not dyn-compatible,
/// because its methods return `impl Future`. The returned futures are `Send`.
pub trait ModelClient: Send + Sync {
    /// Runs one chat completion for `role`. The provider and model come from routing.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Config`] or [`LlmError::MissingKey`] before any request is sent.
    /// Otherwise returns the HTTP, timeout, or reply error from the provider that answered.
    fn chat(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> impl Future<Output = Result<ChatResponse, LlmError>> + Send;

    /// Runs a completion whose reply must parse as `T`. The call is metered even when the
    /// reply does not parse.
    ///
    /// # Errors
    ///
    /// Returns every error from [`ModelClient::chat`], plus [`LlmError::OutputCapped`] when the
    /// reply stopped at the output cap, and [`LlmError::SchemaValidation`] when the reply does
    /// not parse as `T`.
    fn chat_json<T>(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> impl Future<Output = Result<Structured<T>, LlmError>> + Send
    where
        T: DeserializeOwned + JsonSchema,
    {
        async move {
            let cap = request.max_tokens;
            let response = self.chat(role, with_schema::<T>(request)).await?;
            parse_structured(response, cap, str::to_owned)
        }
    }

    /// The providers, keys and role picks in use for model calls. A client with no setup of its
    /// own reports none. It never reports a key itself.
    fn key_status(&self) -> KeyStatus {
        KeyStatus::default()
    }

    /// Checks `key` with `provider` and saves it, so calls use it from then on. Only a client
    /// that keeps its keys in a keys file takes one; this default refuses.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`save_checked`](crate::save_checked), or
    /// [`SetupError::Unavailable`] when this client does not take saved keys.
    fn save_key(
        &self,
        _provider: &str,
        _key: &str,
    ) -> impl Future<Output = Result<KeyOutcome, SetupError>> + Send {
        async { Err(SetupError::Unavailable) }
    }

    /// Removes the saved key for `provider`, and any role pick that named it. Returns whether a
    /// key was removed. This default refuses, as [`ModelClient::save_key`] does.
    ///
    /// # Errors
    ///
    /// Returns [`SetupError::Unavailable`] when this client does not take saved keys.
    fn remove_key(&self, _provider: &str) -> Result<bool, SetupError> {
        Err(SetupError::Unavailable)
    }

    /// Picks `provider` for `role`, or clears the pick when `provider` is `None`. This default
    /// refuses, as [`ModelClient::save_key`] does.
    ///
    /// # Errors
    ///
    /// Returns [`SetupError::Unavailable`] when this client does not take saved keys.
    fn set_role(&self, _role: Role, _provider: Option<&str>) -> Result<(), SetupError> {
        Err(SetupError::Unavailable)
    }
}

/// A typed reply and the accounting record for the call that produced it.
#[derive(Debug, Clone)]
pub struct Structured<T> {
    pub value: T,
    pub call: CallRecord,
}

/// Asks the provider for a reply that matches the JSON Schema of `T`.
pub(crate) fn with_schema<T: JsonSchema>(mut request: ChatRequest) -> ChatRequest {
    request.response_format = Some(JsonSchemaFormat::for_type::<T>());
    request
}

/// The finish reason a provider sends when a reply stops at its output cap.
const FINISH_LENGTH: &str = "length";

/// Parses a reply as `T`. `cap` is the output cap the request set. A reply that stopped at that
/// cap is refused before it is parsed, because its text is cut short. `scrub` removes credentials
/// from provider text before the text is kept in an error.
pub(crate) fn parse_structured<T: DeserializeOwned>(
    response: ChatResponse,
    cap: Option<u32>,
    scrub: impl Fn(&str) -> String,
) -> Result<Structured<T>, LlmError> {
    if response.finish_reason.as_deref() == Some(FINISH_LENGTH) {
        return Err(LlmError::OutputCapped {
            provider: response.call.provider.clone(),
            max_tokens: cap,
        });
    }
    let raw = response.content.as_deref().unwrap_or_default();
    let parsed: Result<T, serde_json::Error> = serde_json::from_str(strip_code_fence(raw.trim()));
    match parsed {
        Ok(value) => Ok(Structured {
            value,
            call: response.call,
        }),
        Err(err) => Err(LlmError::SchemaValidation {
            provider: response.call.provider.clone(),
            detail: scrub(&err.to_string()),
            raw: scrub(raw),
        }),
    }
}

/// Models often wrap JSON in a Markdown fence even when asked not to.
fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.strip_prefix("json").unwrap_or(rest);
    rest.trim().strip_suffix("```").unwrap_or(rest).trim()
}

#[cfg(test)]
mod tests {
    use super::strip_code_fence;

    #[test]
    fn fenced_json_is_unwrapped() {
        assert_eq!(strip_code_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
    }

    #[test]
    fn bare_json_is_left_alone() {
        assert_eq!(strip_code_fence("{\"a\":1}"), "{\"a\":1}");
    }
}
