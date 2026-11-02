//! The portable receipt: plain JSON, deterministic pull request Markdown, and the plant view the
//! UI draws from one receipt.
//!
//! Each function here is a pure function of its inputs, so the same receipt renders to the same
//! bytes. A renderer prints the receipt's own classes, counts and denominators, and refuses a
//! receipt whose counts do not match its items. Nothing here reads model output: the plant view
//! takes its values from the receipt, the confirmed scope and the machine list the caller passes.

use std::collections::BTreeSet;
use std::iter;
use std::num::TryFromIntError;

use acan_intent::{
    Authorization, CanonicalError, CatalogVersion, CheckResult, Coverage, Difference, Evidence,
    EvidenceRow, ExampleView, InvalidValue, ItemClass, MachineId, MustKeepView, NonEmpty,
    NotEnforcedReason, ObligationId, OpenChoiceView, Outcome, PlantCounts, PlantItem, PlantMachine,
    PlantOrder, PlantView, PlantViewV1, ProbeHash, Receipt, ReceiptItem, RoutePath, RouteTemplate,
    RulingStatus, RulingTag, RulingTagState, Scope, ScopeItem, ScopeItemView, ScopeView, Selector,
    SequenceStep, UnixMillis, UnknownReason,
};
use serde::Serialize;
use serde_json::Value;

/// The caption catalog version the plant view names. It must equal the `catalog_version` of the
/// shipped caption catalog; `fixtures/caption-catalog.json` carries the same value.
const CATALOG_VERSION: &str = "captions-4";

/// Why a receipt could not be rendered. A renderer never prints a receipt it cannot check.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum RenderError {
    /// The class counts do not match the items, so no count is printed.
    #[error("receipt counts do not match its items: {0}")]
    Counts(InvalidValue),
    /// The receipt could not be hashed for its identity.
    #[error(transparent)]
    Canonical(#[from] CanonicalError),
    /// The receipt or one of its values could not be written as JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Why a plant view could not be built from a receipt.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum PlantViewError {
    /// The class counts do not match the items, so no view is built.
    #[error("receipt counts do not match its items: {0}")]
    Counts(InvalidValue),
    /// The confirmed scope is not the one the receipt was built from.
    #[error("scope {0} does not match the receipt")]
    ScopeMismatch(&'static str),
    /// A machine is listed twice.
    #[error("machine {0} is listed twice")]
    DuplicateMachine(MachineId),
    /// The receipt, scope or order names a machine the plant does not list.
    #[error("machine {0} is not in the plant")]
    UnknownMachine(MachineId),
    /// A count does not fit the view's integer type.
    #[error("{what} does not fit a plant view")]
    TooMany {
        /// The count that is too large.
        what: &'static str,
        /// Why the conversion failed.
        #[source]
        source: TryFromIntError,
    },
    /// A value the view needs is not valid.
    #[error(transparent)]
    Invalid(#[from] InvalidValue),
    /// The receipt or scope could not be hashed for its identity.
    #[error(transparent)]
    Canonical(#[from] CanonicalError),
    /// A value could not be written as JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// One machine of the plant as the caller reads it from the scan: its id and the route templates
/// whose static path passes through it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MachineInput {
    /// The machine.
    pub(crate) machine_id: MachineId,
    /// The route templates whose static path passes through it.
    pub(crate) routes: Vec<RouteTemplate>,
}

/// The order as the plate shows it. The receipt and the scope carry none of this: the sentence
/// and the pinned machine come from the order, and the confirmation time from the confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlateOrder {
    /// The order's sentence, as the human wrote it.
    pub(crate) sentence: String,
    /// The machine the order is pinned on.
    pub(crate) machine_id: MachineId,
    /// When the human confirmed the scope, if they have.
    pub(crate) confirmed_at: Option<UnixMillis>,
}

/// The receipt as plain JSON: the receipt's own serialization, fields in declaration order,
/// two-space indentation and one trailing newline.
///
/// # Errors
///
/// Returns [`RenderError`] when the counts do not match the items or the receipt cannot be
/// written.
pub(crate) fn render_json(receipt: &Receipt) -> Result<String, RenderError> {
    checked(receipt)?;
    let mut json = serde_json::to_string_pretty(receipt)?;
    json.push('\n');
    Ok(json)
}

/// The receipt as Markdown for a pull request. It names the base and merged commits, the tested
/// context, every class with its denominator, the probe and route denominators, the behavioral
/// denominator apart from route hits, every mask, every ruling with the notes that are not
/// enforced, every item with its bounce attempts, and the unknown, error and stale results.
///
/// # Errors
///
/// Returns [`RenderError`] when the counts do not match the items or the receipt cannot be
/// hashed or written.
pub(crate) fn render_markdown(receipt: &Receipt) -> Result<String, RenderError> {
    checked(receipt)?;
    let mut doc = Doc::default();
    write_summary(&mut doc, receipt)?;
    write_classes(&mut doc, receipt)?;
    write_denominators(&mut doc, receipt)?;
    write_unknowns(&mut doc, receipt)?;
    write_stale(&mut doc, receipt);
    write_masks(&mut doc, receipt);
    write_rulings(&mut doc, receipt);
    write_items(&mut doc, receipt)?;
    write_contests(&mut doc, receipt)?;
    write_context(&mut doc, receipt)?;
    Ok(doc.finish())
}

/// Builds the plant view of one receipt: the order, the machines, the confirmed scope, every
/// receipt item, the counts with their denominators, and no tour.
///
/// The view takes every value from the receipt, the confirmed `scope` and the machine list. It
/// refuses a scope that is not the receipt's, and a receipt, scope or order that names a machine
/// the list does not contain.
///
/// # Errors
///
/// Returns [`PlantViewError`] when the counts do not match the items, the scope is not the
/// receipt's, a machine is unknown or listed twice, or a value cannot be built.
pub(crate) fn plant_view(
    receipt: &Receipt,
    scope: &Scope,
    plate: &PlateOrder,
    machines: &[MachineInput],
) -> Result<PlantView, PlantViewError> {
    receipt.check_counts().map_err(PlantViewError::Counts)?;
    check_scope(receipt, scope)?;
    let known = known_machines(machines)?;
    ensure_machines_known(&known, receipt, scope, plate)?;
    let plant_machines = machines
        .iter()
        .map(|input| plant_machine(receipt, input))
        .collect::<Result<Vec<_>, _>>()?;
    let catalog_version: CatalogVersion = CATALOG_VERSION.parse()?;
    Ok(PlantView {
        version: PlantViewV1,
        receipt_hash: receipt.receipt_hash()?,
        base_sha: receipt.base_commit().clone(),
        merged_sha: receipt.merged_commit().clone(),
        gate_state: receipt.gate_state,
        order: PlantOrder {
            id: receipt.order.clone(),
            sentence: plate.sentence.clone(),
            machine_id: plate.machine_id.clone(),
            scope_rev: receipt.scope_rev,
            confirmed_at: plate.confirmed_at,
        },
        machines: plant_machines,
        scope: scope_view(scope)?,
        items: receipt.items.iter().map(plant_item).collect(),
        counts: plant_counts(receipt)?,
        catalog_version,
        tour: None,
    })
}

/// Fails before anything is printed when the stored counts are not the items' classes.
fn checked(receipt: &Receipt) -> Result<(), RenderError> {
    receipt.check_counts().map_err(RenderError::Counts)
}

fn check_scope(receipt: &Receipt, scope: &Scope) -> Result<(), PlantViewError> {
    if scope.order != receipt.order {
        return Err(PlantViewError::ScopeMismatch("order"));
    }
    if scope.rev != receipt.scope_rev {
        return Err(PlantViewError::ScopeMismatch("revision"));
    }
    if scope.base_commit != *receipt.base_commit() {
        return Err(PlantViewError::ScopeMismatch("base commit"));
    }
    if scope.scope_hash()? != receipt.scope_hash {
        return Err(PlantViewError::ScopeMismatch("hash"));
    }
    Ok(())
}

fn known_machines(machines: &[MachineInput]) -> Result<BTreeSet<&MachineId>, PlantViewError> {
    let mut known = BTreeSet::new();
    for input in machines {
        if !known.insert(&input.machine_id) {
            return Err(PlantViewError::DuplicateMachine(input.machine_id.clone()));
        }
    }
    Ok(known)
}

/// Every machine the order, the receipt, the rulings and the scope name must be in the plant
/// list. A view that dropped one would hide part of the work from the plate.
fn ensure_machines_known(
    known: &BTreeSet<&MachineId>,
    receipt: &Receipt,
    scope: &Scope,
    plate: &PlateOrder,
) -> Result<(), PlantViewError> {
    let referenced =
        iter::once(&plate.machine_id)
            .chain(receipt.items.iter().flat_map(|item| {
                iter::once(&item.machine_id).chain(item.route_path.machines.iter())
            }))
            .chain(
                receipt
                    .rulings
                    .iter()
                    .flat_map(|ruling| ruling.machine_ids.iter()),
            )
            .chain(
                scope
                    .body
                    .may
                    .iter()
                    .flat_map(|item| item.machine_ids.iter()),
            )
            .chain(
                scope
                    .body
                    .must_keep
                    .iter()
                    .flat_map(|rule| rule.machine_ids.iter()),
            )
            .chain(scope.body.open.iter().map(|choice| &choice.machine_id));
    for machine_id in referenced {
        if !known.contains(machine_id) {
            return Err(PlantViewError::UnknownMachine(machine_id.clone()));
        }
    }
    Ok(())
}

/// A machine's coverage and probes come from the receipt alone. A probe passes through a machine
/// when its item's route path names it. Coverage is `uncovered` when no probe passes through it
/// or every route template of it is uncovered, `partial` when some route template is, and
/// `covered` otherwise. A route is matched by its template alone, since the caller's route list
/// has no method. A template that is uncovered for one method counts as uncovered, which errs
/// toward showing less coverage.
fn plant_machine(receipt: &Receipt, input: &MachineInput) -> Result<PlantMachine, PlantViewError> {
    let probes: BTreeSet<ProbeHash> = receipt
        .items
        .iter()
        .filter(|item| {
            item.route_path
                .machines
                .iter()
                .any(|machine| machine == &input.machine_id)
        })
        .map(|item| item.probe_hash)
        .collect();
    let uncovered = input
        .routes
        .iter()
        .filter(|route| {
            receipt
                .routes
                .uncovered
                .iter()
                .any(|entry| &entry.route_template == *route)
        })
        .count();
    let coverage =
        if probes.is_empty() || (!input.routes.is_empty() && uncovered == input.routes.len()) {
            Coverage::Uncovered
        } else if uncovered > 0 {
            Coverage::Partial
        } else {
            Coverage::Covered
        };
    let rulings = receipt
        .rulings
        .iter()
        .filter(|ruling| ruling.machine_ids.iter().any(|id| id == &input.machine_id))
        .map(|ruling| RulingTag {
            ruling_id: ruling.ruling_id.clone(),
            enforced: !matches!(ruling.status, RulingStatus::NotEnforced { .. }),
            state: RulingTagState::Active,
            label: ruling.label.clone(),
        })
        .collect();
    Ok(PlantMachine {
        machine_id: input.machine_id.clone(),
        coverage,
        probes: u32::try_from(probes.len()).map_err(|source| PlantViewError::TooMany {
            what: "probes through a machine",
            source,
        })?,
        routes: input.routes.clone(),
        rulings,
    })
}

fn plant_item(item: &ReceiptItem) -> PlantItem {
    PlantItem {
        item_id: item.item_id.clone(),
        class: item.class,
        machine_id: item.machine_id.clone(),
        probe_id: item.probe_hash,
        episode_id: item.episode_id,
        ruling_id: item.ruling_id.clone(),
        attempts: item.attempts,
        budget: item.budget,
        state: item.state,
        route_path: item.route_path.clone(),
        sequence: item.sequence.clone(),
        evidence: item.evidence.clone(),
    }
}

fn plant_counts(receipt: &Receipt) -> Result<PlantCounts, PlantViewError> {
    Ok(PlantCounts {
        by_class: receipt.counts,
        probes_run: receipt.probes.run,
        probes_total: receipt.probes.total,
        routes_covered: receipt.routes.covered,
        routes_total: receipt.routes.total,
        masks_applied: u32::try_from(receipt.masks.len()).map_err(|source| {
            PlantViewError::TooMany {
                what: "masks",
                source,
            }
        })?,
        behavior: receipt.behavior,
    })
}

fn scope_view(scope: &Scope) -> Result<ScopeView, PlantViewError> {
    let items = scope
        .body
        .may
        .iter()
        .map(scope_item_view)
        .collect::<Result<Vec<_>, _>>()?;
    let must_keep = scope
        .body
        .must_keep
        .iter()
        .map(|rule| MustKeepView {
            id: rule.id.clone(),
            machine_ids: rule.machine_ids.clone(),
            text: rule.text.clone(),
            ruling_id: rule.ruling_id.clone(),
        })
        .collect();
    let open = scope
        .body
        .open
        .iter()
        .map(|choice| OpenChoiceView {
            id: choice.id.clone(),
            machine_id: choice.machine_id.clone(),
            question: choice.question.clone(),
            options: choice.options.clone(),
        })
        .collect();
    Ok(ScopeView {
        rev: scope.rev,
        items,
        must_keep,
        open,
    })
}

fn scope_item_view(item: &ScopeItem) -> Result<ScopeItemView, PlantViewError> {
    let obligations: Vec<ObligationId> = item
        .acceptance
        .iter()
        .map(|predicate| predicate.obligation_id.clone())
        .collect();
    Ok(ScopeItemView {
        item_id: item.id.clone(),
        machine_ids: item.machine_ids.clone(),
        selector_text: selector_text(&item.selector)?,
        obligations: NonEmpty::try_from(obligations)?,
        provenance: item.provenance,
        example: ExampleView {
            kind: item.example.kind,
            before: item.example.before.clone(),
            after: item.example.after.clone(),
        },
    })
}

/// A selector in the core's words: method and route, then channel and field, then principal.
fn selector_text(selector: &Selector) -> Result<String, serde_json::Error> {
    let method = wire(&selector.method)?;
    let channel = wire(&selector.channel)?;
    let principal = wire(&selector.principal)?;
    let field = selector
        .field_path
        .as_ref()
        .map(|path| format!(" {}", path.as_str()))
        .unwrap_or_default();
    Ok(format!(
        "{method} {} · {channel}{field} · {principal}",
        selector.route_template.as_str()
    ))
}

/// The wire words of a value: a string as itself, an object as its string fields joined by
/// spaces (`actor alice`), anything else as compact JSON.
fn wire<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    Ok(match serde_json::to_value(value)? {
        Value::String(word) => word,
        Value::Object(fields) => fields
            .values()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    })
}

/// Markdown under construction. Every block is appended whole, so a value cannot open a block
/// the renderer did not write.
#[derive(Default)]
struct Doc {
    out: String,
}

impl Doc {
    fn line(&mut self, text: impl AsRef<str>) {
        self.out.push_str(text.as_ref());
        self.out.push('\n');
    }

    /// Ensures a blank line before the next block, and none doubled.
    fn block(&mut self) {
        if self.out.is_empty() || self.out.ends_with("\n\n") {
            return;
        }
        if self.out.ends_with('\n') {
            self.out.push('\n');
        } else {
            self.out.push_str("\n\n");
        }
    }

    fn heading(&mut self, level: usize, text: &str) {
        self.block();
        self.line(format!("{} {text}", "#".repeat(level)));
        self.block();
    }

    fn para(&mut self, text: impl AsRef<str>) {
        self.block();
        self.line(text);
        self.block();
    }

    fn bullet(&mut self, text: impl AsRef<str>) {
        self.line(format!("- {}", text.as_ref()));
    }

    /// A table, or `None.` when it has no rows, so an empty list still says so.
    fn table(&mut self, headers: &[&str], rows: &[Vec<String>]) {
        if rows.is_empty() {
            self.para("None.");
            return;
        }
        self.block();
        self.line(format!("| {} |", headers.join(" | ")));
        self.line(format!("| {} |", vec!["---"; headers.len()].join(" | ")));
        for row in rows {
            self.line(format!("| {} |", row.join(" | ")));
        }
        self.block();
    }

    fn finish(self) -> String {
        let trimmed = self.out.trim_end_matches('\n');
        format!("{trimmed}\n")
    }
}

/// Inline code for an identifier or a value. A pipe is escaped so it cannot end a table cell,
/// and a line break becomes a space.
fn code(raw: &str) -> String {
    let flat = raw.replace(['\r', '\n'], " ").replace('|', "\\|");
    if flat.is_empty() {
        return "(empty)".to_owned();
    }
    let fence = "`".repeat(longest_backtick_run(&flat) + 1);
    let pad = if flat.starts_with('`') || flat.ends_with('`') {
        " "
    } else {
        ""
    };
    format!("{fence}{pad}{flat}{pad}{fence}")
}

fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0;
    let mut run = 0;
    for ch in text.chars() {
        if ch == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    longest
}

/// Free text from a model, an agent or a human, escaped so it cannot open markup or a table.
fn text(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\r' | '\n' => escaped.push(' '),
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '|' | '~' | '&' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            other => escaped.push(other),
        }
    }
    escaped
}

fn optional(value: Option<String>) -> String {
    value.map_or_else(|| "none".to_owned(), |raw| code(&raw))
}

fn yes_no(flag: bool) -> &'static str {
    if flag { "yes" } else { "no" }
}

fn evidence_text(evidence: &Evidence) -> String {
    match evidence {
        Evidence::Present { value } => code(&value.to_string()),
        Evidence::Missing {} => "missing".to_owned(),
    }
}

fn unknown_reason(reason: &UnknownReason) -> String {
    match reason {
        UnknownReason::UnstableBase {} => "unstable base".to_owned(),
        UnknownReason::Incomparable { detail } => format!("incomparable: {}", text(detail)),
        UnknownReason::Incomplete {} => "incomplete observation".to_owned(),
        UnknownReason::RunFailed {} => "run failed".to_owned(),
        UnknownReason::Stale {} => "stale evidence".to_owned(),
        UnknownReason::Skipped {} => "step skipped".to_owned(),
        UnknownReason::Error { detail } => format!("evaluator error: {}", text(detail)),
    }
}

fn check_text(result: &CheckResult) -> String {
    match result {
        CheckResult::Pass {} => "pass".to_owned(),
        CheckResult::Fail { detail } => format!("fail: {}", text(detail)),
        CheckResult::Unknown { reason } => format!("unknown: {}", unknown_reason(reason)),
    }
}

fn outcome_text(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Unchanged {} => "unchanged".to_owned(),
        Outcome::Changed { differences } => {
            format!("changed, {} difference(s)", differences.len())
        }
        Outcome::Unknown { reason } => format!("unknown: {}", unknown_reason(reason)),
    }
}

fn authorization_text(authorization: &Authorization) -> String {
    match authorization {
        Authorization::Ruling { ruling_id } => format!("ruling {}", code(ruling_id.as_str())),
        Authorization::MustKeep { must_keep_id } => {
            format!("must-keep {}", code(must_keep_id.as_str()))
        }
        Authorization::MayChange { item_id } => {
            format!("may-change item {}", code(item_id.as_str()))
        }
        Authorization::None {} => "none: base must be preserved".to_owned(),
    }
}

fn difference_text(difference: &Difference) -> String {
    format!(
        "probe {} step {} field {}: base {}, candidate {}",
        code(&difference.probe_hash.to_string()),
        code(difference.step_id.as_str()),
        code(difference.path.as_str()),
        evidence_text(&difference.base),
        evidence_text(&difference.candidate)
    )
}

fn not_enforced_text(reason: &NotEnforcedReason) -> String {
    match reason {
        NotEnforcedReason::Unexpressible { detail } => {
            format!("unexpressible: {}", text(detail))
        }
        NotEnforcedReason::Deferred {} => "deferred by the human".to_owned(),
    }
}

fn ruling_status_text(status: &RulingStatus) -> String {
    match status {
        RulingStatus::Held {} => "held".to_owned(),
        RulingStatus::Violated { item_ids } => {
            let items: Vec<String> = item_ids.iter().map(|id| code(id.as_str())).collect();
            format!("violated by {}", items.join(", "))
        }
        RulingStatus::NotReached {} => "not reached".to_owned(),
        RulingStatus::Stale {} => "stale".to_owned(),
        RulingStatus::Unknown { reason } => format!("unknown: {}", unknown_reason(reason)),
        RulingStatus::NotEnforced { reason } => {
            format!("NOT ENFORCED: {}", not_enforced_text(reason))
        }
    }
}

fn route_text(path: &RoutePath) -> Result<String, serde_json::Error> {
    let machines: Vec<&str> = path.machines.iter().map(MachineId::as_str).collect();
    Ok(format!(
        "{} ({})",
        machines.join(" > "),
        wire(&path.source)?
    ))
}

fn write_summary(doc: &mut Doc, receipt: &Receipt) -> Result<(), RenderError> {
    let context = &receipt.prepared.context;
    doc.heading(
        1,
        &format!("Acan receipt for order {}", code(receipt.order.as_str())),
    );
    doc.bullet(format!(
        "Receipt: {}",
        code(&receipt.receipt_hash()?.to_string())
    ));
    doc.bullet(format!("Version: {}", code(&wire(&receipt.version)?)));
    doc.bullet(format!(
        "Scope: revision {}, hash {}",
        receipt.scope_rev.0,
        code(&receipt.scope_hash.to_string())
    ));
    doc.bullet(format!("Gate state: {}", code(&wire(&receipt.gate_state)?)));
    doc.bullet(format!(
        "Base commit: {}",
        code(receipt.base_commit().as_str())
    ));
    doc.bullet(format!(
        "Merged commit (tested): {}",
        code(receipt.merged_commit().as_str())
    ));
    doc.bullet(format!(
        "Protected head: {}",
        code(context.protected_head.as_str())
    ));
    doc.block();
    Ok(())
}

fn write_classes(doc: &mut Doc, receipt: &Receipt) -> Result<(), RenderError> {
    doc.heading(2, "Items by class");
    let mut rows = Vec::new();
    for class in ItemClass::ALL {
        rows.push(vec![
            code(&wire(&class)?),
            receipt.counts.get(class).to_string(),
            yes_no(class.counts_toward_pass()).to_owned(),
        ]);
    }
    rows.push(vec![
        "**total items**".to_owned(),
        receipt.counts.total().to_string(),
        "-".to_owned(),
    ]);
    doc.para(format!(
        "Each count is out of {} items. Only `preserved` and `obligation_fulfilled` count toward a pass.",
        receipt.counts.total()
    ));
    doc.table(&["Class", "Items", "Counts toward pass"], &rows);
    Ok(())
}

fn write_denominators(doc: &mut Doc, receipt: &Receipt) -> Result<(), RenderError> {
    doc.heading(2, "Probes, routes and behavior");
    doc.para(format!(
        "Probes run: {} of {} ({} errored; errored probes never pass).",
        receipt.probes.run, receipt.probes.total, receipt.probes.errored
    ));
    doc.para(format!(
        "Route hits: {} of {} baseline routes. A route hit is not behavioral coverage.",
        receipt.routes.covered, receipt.routes.total
    ));
    let uncovered: Vec<Vec<String>> = receipt
        .routes
        .uncovered
        .iter()
        .map(|route| {
            Ok(vec![
                code(&wire(&route.method)?),
                code(route.route_template.as_str()),
                text(&route.reason),
            ])
        })
        .collect::<Result<_, RenderError>>()?;
    doc.para("Uncovered routes:");
    doc.table(&["Method", "Route", "Reason"], &uncovered);
    doc.para(format!(
        "Behavioral obligations: {} total; passed {}, failed {}, unknown {}. These count obligations, not route hits.",
        receipt.behavior.obligations_total,
        receipt.behavior.passed,
        receipt.behavior.failed,
        receipt.behavior.unknown
    ));
    Ok(())
}

fn write_unknowns(doc: &mut Doc, receipt: &Receipt) -> Result<(), RenderError> {
    doc.heading(2, "Unknown and error results");
    doc.para(
        "Items that are error, incomparable or unstable base, and items whose outcome or obligation has no answer. None of them pass.",
    );
    let mut lines = Vec::new();
    for item in &receipt.items {
        let mut reasons = Vec::new();
        if matches!(
            item.class,
            ItemClass::Error | ItemClass::Incomparable | ItemClass::UnstableBase
        ) {
            reasons.push(format!("class {}", code(&wire(&item.class)?)));
        }
        if let Outcome::Unknown { reason } = &item.evaluation.outcome {
            reasons.push(format!("outcome {}", unknown_reason(reason)));
        }
        for check in &item.evaluation.obligations {
            if let CheckResult::Unknown { reason } = &check.result {
                reasons.push(format!(
                    "obligation {} {}",
                    code(check.obligation_id.as_str()),
                    unknown_reason(reason)
                ));
            }
        }
        if !reasons.is_empty() {
            lines.push(format!(
                "Item {}: {}",
                code(item.item_id.as_str()),
                reasons.join("; ")
            ));
        }
    }
    if lines.is_empty() {
        doc.para("None.");
    } else {
        doc.block();
        for line in lines {
            doc.bullet(line);
        }
        doc.block();
    }
    Ok(())
}

fn write_stale(doc: &mut Doc, receipt: &Receipt) {
    doc.heading(2, "Stale evidence");
    doc.para("Evidence that belongs to an older commit or scope. Stale never passes.");
    let mut lines = Vec::new();
    for item in &receipt.items {
        let stale_outcome = matches!(
            item.evaluation.outcome,
            Outcome::Unknown {
                reason: UnknownReason::Stale {}
            }
        );
        let stale_check = item.evaluation.obligations.iter().any(|check| {
            matches!(
                check.result,
                CheckResult::Unknown {
                    reason: UnknownReason::Stale {}
                }
            )
        });
        if stale_outcome || stale_check {
            lines.push(format!(
                "Item {}: stale evidence",
                code(item.item_id.as_str())
            ));
        }
    }
    for ruling in &receipt.rulings {
        if matches!(ruling.status, RulingStatus::Stale {}) {
            lines.push(format!("Ruling {}: stale", code(ruling.ruling_id.as_str())));
        }
    }
    if lines.is_empty() {
        doc.para("None.");
    } else {
        doc.block();
        for line in lines {
            doc.bullet(line);
        }
        doc.block();
    }
}

fn write_masks(doc: &mut Doc, receipt: &Receipt) {
    doc.heading(2, "Masks");
    doc.para(format!(
        "Masks applied: {}. Each mask hides one maskable field on which the two base runs disagreed.",
        receipt.masks.len()
    ));
    let rows: Vec<Vec<String>> = receipt
        .masks
        .iter()
        .map(|mask| {
            vec![
                code(&mask.probe_hash.to_string()),
                code(mask.selector.step_id.as_str()),
                code(mask.selector.path.as_str()),
                code(mask.normalization_version.as_str()),
                code(&mask.normalization_hash.to_string()),
                code(&mask.base_a_observation_hash.to_string()),
                code(&mask.base_b_observation_hash.to_string()),
            ]
        })
        .collect();
    doc.table(
        &[
            "Probe",
            "Step",
            "Field",
            "Normalization",
            "Normalization hash",
            "Base A observation",
            "Base B observation",
        ],
        &rows,
    );
}

fn write_rulings(doc: &mut Doc, receipt: &Receipt) {
    doc.heading(2, "Rulings");
    doc.para(format!(
        "Rulings: {}. A NOT ENFORCED ruling is a note: it is shown and never counted as a check.",
        receipt.rulings.len()
    ));
    let mut rows = Vec::new();
    let mut notes = Vec::new();
    for ruling in &receipt.rulings {
        let not_enforced = matches!(ruling.status, RulingStatus::NotEnforced { .. });
        let machines: Vec<&str> = ruling.machine_ids.iter().map(MachineId::as_str).collect();
        let enforcement = if not_enforced {
            "NOT ENFORCED"
        } else {
            "enforced"
        };
        rows.push(vec![
            code(ruling.ruling_id.as_str()),
            ruling.event_seq.to_string(),
            text(&ruling.label),
            code(&machines.join(", ")),
            enforcement.to_owned(),
            ruling_status_text(&ruling.status),
        ]);
        if let RulingStatus::NotEnforced { reason } = &ruling.status {
            notes.push(format!(
                "NOT ENFORCED {} {}: {}",
                code(ruling.ruling_id.as_str()),
                text(&ruling.label),
                not_enforced_text(reason)
            ));
        }
    }
    doc.table(
        &[
            "Ruling",
            "Decision",
            "Label",
            "Machines",
            "Enforcement",
            "Status",
        ],
        &rows,
    );
    if !notes.is_empty() {
        doc.para("Notes that are not enforced:");
        for note in notes {
            doc.bullet(note);
        }
        doc.block();
    }
}

fn write_items(doc: &mut Doc, receipt: &Receipt) -> Result<(), RenderError> {
    doc.heading(2, "Items");
    doc.para(format!(
        "Items: {}. Bounce attempts read as attempts used of the repair budget.",
        receipt.counts.total()
    ));
    let mut rows = Vec::new();
    for item in &receipt.items {
        rows.push(vec![
            code(item.item_id.as_str()),
            code(&wire(&item.class)?),
            code(&wire(&item.state)?),
            code(item.machine_id.as_str()),
            format!("{} of {}", item.attempts, item.budget),
            code(&route_text(&item.route_path)?),
            optional(item.ruling_id.as_ref().map(|id| id.as_str().to_owned())),
            optional(item.episode_id.map(|id| id.to_string())),
        ]);
    }
    doc.table(
        &[
            "Item",
            "Class",
            "State",
            "Machine",
            "Bounce attempts",
            "Route path",
            "Ruling",
            "Episode",
        ],
        &rows,
    );
    for item in &receipt.items {
        write_item_detail(doc, item)?;
    }
    Ok(())
}

fn write_item_detail(doc: &mut Doc, item: &ReceiptItem) -> Result<(), RenderError> {
    let evaluation = &item.evaluation;
    doc.heading(3, &format!("Item {}", code(item.item_id.as_str())));
    doc.bullet(format!(
        "Class: {} (counts toward pass: {})",
        code(&wire(&item.class)?),
        yes_no(item.class.counts_toward_pass())
    ));
    doc.bullet(format!("State: {}", code(&wire(&item.state)?)));
    doc.bullet(format!(
        "Machine: {}; probe {}; episode {}",
        code(item.machine_id.as_str()),
        code(&item.probe_hash.to_string()),
        optional(item.episode_id.map(|id| id.to_string()))
    ));
    doc.bullet(format!(
        "Ruling: {}",
        optional(item.ruling_id.as_ref().map(|id| id.as_str().to_owned()))
    ));
    doc.bullet(format!(
        "Bounce attempts: {} of {}",
        item.attempts, item.budget
    ));
    doc.bullet(format!(
        "Route path: {}",
        code(&route_text(&item.route_path)?)
    ));
    doc.bullet(format!(
        "Authorization: {}",
        authorization_text(&evaluation.authorization)
    ));
    doc.bullet(format!("Outcome: {}", outcome_text(&evaluation.outcome)));
    if let Outcome::Changed { differences } = &evaluation.outcome {
        for difference in differences {
            doc.bullet(format!("Difference: {}", difference_text(difference)));
        }
    }
    if evaluation.obligations.is_empty() {
        doc.bullet("Obligations: none apply");
    }
    for check in &evaluation.obligations {
        doc.bullet(format!(
            "Obligation {}: {}",
            code(check.obligation_id.as_str()),
            check_text(&check.result)
        ));
    }
    doc.para("**Evidence**");
    let evidence: Vec<Vec<String>> = item
        .evidence
        .iter()
        .map(evidence_row)
        .collect::<Result<_, _>>()?;
    doc.table(
        &[
            "Slot",
            "Step",
            "Field",
            "Value",
            "Provenance",
            "Run",
            "Commit",
            "Observation",
        ],
        &evidence,
    );
    doc.para("**Sequence**");
    let sequence: Vec<Vec<String>> = item
        .sequence
        .iter()
        .map(sequence_row)
        .collect::<Result<_, _>>()?;
    doc.table(
        &["Request", "Step", "Run", "Role", "Status", "Tint"],
        &sequence,
    );
    Ok(())
}

fn evidence_row(row: &EvidenceRow) -> Result<Vec<String>, RenderError> {
    Ok(vec![
        code(&wire(&row.slot)?),
        code(row.step.as_str()),
        code(row.field.as_str()),
        evidence_text(&row.value),
        code(&wire(&row.provenance)?),
        optional(row.run_id.as_ref().map(|id| id.as_str().to_owned())),
        optional(row.commit.as_ref().map(|sha| sha.as_str().to_owned())),
        optional(row.observation_hash.map(|hash| hash.to_string())),
    ])
}

fn sequence_row(step: &SequenceStep) -> Result<Vec<String>, RenderError> {
    Ok(vec![
        step.request.to_string(),
        code(step.step_id.as_str()),
        code(step.run_id.as_str()),
        code(&wire(&step.role)?),
        step.status
            .map_or_else(|| "none".to_owned(), |status| status.to_string()),
        code(&wire(&step.tint)?),
    ])
}

fn write_contests(doc: &mut Doc, receipt: &Receipt) -> Result<(), RenderError> {
    doc.heading(2, "Contests");
    doc.para(format!(
        "Contests: {}. A contest asks the human; it never widens the scope. The rationale is the agent's own text.",
        receipt.contests.len()
    ));
    if receipt.contests.is_empty() {
        doc.para("None.");
        return Ok(());
    }
    doc.block();
    for contest in &receipt.contests {
        doc.bullet(format!(
            "Contest {}: {} against episode {} by probe {}, {}. Rationale: {}",
            code(contest.id.as_str()),
            code(&wire(&contest.kind)?),
            code(&contest.target.to_string()),
            code(&contest.reproducer.to_string()),
            code(&wire(&contest.status)?),
            text(&contest.rationale)
        ));
    }
    doc.block();
    Ok(())
}

fn write_context(doc: &mut Doc, receipt: &Receipt) -> Result<(), RenderError> {
    let context = &receipt.prepared.context;
    doc.heading(2, "Tested context");
    doc.para("The gate compares every field below. Land is refused when any of them moved.");
    doc.bullet(format!(
        "Overlay: {}",
        code(&context.overlay_hash.to_string())
    ));
    doc.bullet(format!(
        "Ledger head: event {} at hash {}",
        context.ledger_head.event_seq,
        optional(context.ledger_head.hash.map(|hash| hash.to_string()))
    ));
    doc.bullet(format!(
        "Probe set: {}",
        code(&context.probe_set_hash.to_string())
    ));
    doc.bullet(format!(
        "Fixture set: {}",
        code(&context.fixture_set_hash.to_string())
    ));
    doc.bullet(format!(
        "Runner: {}",
        code(&context.runner_hash.to_string())
    ));
    doc.bullet(format!(
        "Evaluator: {}",
        code(&wire(&context.evaluator_version)?)
    ));
    doc.bullet(format!("Runtime: {}", code(&wire(&context.runtime)?)));
    doc.bullet(format!(
        "Controlled environment: {}",
        code(&context.controlled_env_hash.to_string())
    ));
    doc.bullet(format!(
        "Container policy: {}",
        code(&context.sandbox_policy_hash.to_string())
    ));
    for (path, hash) in &context.effective_artifact_hashes {
        doc.bullet(format!(
            "Artifact {}: {}",
            code(path.as_str()),
            code(&hash.to_string())
        ));
    }
    doc.block();
    Ok(())
}
