use std::future::Future;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::chat::{ChatRequest, ChatResponse, JsonSchemaFormat};
use crate::error::LlmError;
use crate::role::Role;
use crate::trace::Traced;
use crate::usage::CallRecord;

/// The provider name Muse Spark is served under in routing.
pub const MUSE_SPARK_PROVIDER: &str = "meta";

/// The Muse Spark model ids a route may run. A route to any other model is not Muse Spark. A new
/// Muse Spark release is one more entry here.
pub const MUSE_SPARK_MODELS: &[&str] = &["muse-spark-1.3"];

/// Where a role's calls go: the provider that answers them, and the model that provider runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// The provider's name in the routing file.
    pub provider: String,
    /// The model id the route runs.
    pub model: String,
}

impl Destination {
    /// Whether this destination is Muse Spark: the Meta provider running one of
    /// [`MUSE_SPARK_MODELS`].
    #[must_use]
    pub fn is_muse_spark(&self) -> bool {
        self.provider == MUSE_SPARK_PROVIDER && MUSE_SPARK_MODELS.contains(&self.model.as_str())
    }
}

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

    /// Where calls for `role` go, when the client routes them. A client without routing, such as
    /// an offline one, has no destination for any role.
    fn destination(&self, _role: Role) -> Option<Destination> {
        None
    }

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

    /// Runs [`ModelClient::chat`] and also returns the provider's trace fields. A client that
    /// reads none, such as [`crate::Stub`], returns them empty.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`ModelClient::chat`].
    fn chat_traced(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> impl Future<Output = Result<Traced<ChatResponse>, LlmError>> + Send {
        async move {
            let value = self.chat(role, request).await?;
            Ok(Traced {
                value,
                request_id: None,
                reply_model: None,
            })
        }
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
