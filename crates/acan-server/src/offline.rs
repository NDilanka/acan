//! The offline model: built-in replies so a server with no routing file still runs its loop.
//!
//! Each job kind gets the plainest reply that moves it forward: a split into one task on the
//! order's machine, a generic clash explanation with no suggestion, and a built-in scope draft on
//! the order's machine. The scope draft is labelled built in, and a human revises it before it is
//! confirmed. Probe and review requests get no reply at all. Token counts are estimates, so the
//! usage panel shows the calls a live provider would have taken. Every reply here is recorded with
//! provider `offline`, and the replies serve tests and the server without a routing file.

use std::time::Duration;

use acan_intent::{RouteEntry, RouteStatus};
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

    /// The ledger every answered call is recorded in. Only the tests read it, since the server
    /// reads the ledger it shares with the live client.
    #[cfg(test)]
    #[must_use]
    pub fn ledger(&self) -> &UsageLedger {
        &self.ledger
    }
}

impl ModelClient for Offline {
    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        // The context is outside text, fenced for the model. An unfenced context reads as it is.
        let context: Value = request
            .last_user_text()
            .map(|text| acan_llm::outside::unfence(text).unwrap_or(text))
            .and_then(|text| serde_json::from_str(text).ok())
            .ok_or(LlmError::NoScriptedReply { role })?;
        let reply = match model::ask_of(&request).ok_or(LlmError::NoScriptedReply { role })? {
            Ask::Split => split(&context),
            Ask::Explain => explain(),
            Ask::Scope => scope(&context).ok_or(LlmError::NoScriptedReply { role })?,
            // A probe draft gains no authority from a built-in reply, and a review is retired, so
            // none is given.
            Ask::Probe | Ask::Review => {
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

/// The built-in scope draft for the order's machine. It has one may-change item on a covered route
/// that touches the machine, or else the first covered route. Its step is the first probe request
/// the corpus sends on that route, so a probe reaches the item. Its acceptance is a count that one
/// request on the route answers 200, so it needs no witness. It is not a broad grant, and it has
/// no open question. The text says it is built in, and the same checks as a live draft still
/// apply to it. None when the request names no machine, the table covers no route, or no probe
/// sends the route.
fn scope(context: &Value) -> Option<Value> {
    let machine = context["machine"].as_str()?;
    let routes: Vec<RouteEntry> = serde_json::from_value(context["routes"].clone()).ok()?;
    let covered: Vec<&RouteEntry> = routes
        .iter()
        .filter(|route| matches!(route.status, RouteStatus::Covered {}))
        .collect();
    let route = covered
        .iter()
        .copied()
        .find(|route| {
            route
                .route_path
                .machines
                .as_slice()
                .iter()
                .any(|found| found.as_str() == machine)
        })
        .or_else(|| covered.first().copied())?;
    let method = serde_json::to_value(route.method).ok()?;
    let template = serde_json::to_value(&route.route_template).ok()?;
    let requests = context["requests"].as_array()?;
    let step = requests
        .iter()
        .find(|request| request["method"] == method && request["route_template"] == template)
        .and_then(|request| request["step_id"].as_str())?;
    Some(json!({
        "may": [{
            "id": "i-built-in",
            "selector": {
                "surface": "http",
                "method": method,
                "route_template": template,
                "step_context": { "kind": "step", "step_id": step },
                "principal": { "kind": "anonymous" },
                "channel": "status"
            },
            "change": "modify",
            "acceptance": [{
                "obligation_id": "obl-built-in",
                "predicate": {
                    "predicate": "count",
                    "filter": {
                        "run": "candidate",
                        "method": method,
                        "route_template": template,
                        "status_in": [200],
                        "principal": { "kind": "any" }
                    },
                    "op": "ge",
                    "value": 1
                },
                "text": "Built-in offline draft: at least one request on this route answers 200."
            }],
            "provenance": "model",
            "example": {
                "kind": "target",
                "step_id": step,
                "path": "status",
                "after": { "state": "present", "value": 200 }
            },
            "machine_ids": [machine]
        }],
        "must_keep": [],
        "open": []
    }))
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
    use std::str::FromStr;

    use acan_core::MachineKey;
    use acan_intent::{ModelRef, ProbeCorpus, RunLimits, ScopeBody, StepContext};
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
    async fn a_split_is_answered_from_its_fenced_context() {
        let context = json!({ "order": "Add a limit", "machine": "ui" });
        let split = ChatRequest {
            messages: vec![
                Message::system(model::SPLIT_PROMPT),
                Message::user(acan_llm::outside::fence(&context.to_string())),
            ],
            ..ChatRequest::default()
        };
        let reply = Offline::default()
            .chat(Role::Foreman, split)
            .await
            .expect("the offline split reads its fenced context");
        let text = reply.content.expect("the offline split has a reply");
        let tasks: Value = serde_json::from_str(&text).expect("the offline split is JSON");
        assert_eq!(tasks["tasks"][0]["scope"][0], "ui");
        assert_eq!(tasks["tasks"][0]["title"], "Add a limit");
    }

    #[tokio::test]
    async fn a_scope_request_gets_a_built_in_draft_that_the_checks_accept() {
        let corpus: ProbeCorpus =
            serde_json::from_str(include_str!("../../acan-intent/fixtures/probe-corpus.json"))
                .expect("the probe corpus fixture decodes");
        let machines: Vec<MachineKey> = ["api", "ui"]
            .into_iter()
            .map(|name| MachineKey::from_str(name).expect("a valid machine name"))
            .collect();
        let bounds = model::DraftBounds {
            machines: &machines,
            routes: &corpus.routes,
            fixtures: &corpus.fixtures,
            probes: &corpus.probes,
            limits: RunLimits {
                max_requests: 5,
                max_output_bytes: 65_536,
                sequence_timeout_ms: 30_000,
            },
        };
        let reply = Offline::default()
            .chat(
                Role::Foreman,
                model::scope_request_on("Add a limit", "ui", &bounds, None),
            )
            .await
            .expect("the built-in scope answers the scope request");
        let text = reply.content.expect("the built-in scope has a reply");
        let body: ScopeBody = serde_json::from_str(&text).expect("the reply is a scope body");

        let draft = model::scope_draft(
            body,
            ModelRef::from_str("offline").expect("a valid model id"),
            &bounds,
        )
        .expect("the built-in scope passes the checks a live draft passes");

        let proposal = draft.proposal();
        assert_eq!(proposal.may.len(), 1, "one item on the order's machine");
        assert!(proposal.must_keep.is_empty(), "no must-keep rule");
        assert!(proposal.open.is_empty(), "no open question");
        let item = &proposal.may[0];
        assert_eq!(item.machine_ids.as_slice()[0].as_str(), "ui");
        assert_eq!(item.selector.route_template.as_str(), "/health");
        assert!(
            matches!(
                &item.selector.step_context,
                StepContext::Step { step_id } if step_id.as_str() == "s1"
            ),
            "the step is the first probe request on the route"
        );
        assert!(!item.selector.is_broad(), "the item is no broad grant");
        assert!(
            item.acceptance.as_slice()[0]
                .text
                .starts_with("Built-in offline draft"),
            "the built-in draft says so"
        );
    }

    #[tokio::test]
    async fn a_scope_request_without_a_machine_gets_no_reply() {
        let err = Offline::default()
            .chat(
                Role::Foreman,
                request(
                    model::SCOPE_PROMPT,
                    &json!({ "routes": [], "steps": [], "order": "Add a limit" }),
                ),
            )
            .await
            .expect_err("a built-in scope needs the order's machine");
        assert!(
            matches!(err, LlmError::NoScriptedReply { .. }),
            "got {err:?}"
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
