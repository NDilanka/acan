//! The offline model: built-in replies so a server with no routing file still runs its loop.
//!
//! Each job kind gets the plainest reply that moves it forward: a split into one task on the
//! order's machine, and a generic clash explanation with no suggestion. Scope, probe and review
//! requests get no reply at all. Token counts
//! are estimates, so the usage panel shows the calls a live provider would have taken. Every reply
//! here is recorded with provider `offline`, and the replies serve tests and the server without a
//! routing file.

use std::time::Duration;

use acan_llm::{
    CallRecord, ChatRequest, ChatResponse, LlmError, ModelClient, Role, Usage, UsageLedger,
};
use serde_json::{Value, json};

use crate::model::{self, Ask};

/// Characters per token for the usage estimate, the usual rule of thumb for English text.
const CHARS_PER_TOKEN: usize = 4;

/// Answers model jobs from built-in replies and records each call in its ledger.
#[derive(Debug, Default)]
pub struct Offline {
    ledger: UsageLedger,
}

impl Offline {
    /// Records every answered call in `ledger`, so the live client and the fallback share one.
    #[must_use]
    pub fn with_ledger(ledger: UsageLedger) -> Self {
        Self { ledger }
    }

    /// The ledger every answered call is recorded in.
    #[must_use]
    pub fn ledger(&self) -> &UsageLedger {
        &self.ledger
    }
}

impl ModelClient for Offline {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let context: Value = request
            .last_user_text()
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or(LlmError::NoScriptedReply { role })?;
        let reply = match model::ask_of(&request).ok_or(LlmError::NoScriptedReply { role })? {
            Ask::Split => split(&context),
            Ask::Explain => explain(),
            // A scope or probe draft gains no authority from a built-in reply, and a review is
            // retired, so none is given.
            Ask::Scope | Ask::Probe | Ask::Review => {
                return Err(LlmError::NoScriptedReply { role });
            }
        }
        .to_string();
        let prompt_chars: usize = request.last_user_text().map_or(0, str::len);
        let call = CallRecord {
            role,
            provider: "offline".to_owned(),
            model: "offline".to_owned(),
            usage: Usage {
                prompt_tokens: estimate(prompt_chars),
                completion_tokens: estimate(reply.len()),
            },
            latency: Duration::ZERO,
        };
        self.ledger.record(call.clone());
        Ok(ChatResponse {
            content: Some(reply),
            tool_calls: Vec::new(),
            finish_reason: Some("stop".to_owned()),
            call,
        })
    }
}

fn estimate(chars: usize) -> u32 {
    u32::try_from(chars.div_ceil(CHARS_PER_TOKEN)).unwrap_or(u32::MAX)
}

fn split(context: &Value) -> Value {
    let sentence = context["order"].as_str().unwrap_or("the order");
    json!({
        "tasks": [{
            "title": sentence,
            "brief": format!("Make this true: {sentence}"),
            "scope": [context["machine"]],
        }]
    })
}

fn explain() -> Value {
    json!({
        "consequence": "Both tasks change the same code, so whichever lands second would build on code that has moved under it.",
        "breaks": null,
        "suggested": null,
    })
}

#[cfg(test)]
mod tests {
    use acan_llm::Message;

    use super::*;

    fn request(system: &str, context: &Value) -> ChatRequest {
        ChatRequest {
            messages: vec![Message::system(system), Message::user(context.to_string())],
            ..ChatRequest::default()
        }
    }

    #[tokio::test]
    async fn a_review_request_gets_no_offline_reply() {
        let err = Offline::default()
            .chat(
                Role::Foreman,
                request(model::REVIEW_PROMPT, &json!({ "readbacks": [] })),
            )
            .await
            .expect_err("the offline client answers no review");
        assert!(
            matches!(err, LlmError::NoScriptedReply { .. }),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn an_explain_reply_becomes_an_explanation() {
        let reply = Offline::default()
            .chat_json::<model::ExplainReply>(
                Role::Fast,
                request(model::EXPLAIN_PROMPT, &json!({ "clash": "c-1" })),
            )
            .await
            .expect("the offline explanation parses");
        let explanation = model::explanation(reply.value).expect("a valid explanation");
        assert!(
            !explanation.consequence.is_empty(),
            "it says what goes wrong"
        );
    }

    #[tokio::test]
    async fn an_unknown_request_gets_no_reply() {
        let err = Offline::default()
            .chat(Role::Foreman, request("something else", &json!({})))
            .await
            .expect_err("only the server's own prompts are answered");
        assert!(
            matches!(err, LlmError::NoScriptedReply { .. }),
            "got {err:?}"
        );
    }
}
