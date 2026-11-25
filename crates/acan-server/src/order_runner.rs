//! One Droid turn per order attempt, with developer decisions routed through Cards.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use acan_droid::{
    AskAnswer, AskQuestion, AskUserReply, AskUserRequest, DroidError, DroidEvent, DroidSession,
    Headroom, PermissionDetails, PermissionReply, PermissionRequest, RequestId, SessionInit,
    SessionSettings, SpawnConfig,
};
use acan_hook::{ENDPOINT_ENV, ORDER_ENV, transport::Endpoint};
use acan_intent::{
    Block, CardEvidenceProposal, MAX_BLOCKS, MAX_PROSE_CHARS, MAX_SNIPPET_CHARS, Proposal,
    ValidatedProposal, validate_proposal,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::runtime::Handle;
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

use crate::auth::Principal;

/// One daemon may run two orders at once, including orders waiting for a Card answer.
pub const MAX_CONCURRENT_ORDERS: usize = 2;
/// The maximum length in characters of an Other or resume note.
pub const MAX_ONE_LINE_CHARS: usize = 500;

const MAX_QUEUED_REQUESTS: usize = 16;
const MAX_ORDER_ID_CHARS: usize = 128;
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
const RESTART_REASON: &str = "daemon restarted before order finished";
const RESUME: &str = "resume";
const CANCEL: &str = "cancel";
const DENY: &str = "deny";

/// The runner's current state for an order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OrderRunState {
    Running,
    WaitingCard {
        card_id: String,
    },
    PausedLimit {
        reason: String,
        retry_after_ms: Option<u64>,
    },
    Interrupted {
        reason: String,
    },
    Completed,
    Failed {
        reason: String,
    },
    Cancelled,
}

/// A decision or pause surfaced to the developer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Card {
    pub id: String,
    pub order_id: String,
    pub request_id: Option<RequestId>,
    pub kind: CardKind,
    pub title: String,
    pub summary: String,
    pub options: Vec<CardOption>,
    pub allow_other: bool,
    pub evidence: Vec<Block>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CardKind {
    Permission,
    Question,
    ScopeApproval,
    RateLimit,
    Interrupted,
    Interlock,
    Review,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CardOption {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
}

/// A decision about the current Card, not a message to an open-ended chat.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CardDecision {
    SelectOption { option_id: String },
    Other { note: String },
    Cancel,
}

impl Card {
    fn new(order_id: &str, kind: CardKind, title: &str, summary: String) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            order_id: order_id.to_owned(),
            request_id: None,
            kind,
            title: title.to_owned(),
            summary,
            options: Vec::new(),
            allow_other: false,
            evidence: Vec::new(),
        }
    }

    /// Presents Droid's exact permission choices, including the inner calls of a Script.
    #[must_use]
    pub fn from_permission(order_id: &str, id: &RequestId, request: &PermissionRequest) -> Self {
        let mut card = Self::new(
            order_id,
            CardKind::Permission,
            "Tool permission",
            "Review all covered tool calls before approving.".to_owned(),
        );
        card.request_id = Some(id.clone());
        let has_calls = !request.tool_uses.is_empty() || request.script_calls().next().is_some();
        let Some(blocks) = has_calls
            .then(|| permission_blocks(&card.summary, request))
            .flatten()
        else {
            card.summary = if has_calls {
                "This permission request is too large to review completely. Deny or cancel it."
            } else {
                "Droid did not provide any tool calls to review. Deny or cancel this request."
            }
            .to_owned();
            if request.options.iter().any(|option| option.value == DENY) {
                card.offer(DENY, "Deny");
            }
            card.offer(CANCEL, "Cancel");
            return card;
        };
        card.evidence = blocks;
        card.options = request
            .options
            .iter()
            .map(|option| CardOption {
                id: option.value.clone(),
                label: option.label.clone(),
                description: None,
            })
            .collect();
        card
    }

    /// Presents one Card per question, in Droid's order. The runner replies with a single batch.
    pub fn from_ask_user<'a>(
        order_id: &'a str,
        id: &'a RequestId,
        request: &'a AskUserRequest,
    ) -> impl Iterator<Item = Self> + 'a {
        request
            .questions
            .iter()
            .map(move |question| Self::question(order_id, id, question))
    }

    fn question(order_id: &str, id: &RequestId, question: &AskQuestion) -> Self {
        let mut card = Self::new(
            order_id,
            CardKind::Question,
            question.topic.as_deref().unwrap_or("Question from Droid"),
            question.question.clone(),
        );
        card.request_id = Some(id.clone());
        card.allow_other = true;
        card.options = question
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| CardOption {
                id: option.clone(),
                label: option.clone(),
                description: question
                    .option_descriptions
                    .as_ref()
                    .and_then(|descriptions| descriptions.get(index).cloned()),
            })
            .collect();
        card
    }

    fn pause(
        order_id: &str,
        kind: CardKind,
        title: &str,
        summary: String,
        resumable: bool,
    ) -> Self {
        let mut card = Self::new(order_id, kind, title, summary);
        if resumable {
            card.allow_other = true;
            card.offer(RESUME, "Resume");
        }
        card.offer(CANCEL, "Cancel");
        card
    }

    fn offer(&mut self, id: &str, label: &str) {
        self.options.push(CardOption {
            id: id.to_owned(),
            label: label.to_owned(),
            description: None,
        });
    }

    fn validate(&self, decision: &CardDecision) -> Result<(), OrderRunnerError> {
        match decision {
            CardDecision::SelectOption { option_id }
                if !self.options.iter().any(|option| option.id == *option_id) =>
            {
                Err(OrderRunnerError::InvalidOption)
            }
            CardDecision::Other { .. } if !self.allow_other => {
                Err(OrderRunnerError::OtherNotAllowed)
            }
            CardDecision::Other { note } => validate_note(note),
            _ => Ok(()),
        }
    }
}

// Keep each tool use and inner call identifiable, with its entire JSON input and confirmation.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionDisplay<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_id: Option<&'a str>,
    confirmation_type: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    arguments: Option<&'a Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Map<String, Value>>,
}

fn permission_blocks(summary: &str, request: &PermissionRequest) -> Option<Vec<Block>> {
    let mut blocks = Vec::new();
    let details = request.details.as_ref();
    if request.confirmation_type.is_some()
        || details.is_some_and(|details| !details.fields.is_empty())
    {
        push_permission_block(
            &mut blocks,
            "Permission confirmation".to_owned(),
            PermissionDisplay {
                tool_name: None,
                tool_id: None,
                confirmation_type: request.confirmation_type.as_deref(),
                arguments: None,
                details: details.map(|details| &details.fields),
            },
        )?;
    }
    push_script_blocks(&mut blocks, "Permission", details)?;
    for (index, item) in request.tool_uses.iter().enumerate() {
        let caption = format!("Tool use {}", index + 1);
        let details = item.details.as_ref();
        push_permission_block(
            &mut blocks,
            caption.clone(),
            PermissionDisplay {
                tool_name: Some(&item.tool_use.name),
                tool_id: Some(&item.tool_use.id),
                confirmation_type: item.confirmation_type.as_deref(),
                arguments: Some(&item.tool_use.input),
                details: details.map(|details| &details.fields),
            },
        )?;
        push_script_blocks(&mut blocks, &caption, details)?;
    }
    validate_card_blocks(summary, blocks)
}

fn push_script_blocks(
    blocks: &mut Vec<Block>,
    caption: &str,
    details: Option<&PermissionDetails>,
) -> Option<()> {
    if let Some(details) = details {
        for (index, call) in details.calls.iter().enumerate() {
            push_permission_block(
                blocks,
                format!("{caption} / Script call {}", index + 1),
                PermissionDisplay {
                    tool_name: Some(&call.tool_name),
                    tool_id: None,
                    confirmation_type: Some(&call.confirmation.kind),
                    arguments: Some(&call.tool_input),
                    details: Some(&call.confirmation.fields),
                },
            )?;
        }
    }
    Some(())
}

fn push_permission_block(
    blocks: &mut Vec<Block>,
    caption: String,
    details: PermissionDisplay<'_>,
) -> Option<()> {
    if blocks.len() >= MAX_BLOCKS {
        return None;
    }
    let json = serde_json::to_string(&details).ok()?;
    if json.chars().nth(MAX_SNIPPET_CHARS).is_some() {
        return None;
    }
    let mut code = String::with_capacity(json.len());
    for ch in json.chars() {
        if unsafe_line_char(ch) {
            // serde_json leaves some Unicode controls and bidi characters unescaped.
            write!(code, "\\u{:04x}", u32::from(ch)).expect("write to String");
        } else {
            code.push(ch);
        }
    }
    blocks.push(Block::CodeSnippet {
        language: Some("json".to_owned()),
        code,
        caption: Some(caption),
    });
    Some(())
}

fn validate_card_blocks(summary: &str, blocks: Vec<Block>) -> Option<Vec<Block>> {
    let proposal = Proposal::CardEvidence(CardEvidenceProposal {
        summary: summary.to_owned(),
        blocks,
    });
    match validate_proposal(proposal).ok()?.into_inner() {
        Proposal::CardEvidence(evidence) => Some(evidence.blocks),
        _ => unreachable!("validated proposals preserve their place"),
    }
}

/// Authority to act on a Card. Only derive it from the server's authenticated principal,
/// never a principal supplied in a request body.
#[derive(Debug)]
pub struct DeveloperAuthority(());

impl DeveloperAuthority {
    /// Refuses agent principals, regardless of which agent or token they represent.
    pub fn from_principal(principal: &Principal) -> Result<Self, OrderRunnerError> {
        match principal {
            Principal::Member(_) => Ok(Self(())),
            Principal::Agent(_) => Err(OrderRunnerError::AgentApprovalForbidden),
        }
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OrderRunnerError {
    #[error("at most {limit} orders can run at once")]
    ConcurrencyLimitReached { limit: usize },
    #[error("the order is already active")]
    OrderAlreadyActive,
    #[error("the paused or interrupted order needs a developer decision")]
    ResumeRequired,
    #[error("only a developer can answer a Card")]
    AgentApprovalForbidden,
    #[error("Card not found")]
    CardNotFound,
    #[error("the Card has already been resolved")]
    CardAlreadyResolved,
    #[error("the option was not offered on this Card")]
    InvalidOption,
    #[error("this Card does not accept an Other note")]
    OtherNotAllowed,
    #[error("a note must be a non-empty single line of at most {MAX_ONE_LINE_CHARS} characters")]
    InvalidNote,
    #[error(
        "an order id must be 1 to {MAX_ORDER_ID_CHARS} characters, without whitespace or controls"
    )]
    InvalidOrderId,
    #[error("a hook endpoint must be a valid local or loopback address without controls")]
    InvalidHookEndpoint,
    #[error(
        "an order intent must be a non-empty single line of at most {MAX_PROSE_CHARS} characters"
    )]
    InvalidPrompt,
    #[error("only a validated card_evidence proposal can supply Card blocks")]
    InvalidEvidencePlace,
    #[error("the proposed evidence would exceed the Card display limits")]
    EvidenceExceedsCardLimits,
    #[error("order not found")]
    OrderNotFound,
    #[error("only a paused or interrupted order can be resumed")]
    OrderNotResumable,
    #[error("the server has no launch configuration for this interrupted order")]
    ResumeSpecMissing,
    #[error("only a running, paused or interrupted order can be cancelled")]
    OrderNotCancellable,
    #[error("persisted orders must be loaded before starting any orders")]
    RunnerAlreadyLoaded,
    #[error("more than one persisted record has the same order id")]
    DuplicatePersistedOrder,
    #[error("starting a Droid session requires a Tokio runtime")]
    RuntimeUnavailable,
}

// Match GenUI's single-line rules, including Unicode line separators and bidi controls.
fn single_line(text: &str, max: usize) -> bool {
    text.chars().nth(max).is_none()
        && !text.trim().is_empty()
        && !text.chars().any(unsafe_line_char)
}

fn unsafe_line_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

fn validate_order_id(order_id: &str) -> Result<(), OrderRunnerError> {
    (single_line(order_id, MAX_ORDER_ID_CHARS) && !order_id.chars().any(char::is_whitespace))
        .then_some(())
        .ok_or(OrderRunnerError::InvalidOrderId)
}

fn validate_note(note: &str) -> Result<(), OrderRunnerError> {
    single_line(note, MAX_ONE_LINE_CHARS)
        .then_some(())
        .ok_or(OrderRunnerError::InvalidNote)
}

/// What the caller supplies to start an order in its worktree.
#[derive(Clone, Debug)]
pub struct OrderRunSpec<'a> {
    pub order_id: String,
    pub prompt: String,
    pub spawn_config: SpawnConfig,
    pub init: SessionInit<'a>,
    pub settings: SessionSettings,
    pub startup_timeout: Duration,
    pub hook_endpoint: Option<String>,
}

#[derive(Debug)]
struct Launch {
    prompt: String,
    spawn_config: SpawnConfig,
    machine_id: String,
    cwd: PathBuf,
    settings: SessionSettings,
    startup_timeout: Duration,
}

impl OrderRunSpec<'_> {
    fn into_launch(self) -> Result<(String, Arc<Launch>), OrderRunnerError> {
        validate_order_id(&self.order_id)?;
        if !single_line(&self.prompt, MAX_PROSE_CHARS) {
            return Err(OrderRunnerError::InvalidPrompt);
        }
        let mut spawn_config = self
            .spawn_config
            .without_env_var(ORDER_ENV)
            .without_env_var(ENDPOINT_ENV);
        if let Some(endpoint) = self.hook_endpoint {
            if endpoint.chars().any(unsafe_line_char) || Endpoint::parse(&endpoint).is_err() {
                return Err(OrderRunnerError::InvalidHookEndpoint);
            }
            spawn_config = spawn_config
                .with_env_var(ORDER_ENV, &self.order_id)
                .with_env_var(ENDPOINT_ENV, endpoint);
        }
        let launch = Launch {
            prompt: self.prompt,
            spawn_config,
            machine_id: self.init.machine_id.to_owned(),
            cwd: self.init.cwd.to_owned(),
            settings: self.settings,
            startup_timeout: self.startup_timeout,
        };
        Ok((self.order_id, Arc::new(launch)))
    }
}

/// What a store needs to retain across restarts. Launch configuration and credentials stay
/// outside this record, and a live Card request cannot be carried over to a new Droid session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedOrderState {
    pub order_id: String,
    pub state: OrderRunState,
}

/// A restored state and the new Card, if the developer needs to act.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconciledOrder {
    pub order_id: String,
    pub state: OrderRunState,
    pub card: Option<Card>,
}

/// Restores durable states without launching a process or offering to resume an order.
/// Returns an error if a persisted order ID is invalid.
pub fn reconcile_on_startup(
    records: Vec<PersistedOrderState>,
) -> Result<Vec<ReconciledOrder>, OrderRunnerError> {
    reconcile_orders(records, |_| false)
}

fn reconcile_orders(
    records: Vec<PersistedOrderState>,
    resumable: impl Fn(&str) -> bool,
) -> Result<Vec<ReconciledOrder>, OrderRunnerError> {
    records
        .into_iter()
        .map(|PersistedOrderState { order_id, state }| {
            validate_order_id(&order_id)?;
            let state = match state {
                OrderRunState::Running
                | OrderRunState::WaitingCard { .. }
                | OrderRunState::PausedLimit { .. } => OrderRunState::Interrupted {
                    reason: RESTART_REASON.to_owned(),
                },
                state => state,
            };
            let card = match &state {
                OrderRunState::Interrupted { reason } => Some(Card::pause(
                    &order_id,
                    CardKind::Interrupted,
                    "Order interrupted",
                    reason.clone(),
                    resumable(&order_id),
                )),
                _ => None,
            };
            Ok(ReconciledOrder {
                order_id,
                state,
                card,
            })
        })
        .collect()
}

/// Owns the daemon's order slots and the sending end of each live approval channel.
/// Clones share the same table. Dropping the last manager cancels its running orders.
#[derive(Clone, Debug, Default)]
pub struct OrderRunnerManager {
    table: Arc<Mutex<Table>>,
}

#[derive(Debug, Default)]
struct Table {
    orders: BTreeMap<String, Order>,
    active: BTreeMap<String, Active>,
    cards: BTreeMap<String, PendingCard>,
    resolved: BTreeSet<String>,
}

#[derive(Debug)]
struct Order {
    state: OrderRunState,
    card_id: Option<String>,
    launch: Option<Arc<Launch>>,
}

#[derive(Debug)]
struct Active {
    cancel: Option<oneshot::Sender<()>>,
    stopped: watch::Receiver<()>,
}

#[derive(Debug)]
struct PendingCard {
    card: Card,
    answer: Option<oneshot::Sender<CardDecision>>,
    required_evidence: usize,
}

impl OrderRunnerManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserves a slot immediately or returns an error. Process or protocol failures become
    /// order states. Paused orders must be resumed with developer authority, not started again.
    pub fn start_order(&self, spec: OrderRunSpec<'_>) -> Result<(), OrderRunnerError> {
        let (order_id, launch) = spec.into_launch()?;
        let table = lock(&self.table);
        if table.active.contains_key(&order_id) {
            return Err(OrderRunnerError::OrderAlreadyActive);
        }
        if table.orders.get(&order_id).is_some_and(|order| {
            matches!(
                order.state,
                OrderRunState::PausedLimit { .. } | OrderRunState::Interrupted { .. }
            )
        }) {
            return Err(OrderRunnerError::ResumeRequired);
        }
        self.start_attempt(table, &order_id, launch, None)
    }

    /// Loads startup records into a fresh runner. The caller can supply trusted launch specs for
    /// interrupted orders; without a spec an order can still be cancelled, but not resumed.
    pub fn load_persisted_orders<'a>(
        &self,
        records: Vec<PersistedOrderState>,
        specs: impl IntoIterator<Item = OrderRunSpec<'a>>,
    ) -> Result<Vec<ReconciledOrder>, OrderRunnerError> {
        let mut launches = specs
            .into_iter()
            .map(OrderRunSpec::into_launch)
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let restored = reconcile_orders(records, |id| launches.contains_key(id))?;
        let mut next = Table::default();
        for record in &restored {
            let replaced = next.orders.insert(
                record.order_id.clone(),
                Order {
                    state: record.state.clone(),
                    card_id: None,
                    launch: record
                        .card
                        .as_ref()
                        .and_then(|_| launches.remove(&record.order_id)),
                },
            );
            if replaced.is_some() {
                return Err(OrderRunnerError::DuplicatePersistedOrder);
            }
            if let Some(card) = &record.card {
                next.open_card(card.clone(), None);
            }
        }
        let mut table = lock(&self.table);
        if !table.orders.is_empty() {
            return Err(OrderRunnerError::RunnerAlreadyLoaded);
        }
        *table = next;
        Ok(restored)
    }

    /// A consistent snapshot for the caller's durable store.
    #[must_use]
    pub fn persisted_states(&self) -> Vec<PersistedOrderState> {
        lock(&self.table)
            .orders
            .iter()
            .map(|(order_id, order)| PersistedOrderState {
                order_id: order_id.clone(),
                state: order.state.clone(),
            })
            .collect()
    }

    #[must_use]
    pub fn state(&self, order_id: &str) -> Option<OrderRunState> {
        Some(lock(&self.table).orders.get(order_id)?.state.clone())
    }

    #[must_use]
    pub fn pending_card(&self, order_id: &str) -> Option<Card> {
        let table = lock(&self.table);
        let card_id = table.orders.get(order_id)?.card_id.as_ref()?;
        Some(table.cards.get(card_id)?.card.clone())
    }

    /// Replaces a pending Card's supplemental evidence without changing its decision or
    /// replacing the actions shown for a permission request.
    pub fn attach_card_evidence(
        &self,
        card_id: &str,
        evidence: ValidatedProposal,
    ) -> Result<(), OrderRunnerError> {
        let Proposal::CardEvidence(evidence) = evidence.into_inner() else {
            return Err(OrderRunnerError::InvalidEvidencePlace);
        };
        let mut table = lock(&self.table);
        let pending = table.card_mut(card_id)?;
        let card = &mut pending.card;
        if card.kind == CardKind::Permission {
            let mut blocks = card.evidence[..pending.required_evidence].to_vec();
            blocks.extend(evidence.blocks);
            card.evidence = validate_card_blocks(&card.summary, blocks)
                .ok_or(OrderRunnerError::EvidenceExceedsCardLimits)?;
        } else {
            card.evidence = evidence.blocks;
        }
        Ok(())
    }

    /// Delivers one validated decision to the waiting Droid attempt, or acts on a paused order.
    /// Invalid decisions leave the Card pending. A live Card's Cancel declines the request;
    /// [`Self::cancel_order`] stops the order itself.
    pub fn answer_card(
        &self,
        _authority: &DeveloperAuthority,
        card_id: &str,
        decision: CardDecision,
    ) -> Result<(), OrderRunnerError> {
        let mut table = lock(&self.table);
        let pending = table.card_mut(card_id)?;
        pending.card.validate(&decision)?;
        let order_id = pending.card.order_id.clone();
        if let Some(answer) = pending.answer.take() {
            table.close_card(&order_id);
            return answer
                .send(decision)
                .map_err(|_| OrderRunnerError::CardAlreadyResolved);
        }
        match decision {
            CardDecision::Other { note } => self.resume(table, &order_id, Some(note)),
            CardDecision::SelectOption { option_id } if option_id == RESUME => {
                self.resume(table, &order_id, None)
            }
            _ => {
                table.cancel_paused(&order_id);
                Ok(())
            }
        }
    }

    /// Starts a fresh Droid session in the same worktree with the original intent and optional
    /// developer note. The reported retry delay is advisory; no timer retries an order.
    pub fn resume_order(
        &self,
        _authority: &DeveloperAuthority,
        order_id: &str,
        note: Option<String>,
    ) -> Result<(), OrderRunnerError> {
        if let Some(note) = &note {
            validate_note(note)?;
        }
        self.resume(lock(&self.table), order_id, note)
    }

    /// Stops an order and waits for a live child to be cleaned up before returning its slot.
    pub async fn cancel_order(
        &self,
        _authority: &DeveloperAuthority,
        order_id: &str,
    ) -> Result<(), OrderRunnerError> {
        let mut stopped = {
            let mut table = lock(&self.table);
            if let Some(active) = table.active.get_mut(order_id) {
                if let Some(cancel) = active.cancel.take() {
                    let _ = cancel.send(());
                }
                let stopped = active.stopped.clone();
                table.close_card(order_id);
                stopped
            } else {
                match table.orders.get(order_id).map(|order| &order.state) {
                    Some(OrderRunState::PausedLimit { .. } | OrderRunState::Interrupted { .. }) => {
                        table.cancel_paused(order_id);
                    }
                    Some(OrderRunState::Cancelled) => {}
                    Some(_) => return Err(OrderRunnerError::OrderNotCancellable),
                    None => return Err(OrderRunnerError::OrderNotFound),
                }
                return Ok(());
            }
        };
        // Only the worker holds the sender; its drop follows cleanup and slot release.
        let _ = stopped.changed().await;
        Ok(())
    }

    fn resume(
        &self,
        table: MutexGuard<'_, Table>,
        order_id: &str,
        note: Option<String>,
    ) -> Result<(), OrderRunnerError> {
        let order = table
            .orders
            .get(order_id)
            .ok_or(OrderRunnerError::OrderNotFound)?;
        if !matches!(
            order.state,
            OrderRunState::PausedLimit { .. } | OrderRunState::Interrupted { .. }
        ) {
            return Err(OrderRunnerError::OrderNotResumable);
        }
        let launch = order
            .launch
            .clone()
            .ok_or(OrderRunnerError::ResumeSpecMissing)?;
        self.start_attempt(table, order_id, launch, note)
    }

    fn start_attempt(
        &self,
        mut table: MutexGuard<'_, Table>,
        order_id: &str,
        launch: Arc<Launch>,
        note: Option<String>,
    ) -> Result<(), OrderRunnerError> {
        if table.active.len() >= MAX_CONCURRENT_ORDERS {
            return Err(OrderRunnerError::ConcurrencyLimitReached {
                limit: MAX_CONCURRENT_ORDERS,
            });
        }
        let runtime = Handle::try_current().map_err(|_| OrderRunnerError::RuntimeUnavailable)?;
        table.close_card(order_id);
        table.orders.insert(
            order_id.to_owned(),
            Order {
                state: OrderRunState::Running,
                card_id: None,
                launch: Some(launch.clone()),
            },
        );
        let (cancel, cancelled) = oneshot::channel();
        let (stop, stopped) = watch::channel(());
        table.active.insert(
            order_id.to_owned(),
            Active {
                cancel: Some(cancel),
                stopped,
            },
        );
        let mut worker = Worker {
            table: Arc::downgrade(&self.table),
            order_id: order_id.to_owned(),
            finished: false,
            _stop: stop,
        };
        drop(table);
        runtime.spawn(async move {
            let outcome = worker.run(&launch, note, cancelled).await;
            worker.finish(outcome);
        });
        Ok(())
    }
}

fn lock(table: &Mutex<Table>) -> MutexGuard<'_, Table> {
    table.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Table {
    fn order_mut(&mut self, order_id: &str) -> &mut Order {
        self.orders.get_mut(order_id).expect("order is registered")
    }

    fn card_mut(&mut self, card_id: &str) -> Result<&mut PendingCard, OrderRunnerError> {
        self.cards.get_mut(card_id).ok_or_else(|| {
            if self.resolved.contains(card_id) {
                OrderRunnerError::CardAlreadyResolved
            } else {
                OrderRunnerError::CardNotFound
            }
        })
    }

    fn open_card(&mut self, card: Card, answer: Option<oneshot::Sender<CardDecision>>) {
        let card_id = card.id.clone();
        self.order_mut(&card.order_id).card_id = Some(card_id.clone());
        let required_evidence = card.evidence.len();
        self.cards.insert(
            card_id,
            PendingCard {
                card,
                answer,
                required_evidence,
            },
        );
    }

    fn close_card(&mut self, order_id: &str) {
        if let Some(order) = self.orders.get_mut(order_id)
            && let Some(card_id) = order.card_id.take()
        {
            if matches!(order.state, OrderRunState::WaitingCard { .. }) {
                order.state = OrderRunState::Running;
            }
            self.cards.remove(&card_id);
            self.resolved.insert(card_id);
        }
    }

    fn cancel_paused(&mut self, order_id: &str) {
        self.close_card(order_id);
        let order = self.order_mut(order_id);
        order.state = OrderRunState::Cancelled;
        order.launch = None;
    }
}

enum Outcome {
    Completed,
    Cancelled,
    Failed(String),
    Limited {
        message: String,
        retry_after: Option<Duration>,
        headroom: Headroom,
    },
}

impl From<DroidError> for Outcome {
    fn from(error: DroidError) -> Self {
        match error {
            DroidError::RateLimited {
                message,
                retry_after,
                headroom,
            } => Self::Limited {
                message,
                retry_after,
                headroom,
            },
            error => Self::Failed(error.to_string()),
        }
    }
}

struct Worker {
    table: Weak<Mutex<Table>>,
    order_id: String,
    finished: bool,
    _stop: watch::Sender<()>,
}

impl Worker {
    async fn run(
        &self,
        launch: &Launch,
        note: Option<String>,
        mut cancelled: oneshot::Receiver<()>,
    ) -> Outcome {
        let mut session = tokio::select! {
            biased;
            _ = &mut cancelled => return Outcome::Cancelled,
            spawned = spawn_droid(&launch.spawn_config) => match spawned {
                Ok(session) => session,
                Err(error) => return error.into(),
            },
        };
        let outcome = tokio::select! {
            biased;
            _ = &mut cancelled => Outcome::Cancelled,
            Err(outcome) = self.drive(&mut session, launch, note) => outcome,
        };
        if matches!(outcome, Outcome::Completed) {
            let _ = session.shutdown(CLOSE_TIMEOUT).await;
        }
        let _ = session.kill().await;
        // Drop the owned process before finish makes the slot available, even if cleanup errored.
        drop(session);
        outcome
    }

    async fn drive(
        &self,
        session: &mut DroidSession,
        launch: &Launch,
        note: Option<String>,
    ) -> Result<std::convert::Infallible, Outcome> {
        let prompt = match note {
            Some(note) => format!("{} (Developer resume note: {note})", launch.prompt),
            None => launch.prompt.clone(),
        };
        session
            .initialize(
                SessionInit {
                    machine_id: &launch.machine_id,
                    cwd: &launch.cwd,
                },
                &launch.settings,
                launch.startup_timeout,
            )
            .await?;
        session.check_startup_rate_limit().await?;
        session.send_user_message(&prompt).await?;
        let mut turn = LiveTurn {
            worker: self,
            session,
            queued: VecDeque::new(),
        };
        loop {
            let request = match turn.queued.pop_front() {
                Some(request) => request,
                None => next_request(turn.session).await?,
            };
            turn.answer(request).await?;
        }
    }

    fn open_card(&self, card: Card) -> Result<oneshot::Receiver<CardDecision>, Outcome> {
        let table = self.table.upgrade().ok_or(Outcome::Cancelled)?;
        let mut table = lock(&table);
        if table.active[&self.order_id].cancel.is_none() {
            return Err(Outcome::Cancelled);
        }
        let (answer, decision) = oneshot::channel();
        table.order_mut(&self.order_id).state = OrderRunState::WaitingCard {
            card_id: card.id.clone(),
        };
        table.open_card(card, Some(answer));
        Ok(decision)
    }

    fn finish(&mut self, outcome: Outcome) {
        self.finished = true;
        let Some(table) = self.table.upgrade() else {
            return;
        };
        let mut table = lock(&table);
        let active = table.active.remove(&self.order_id).expect("slot is held");
        let outcome = if active.cancel.is_none() {
            Outcome::Cancelled
        } else {
            outcome
        };
        table.close_card(&self.order_id);
        let order = table.order_mut(&self.order_id);
        order.state = match outcome {
            Outcome::Completed => OrderRunState::Completed,
            Outcome::Cancelled => OrderRunState::Cancelled,
            Outcome::Failed(reason) => OrderRunState::Failed { reason },
            Outcome::Limited {
                message,
                retry_after,
                headroom,
            } => {
                let retry_after_ms =
                    retry_after.map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
                order.state = OrderRunState::PausedLimit {
                    reason: message.clone(),
                    retry_after_ms,
                };
                let mut summary = message;
                if let Headroom::Known { remaining, limit } = headroom {
                    summary.push_str(&format!(" ({remaining} of {limit} remaining)"));
                }
                table.open_card(
                    Card::pause(
                        &self.order_id,
                        CardKind::RateLimit,
                        "Droid rate limit",
                        summary,
                        true,
                    ),
                    None,
                );
                return;
            }
        };
        order.launch = None;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(Outcome::Failed(
                "the Droid runner stopped unexpectedly".to_owned(),
            ));
        }
    }
}

async fn spawn_droid(config: &SpawnConfig) -> Result<DroidSession, DroidError> {
    #[cfg(target_os = "linux")]
    {
        // A concurrent fork can briefly inherit a newly written shim's write descriptor.
        // Retry only ETXTBSY at launch, never a protocol or rate-limit error.
        for _ in 0..19 {
            match DroidSession::spawn(config).await {
                Err(DroidError::Io(error))
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                result => return result,
            }
        }
    }
    DroidSession::spawn(config).await
}

enum LiveRequest {
    Permission(RequestId, PermissionRequest),
    Question(RequestId, AskUserRequest),
}

async fn next_request(session: &mut DroidSession) -> Result<LiveRequest, Outcome> {
    loop {
        match session.next_event().await? {
            Some(DroidEvent::PermissionRequested { id, request }) => {
                return Ok(LiveRequest::Permission(id, request));
            }
            Some(DroidEvent::AskUserRequested { id, request }) => {
                return Ok(LiveRequest::Question(id, request));
            }
            Some(DroidEvent::RateLimited {
                message,
                retry_after,
                headroom,
            }) => {
                return Err(Outcome::Limited {
                    message,
                    retry_after,
                    headroom,
                });
            }
            Some(DroidEvent::TurnCompleted { .. }) if session.finished() => {
                return Err(Outcome::Completed);
            }
            Some(DroidEvent::TurnCompleted { reason, .. }) => {
                return Err(match reason.as_deref() {
                    Some("cancelled" | "canceled") => Outcome::Cancelled,
                    Some(reason) => Outcome::Failed(format!("Droid ended the turn: {reason}")),
                    None => Outcome::Failed("Droid ended an unfinished turn".to_owned()),
                });
            }
            Some(DroidEvent::RequestFailed { error, .. }) => {
                return Err(DroidError::Rpc(error.code, error.message).into());
            }
            Some(DroidEvent::UnknownRequest { method, .. }) => {
                return Err(Outcome::Failed(format!(
                    "unsupported Droid request: {method}"
                )));
            }
            Some(DroidEvent::Exited { .. }) | None => {
                return Err(DroidError::Exited("the order's turn").into());
            }
            _ => {}
        }
    }
}

struct LiveTurn<'a> {
    worker: &'a Worker,
    session: &'a mut DroidSession,
    queued: VecDeque<LiveRequest>,
}

impl LiveTurn<'_> {
    async fn answer(&mut self, request: LiveRequest) -> Result<(), Outcome> {
        let order_id = &self.worker.order_id;
        match request {
            LiveRequest::Permission(id, request) => {
                let card = Card::from_permission(order_id, &id, &request);
                let selected_option = match self.wait(card).await? {
                    CardDecision::Cancel => CANCEL.to_owned(),
                    CardDecision::SelectOption { option_id } => option_id,
                    _ => return Err(Outcome::Cancelled),
                };
                let selected_option = if request.options.iter().any(|o| o.value == selected_option)
                {
                    selected_option
                } else if selected_option == CANCEL
                    && request.options.iter().any(|o| o.value == DENY)
                {
                    DENY.to_owned()
                } else {
                    // The permission RPC has no separate decline response. Never invent an option.
                    return Err(Outcome::Cancelled);
                };
                self.session
                    .reply_permission(&id, &PermissionReply { selected_option })
                    .await?;
            }
            LiveRequest::Question(id, request) => {
                let mut answers = Vec::new();
                for (question, card) in request
                    .questions
                    .iter()
                    .zip(Card::from_ask_user(order_id, &id, &request))
                {
                    let answer = match self.wait(card).await? {
                        CardDecision::SelectOption { option_id } => option_id,
                        CardDecision::Other { note } => note,
                        CardDecision::Cancel => {
                            self.session
                                .reply_ask_user(&id, &AskUserReply::Declined)
                                .await?;
                            return Ok(());
                        }
                    };
                    answers.push(AskAnswer {
                        index: question.index,
                        question: question.question.clone(),
                        answer,
                    });
                }
                self.session
                    .reply_ask_user(&id, &AskUserReply::Answers(answers))
                    .await?;
            }
        }
        Ok(())
    }

    async fn wait(&mut self, card: Card) -> Result<CardDecision, Outcome> {
        let mut decision = self.worker.open_card(card)?;
        loop {
            tokio::select! {
                biased;
                answer = &mut decision => return answer.map_err(|_| Outcome::Cancelled),
                signal = next_request(self.session) => match signal {
                    Ok(request) if self.queued.len() < MAX_QUEUED_REQUESTS => {
                        self.queued.push_back(request);
                    }
                    Ok(_) => return Err(Outcome::Failed("too many pending Droid requests".to_owned())),
                    Err(Outcome::Completed) => return Err(Outcome::Failed(
                        "Droid completed before its Card was answered".to_owned(),
                    )),
                    Err(outcome) => return Err(outcome),
                },
            }
        }
    }
}
