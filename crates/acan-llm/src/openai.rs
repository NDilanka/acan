use std::time::{Duration, Instant};

use reqwest::header::HeaderMap;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::chat::{ChatRequest, ChatResponse, Message, ToolCall, ToolKind, ToolSpec};
use crate::client::{Destination, ModelClient, Structured, parse_structured, with_schema};
use crate::config::{ConfigError, LlmConfig, ProviderConfig, StructuredOutput, TokenLimitField};
use crate::error::LlmError;
use crate::key::{ApiKey, ApiKeys};
use crate::role::Role;
use crate::trace::{RequestId, RequestIdSource, Traced};
use crate::usage::{CallRecord, Usage, UsageLedger};

/// Most bytes of a provider error body kept in an error, counted after credentials are redacted.
const ERROR_BODY_LIMIT: usize = 512;

const RESPONSE_BODY_LIMIT: usize = 4 * 1024 * 1024;

/// Response headers that may carry the provider's request id, in the order they are checked.
/// Baseten sends its id only under its own header name.
const REQUEST_ID_HEADERS: &[&str] = &["x-request-id", "x-baseten-request-id"];

/// Retries for 429 and 5xx replies. Waits `base_delay`, doubling it before each retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_retries: u32,
    pub base_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
        }
    }
}

/// The one client for every provider. Each provider speaks the OpenAI chat-completions format.
#[derive(Debug)]
pub struct OpenAiCompatible {
    config: LlmConfig,
    keys: ApiKeys,
    http: reqwest::Client,
    retry: RetryPolicy,
    ledger: UsageLedger,
}

impl OpenAiCompatible {
    /// Builds the client with the default retry policy.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Client`] when the HTTP client cannot be built.
    pub fn new(config: LlmConfig, keys: ApiKeys) -> Result<Self, ConfigError> {
        Ok(Self {
            config,
            keys,
            // reqwest's own retries would replay requests outside the 429 and 5xx policy.
            http: reqwest::Client::builder()
                .retry(reqwest::retry::never())
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            retry: RetryPolicy::default(),
            ledger: UsageLedger::default(),
        })
    }

    pub fn with_retry_policy(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Records this client's calls in `ledger`, so several clients can share one ledger.
    #[must_use]
    pub fn with_ledger(mut self, ledger: UsageLedger) -> Self {
        self.ledger = ledger;
        self
    }

    pub fn ledger(&self) -> &UsageLedger {
        &self.ledger
    }

    /// Runs one chat completion and also returns the provider's request id and echoed model.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`ModelClient::chat`].
    pub async fn chat_traced(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> Result<Traced<ChatResponse>, LlmError> {
        let route = self.config.resolve(role)?;
        let provider = route.provider;
        let key = self
            .keys
            .get(&provider.name)
            .ok_or_else(|| LlmError::MissingKey {
                provider: provider.name.clone(),
                env_var: provider.api_key_env.clone(),
            })?;

        let body = wire_request(
            route.model,
            &request,
            provider.structured_output,
            provider.token_limit_field,
        );
        let started = Instant::now();
        let Posted {
            completion,
            request_id,
        } = self.post_with_retries(provider, key, &body).await?;
        let latency = started.elapsed();

        let reply_model = completion.model;
        let usage = completion.usage.unwrap_or_else(|| {
            tracing::warn!(
                provider = %provider.name,
                model = route.model,
                "reply carried no usage, recording zero tokens"
            );
            Usage::default()
        });
        let call = CallRecord {
            role,
            provider: provider.name.clone(),
            model: route.model.to_owned(),
            usage,
            latency,
        };
        self.ledger.record(call.clone());

        let choice =
            completion
                .choices
                .into_iter()
                .next()
                .ok_or_else(|| LlmError::MalformedResponse {
                    provider: provider.name.clone(),
                    detail: "reply has no choices".to_owned(),
                })?;
        Ok(Traced {
            value: ChatResponse {
                content: choice.message.content,
                tool_calls: choice.message.tool_calls.unwrap_or_default(),
                finish_reason: choice.finish_reason,
                call,
            },
            request_id,
            reply_model,
        })
    }

    /// Runs a completion whose reply must parse as `T`, and also returns the trace fields.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`ModelClient::chat_json`]. A reply that does not parse as `T`
    /// is a [`LlmError::SchemaValidation`], and a reply that stopped at the output cap is a
    /// [`LlmError::OutputCapped`]. Both carry the call's trace, with the request id the provider
    /// sent.
    pub async fn chat_json_traced<T>(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> Result<Traced<Structured<T>>, LlmError>
    where
        T: DeserializeOwned + JsonSchema,
    {
        let cap = request.max_tokens;
        let Traced {
            value,
            request_id,
            reply_model,
        } = self.chat_traced(role, with_schema::<T>(request)).await?;
        let trace_request_id = request_id.as_ref().map(|id| id.value.clone());
        let structured =
            parse_structured(value, cap, trace_request_id, |text| self.keys.redact(text))?;
        Ok(Traced {
            value: structured,
            request_id,
            reply_model,
        })
    }

    async fn post_with_retries(
        &self,
        provider: &ProviderConfig,
        key: &ApiKey,
        body: &WireRequest<'_>,
    ) -> Result<Posted, LlmError> {
        let url = format!(
            "{}/chat/completions",
            provider.base_url.trim_end_matches('/')
        );
        let timeout = Duration::from_secs(provider.timeout_secs);
        let mut attempt: u32 = 1;
        loop {
            let mut response = self
                .http
                .post(&url)
                .bearer_auth(key.expose())
                .timeout(timeout)
                .json(body)
                .send()
                .await
                .map_err(|err| transport_error(&provider.name, err))?;
            let status = response.status();

            if status.is_success() {
                let header_id = header_request_id(response.headers()).map(|mut id| {
                    id.value = self.keys.redact(&id.value);
                    id
                });
                let mut bytes = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|err| transport_error(&provider.name, err))?
                {
                    if chunk.len() > RESPONSE_BODY_LIMIT - bytes.len() {
                        return Err(LlmError::MalformedResponse {
                            provider: provider.name.clone(),
                            detail: "reply exceeds the 4 MiB body limit".to_owned(),
                        });
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let text = self.keys.redact(&String::from_utf8_lossy(&bytes));
                let completion: WireCompletion =
                    serde_json::from_str(&text).map_err(|err| LlmError::MalformedResponse {
                        provider: provider.name.clone(),
                        detail: self.keys.redact(&err.to_string()),
                    })?;
                let request_id = header_id.or_else(|| body_request_id(completion.id.as_deref()));
                return Ok(Posted {
                    completion,
                    request_id,
                });
            }

            let retryable = status.as_u16() == 429 || status.is_server_error();
            if retryable && attempt <= self.retry.max_retries {
                let delay = self
                    .retry
                    .base_delay
                    .saturating_mul(2u32.saturating_pow(attempt - 1));
                tracing::warn!(
                    provider = %provider.name,
                    status = status.as_u16(),
                    attempt,
                    ?delay,
                    "retrying model call"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
                continue;
            }

            return Err(self.status_error(&provider.name, response, attempt).await);
        }
    }

    /// Builds the error for a final reply. Only variants that keep a body read it.
    async fn status_error(
        &self,
        provider: &str,
        mut response: reqwest::Response,
        attempts: u32,
    ) -> LlmError {
        let status = response.status().as_u16();
        let provider = provider.to_owned();
        match status {
            429 => LlmError::RateLimited { provider, attempts },
            500..=599 => LlmError::ServerError {
                provider,
                status,
                attempts,
            },
            402 => LlmError::PaymentRequired {
                provider,
                body: self.read_error_body(&mut response).await,
            },
            _ => LlmError::Status {
                provider,
                status,
                body: self.read_error_body(&mut response).await,
            },
        }
    }

    /// Reads the start of an error body and redacts configured keys.
    ///
    /// The read window is the byte budget plus the longest key, so any key that starts inside the
    /// budget is read whole and redacted before the cut. A failed read keeps the bytes already
    /// received, because the body is only diagnostic.
    async fn read_error_body(&self, response: &mut reqwest::Response) -> String {
        let window = ERROR_BODY_LIMIT + self.keys.longest_len();
        let mut bytes = Vec::with_capacity(window);
        while bytes.len() < window {
            let Ok(Some(chunk)) = response.chunk().await else {
                break;
            };
            let take = chunk.len().min(window - bytes.len());
            bytes.extend_from_slice(&chunk[..take]);
        }
        let text = self.keys.redact(&String::from_utf8_lossy(&bytes));
        truncate_to_bytes(text, ERROR_BODY_LIMIT)
    }
}

impl ModelClient for OpenAiCompatible {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        self.chat_traced(role, request)
            .await
            .map(|traced| traced.value)
    }

    fn destination(&self, role: Role) -> Option<Destination> {
        let route = self.config.resolve(role).ok()?;
        Some(Destination {
            provider: route.provider.name.clone(),
            model: route.model.to_owned(),
        })
    }

    async fn chat_json<T>(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> Result<Structured<T>, LlmError>
    where
        T: DeserializeOwned + JsonSchema,
    {
        self.chat_json_traced(role, request)
            .await
            .map(|traced| traced.value)
    }

    async fn chat_traced(
        &self,
        role: Role,
        request: ChatRequest,
    ) -> Result<Traced<ChatResponse>, LlmError> {
        OpenAiCompatible::chat_traced(self, role, request).await
    }
}

/// A successful reply body and the request id read from its headers or body.
struct Posted {
    completion: WireCompletion,
    request_id: Option<RequestId>,
}

/// The first configured request-id header with a non-empty value.
fn header_request_id(headers: &HeaderMap) -> Option<RequestId> {
    REQUEST_ID_HEADERS.iter().find_map(|name| {
        let value = headers.get(*name)?.to_str().ok()?.trim();
        (!value.is_empty()).then(|| RequestId {
            value: value.to_owned(),
            source: RequestIdSource::Header((*name).to_owned()),
        })
    })
}

fn body_request_id(id: Option<&str>) -> Option<RequestId> {
    id.filter(|id| !id.is_empty()).map(|id| RequestId {
        value: id.to_owned(),
        source: RequestIdSource::Body,
    })
}

fn transport_error(provider: &str, err: reqwest::Error) -> LlmError {
    if err.is_timeout() {
        LlmError::Timeout {
            provider: provider.to_owned(),
        }
    } else {
        LlmError::Transport {
            provider: provider.to_owned(),
            source: err,
        }
    }
}

/// Cuts `text` to at most `limit` bytes, stepping back to a character boundary if needed.
fn truncate_to_bytes(mut text: String, limit: usize) -> String {
    if text.len() > limit {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Without `response_format`, the prompt fallback asks for JSON and `chat_json` validates it.
fn wire_request<'a>(
    model: &'a str,
    request: &'a ChatRequest,
    mode: StructuredOutput,
    token_limit_field: TokenLimitField,
) -> WireRequest<'a> {
    let (max_tokens, max_completion_tokens) = match token_limit_field {
        TokenLimitField::MaxTokens => (request.max_tokens, None),
        TokenLimitField::MaxCompletionTokens => (None, request.max_tokens),
    };
    let (messages, response_format) = match (&request.response_format, mode) {
        (None, _) => (request.messages.clone(), None),
        (Some(format), StructuredOutput::JsonSchema) => (
            request.messages.clone(),
            Some(WireResponseFormat {
                kind: "json_schema",
                json_schema: WireJsonSchema {
                    name: &format.name,
                    schema: &format.schema,
                    strict: false,
                },
            }),
        ),
        (Some(format), StructuredOutput::Prompt) => {
            let mut messages = Vec::with_capacity(request.messages.len() + 1);
            messages.push(Message::system(schema_prompt(&format.schema)));
            messages.extend(request.messages.iter().cloned());
            (messages, None)
        }
    };

    WireRequest {
        model,
        messages,
        tools: request
            .tools
            .iter()
            .map(|function| WireTool {
                kind: ToolKind::Function,
                function,
            })
            .collect(),
        temperature: request.temperature,
        max_tokens,
        max_completion_tokens,
        response_format,
    }
}

fn schema_prompt(schema: &Value) -> String {
    format!(
        "Reply with one JSON object that matches this JSON Schema. Write no prose and no code fences.\n{schema}"
    )
}

#[derive(Serialize)]
struct WireRequest<'a> {
    model: &'a str,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<WireResponseFormat<'a>>,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: ToolKind,
    function: &'a ToolSpec,
}

#[derive(Serialize)]
struct WireResponseFormat<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    json_schema: WireJsonSchema<'a>,
}

#[derive(Serialize)]
struct WireJsonSchema<'a> {
    name: &'a str,
    schema: &'a Value,
    strict: bool,
}

#[derive(Deserialize)]
struct WireCompletion {
    id: Option<String>,
    model: Option<String>,
    choices: Vec<WireChoice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct WireChoice {
    message: WireReply,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct WireReply {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,
}
