use std::fmt;
use std::time::Duration;

use serde::Serialize;

use crate::chat::{ChatRequest, ChatResponse, ToolCall};
use crate::client::{Destination, ModelClient};
use crate::error::LlmError;
use crate::role::Role;
use crate::usage::{CallRecord, Usage, UsageLedger};

type Matcher = Box<dyn Fn(&ChatRequest) -> bool + Send + Sync>;

/// An offline provider. Replies come from a script keyed by role and a request matcher.
#[derive(Default)]
pub struct Stub {
    rules: Vec<Rule>,
    routes: Vec<(Role, Destination)>,
    ledger: UsageLedger,
}

struct Rule {
    role: Role,
    matches: Matcher,
    reply: Reply,
}

/// What a scripted call returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reply {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
}

impl Reply {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: Some(content.into()),
            ..Self::default()
        }
    }

    /// A reply whose text is `value` serialized as JSON.
    ///
    /// # Panics
    ///
    /// Panics if `value` cannot be represented as JSON. [`Reply::try_json`] returns the error.
    pub fn json(value: &impl Serialize) -> Self {
        Self::try_json(value)
            .unwrap_or_else(|err| panic!("stub reply must serialize to JSON: {err}"))
    }

    /// A reply whose text is `value` serialized as JSON.
    ///
    /// # Errors
    ///
    /// Returns the `serde_json` error when `value` cannot be represented as JSON.
    pub fn try_json(value: &impl Serialize) -> Result<Self, serde_json::Error> {
        serde_json::to_string(value).map(Self::text)
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = usage;
        self
    }
}

impl Stub {
    /// Declares where calls for `role` are routed, as a routing file would. The call records of
    /// that role name this destination. A role with no declared route has no destination, and its
    /// calls are recorded as the stub's own.
    pub fn routed(mut self, role: Role, destination: Destination) -> Self {
        self.routes.push((role, destination));
        self
    }

    /// The destination `role` was declared with, if any.
    fn route(&self, role: Role) -> Option<&Destination> {
        self.routes
            .iter()
            .find(|(routed, _)| *routed == role)
            .map(|(_, destination)| destination)
    }

    /// Adds a reply for `role` when `matches` returns true. The first matching rule wins.
    pub fn when(
        mut self,
        role: Role,
        matches: impl Fn(&ChatRequest) -> bool + Send + Sync + 'static,
        reply: Reply,
    ) -> Self {
        self.rules.push(Rule {
            role,
            matches: Box::new(matches),
            reply,
        });
        self
    }

    pub fn ledger(&self) -> &UsageLedger {
        &self.ledger
    }
}

impl fmt::Debug for Stub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stub")
            .field("rules", &self.rules.len())
            .finish_non_exhaustive()
    }
}

impl ModelClient for Stub {
    fn destination(&self, role: Role) -> Option<Destination> {
        self.route(role).cloned()
    }

    async fn chat(&self, role: Role, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let rule = self
            .rules
            .iter()
            .find(|rule| rule.role == role && (rule.matches)(&request))
            .ok_or(LlmError::NoScriptedReply { role })?;
        let (provider, model) = match self.route(role) {
            Some(destination) => (destination.provider.clone(), destination.model.clone()),
            None => ("stub".to_owned(), "stub".to_owned()),
        };
        let call = CallRecord {
            role,
            provider,
            model,
            usage: rule.reply.usage,
            latency: Duration::ZERO,
        };
        self.ledger.record(call.clone());
        Ok(ChatResponse {
            content: rule.reply.content.clone(),
            tool_calls: rule.reply.tool_calls.clone(),
            finish_reason: Some("stop".to_owned()),
            call,
        })
    }
}
