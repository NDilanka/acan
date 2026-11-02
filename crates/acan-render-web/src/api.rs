//! Typed decoders for the scope-first reads the page makes.
//!
//! Each decoder names the type the origin encodes, so a changed field fails here and not on the page.
//! The receipt, the scope, the rulings and the confirmation body are contract types in `acan-intent`.
//! The scope draft and the ruling draft are answered by two routes whose response types the origin
//! keeps private, so their shapes are mirrored here. The usage rows come from `acan-llm`, which
//! derives only `Serialize`, so they are mirrored too.

use acan_intent::{
    ContrastExample, ContrastExampleHash, GetReceiptResponse, GetRulingsResponse, GetScopeResponse,
    LedgerHead, NormalizationHash, NormalizationVersion, ProbeHash, ScopeConfirmation, ScopeDraft,
    ScopeHash, Selector,
};
use serde::Deserialize;

/// `GET /api/tasks/{task}/receipt`: the latest receipt of a task's gate runs.
///
/// # Errors
///
/// Returns the JSON error when the text is not that response.
pub fn receipt(json: &str) -> Result<GetReceiptResponse, serde_json::Error> {
    serde_json::from_str(json)
}

/// `GET /api/tasks/{task}/scope`: the confirmed scope of a task's order.
///
/// # Errors
///
/// Returns the JSON error when the text is not that response.
pub fn scope(json: &str) -> Result<GetScopeResponse, serde_json::Error> {
    serde_json::from_str(json)
}

/// `GET /api/tasks/{task}/rulings`: the rulings in force on the named machines.
///
/// # Errors
///
/// Returns the JSON error when the text is not that response.
pub fn rulings(json: &str) -> Result<GetRulingsResponse, serde_json::Error> {
    serde_json::from_str(json)
}

/// `GET /api/orders/{order}/scope/confirmation`: the body a member sends as `confirm_scope`. The
/// member edits the budget and the answers in it, then posts it.
///
/// # Errors
///
/// Returns the JSON error when the text is not that body.
pub fn confirmation(json: &str) -> Result<ScopeConfirmation, serde_json::Error> {
    serde_json::from_str(json)
}

/// What the scope-draft job reported for one order revision. Mirrors the origin's outcome, which
/// is tagged by `state`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ScopeDraftOutcome {
    /// The foreman's draft. A human adopts it by revising and then confirming the scope.
    Drafted {
        /// The draft, with the model that proposed it.
        draft: Box<ScopeDraft>,
    },
    /// The last attempt failed. Nothing was drafted, and nothing falls back to another draft.
    Failed {
        /// Why the draft failed.
        reason: String,
    },
}

/// `GET /api/orders/{order}/scope-draft`: the latest draft of an order, with the revision it
/// proposes.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ScopeDraftView {
    /// The order.
    pub order: String,
    /// The revision the draft is for.
    pub rev: u32,
    /// Drafted or failed.
    #[serde(flatten)]
    pub outcome: ScopeDraftOutcome,
}

/// Decodes `GET /api/orders/{order}/scope-draft`.
///
/// # Errors
///
/// Returns the JSON error when the text is not that response.
pub fn scope_draft(json: &str) -> Result<ScopeDraftView, serde_json::Error> {
    serde_json::from_str(json)
}

/// One example kept for a discrepancy episode, with the normalization it was compared under.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DraftExample {
    /// The candidate run that produced it.
    pub run: String,
    /// The example's hash.
    pub hash: ContrastExampleHash,
    /// The probe it was taken on.
    pub probe_hash: ProbeHash,
    /// The normalization version it was compared under.
    pub normalization_version: NormalizationVersion,
    /// The normalization hash it was compared under.
    pub normalization_hash: NormalizationHash,
    /// The example itself.
    pub example: ContrastExample,
}

/// `GET /api/orders/{order}/episodes/{episode}/ruling-draft`: what a member needs to rule on one
/// discrepancy episode. The draft confirms nothing.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RulingDraftView {
    /// The order.
    pub order: String,
    /// The episode.
    pub episode: String,
    /// The scope the ruling is made under.
    pub scope_hash: ScopeHash,
    /// The ledger head the ruling must chain from.
    pub ledger_head: LedgerHead,
    /// The episode's selector.
    pub selector: Selector,
    /// The probe of the example the ruling is on.
    pub probe_hash: ProbeHash,
    /// The example the ruling is on: the episode's latest candidate run, for its probe.
    pub ruled_on: ContrastExampleHash,
    /// The normalization version the ruling is on.
    pub normalization_version: NormalizationVersion,
    /// The normalization hash the ruling is on.
    pub normalization_hash: NormalizationHash,
    /// Every example kept for the episode.
    pub examples: Vec<DraftExample>,
}

/// Decodes `GET /api/orders/{order}/episodes/{episode}/ruling-draft`.
///
/// # Errors
///
/// Returns the JSON error when the text is not that response.
pub fn ruling_draft(json: &str) -> Result<RulingDraftView, serde_json::Error> {
    serde_json::from_str(json)
}

/// One row of `GET /api/usage`: the calls one role made to one provider's model, and their tokens.
/// The latency the origin also sends is not read here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UsageRow {
    /// The role the calls served, such as `split` or `explain`.
    pub role: String,
    /// The provider that answered, such as `Meta Model API` or `Baseten`.
    pub provider: String,
    /// The model that answered.
    pub model: String,
    /// How many calls there were.
    pub calls: u64,
    /// Prompt tokens across the calls.
    pub prompt_tokens: u64,
    /// Completion tokens across the calls.
    pub completion_tokens: u64,
}

/// Decodes `GET /api/usage`.
///
/// # Errors
///
/// Returns the JSON error when the text is not a list of usage rows.
pub fn usage(json: &str) -> Result<Vec<UsageRow>, serde_json::Error> {
    serde_json::from_str(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    const RECEIPT: &str = include_str!("../../acan-intent/fixtures/mcp-get-receipt-response.json");
    const SCOPE: &str = include_str!("../../acan-intent/fixtures/mcp-get-scope-response.json");
    const RULINGS: &str = include_str!("../../acan-intent/fixtures/mcp-get-rulings-response.json");
    const CONFIRMATION: &str = include_str!("../../acan-intent/fixtures/scope-confirmation.json");
    const DRAFT: &str = include_str!("../../acan-intent/fixtures/scope-draft.json");
    const EXAMPLE: &str = include_str!("../../acan-intent/fixtures/contrast-example.json");

    const HASH: &str = "sha256:4cdb7f1860ddb4f8100e605cabe7be37977a6b216eb814fb813d0e81c9874ddd";

    #[test]
    fn the_contract_responses_decode() {
        receipt(RECEIPT).expect("the receipt decodes");
        scope(SCOPE).expect("the scope decodes");
        rulings(RULINGS).expect("the rulings decode");
        confirmation(CONFIRMATION).expect("the confirmation decodes");
    }

    #[test]
    fn a_drafted_outcome_carries_the_draft() {
        let draft: Value = serde_json::from_str(DRAFT).expect("the draft fixture is JSON");
        let text = json!({ "order": "o-3", "rev": 2, "state": "drafted", "draft": draft });
        let view = scope_draft(&text.to_string()).expect("a drafted view decodes");
        assert_eq!(view.rev, 2);
        assert!(matches!(view.outcome, ScopeDraftOutcome::Drafted { .. }));
    }

    #[test]
    fn a_failed_outcome_carries_the_reason() {
        let text = json!({ "order": "o-3", "rev": 2, "state": "failed", "reason": "empty reply" });
        let view = scope_draft(&text.to_string()).expect("a failed view decodes");
        assert_eq!(
            view.outcome,
            ScopeDraftOutcome::Failed {
                reason: "empty reply".to_owned()
            }
        );
    }

    #[test]
    fn an_unknown_outcome_state_is_refused() {
        let text = json!({ "order": "o-3", "rev": 2, "state": "maybe" });
        assert!(scope_draft(&text.to_string()).is_err());
    }

    #[test]
    fn a_ruling_draft_with_no_examples_decodes() {
        let scope_value: Value = serde_json::from_str(SCOPE).expect("the scope fixture is JSON");
        let selector = scope_value["scope"]["body"]["may"][0]["selector"].clone();
        let text = json!({
            "order": "o-3",
            "episode": HASH,
            "scope_hash": HASH,
            "ledger_head": { "event_seq": 2, "hash": HASH },
            "selector": selector,
            "probe_hash": HASH,
            "ruled_on": HASH,
            "normalization_version": "acan-normalization/1",
            "normalization_hash": HASH,
            "examples": [],
        });
        let view = ruling_draft(&text.to_string()).expect("the draft decodes");
        assert!(view.examples.is_empty());
    }

    #[test]
    fn a_ruling_draft_example_decodes_as_a_contrast_example() {
        let scope_value: Value = serde_json::from_str(SCOPE).expect("the scope fixture is JSON");
        let selector = scope_value["scope"]["body"]["may"][0]["selector"].clone();
        let example: Value = serde_json::from_str(EXAMPLE).expect("the example fixture is JSON");
        let text = json!({
            "order": "o-3",
            "episode": HASH,
            "scope_hash": HASH,
            "ledger_head": { "event_seq": 2, "hash": HASH },
            "selector": selector,
            "probe_hash": HASH,
            "ruled_on": HASH,
            "normalization_version": "acan-normalization/1",
            "normalization_hash": HASH,
            "examples": [{
                "run": "r-1",
                "hash": HASH,
                "probe_hash": HASH,
                "normalization_version": "acan-normalization/1",
                "normalization_hash": HASH,
                "example": example,
            }],
        });
        let view = ruling_draft(&text.to_string()).expect("the draft decodes");
        assert_eq!(view.examples.len(), 1);
        assert_eq!(view.examples[0].run, "r-1");
    }

    /// The unit/d02 samples. Each draft and example body is a unit/d02 fixture, and each wrapper
    /// has the keys of the handler's response struct in unit/d02 `http.rs`. The hashes in the
    /// ruling draft are placeholders of the right shape; the live check is the server's e2e test.
    const SCOPE_DRAFT_DRAFTED: &str = include_str!("../fixtures/d02/scope-draft-drafted.json");
    const SCOPE_DRAFT_FAILED: &str = include_str!("../fixtures/d02/scope-draft-failed.json");
    const RULING_DRAFT: &str = include_str!("../fixtures/d02/ruling-draft.json");

    #[test]
    fn the_d02_scope_draft_samples_decode_into_the_mirror() {
        let drafted = scope_draft(SCOPE_DRAFT_DRAFTED).expect("the drafted sample decodes");
        assert_eq!(drafted.order, "o-3");
        assert_eq!(drafted.rev, 2);
        assert!(matches!(drafted.outcome, ScopeDraftOutcome::Drafted { .. }));

        let failed = scope_draft(SCOPE_DRAFT_FAILED).expect("the failed sample decodes");
        assert_eq!(
            failed.outcome,
            ScopeDraftOutcome::Failed {
                reason: "the reply did not parse as a scope draft".to_owned()
            }
        );
    }

    #[test]
    fn the_d02_ruling_draft_sample_decodes_into_the_mirror() {
        let view = ruling_draft(RULING_DRAFT).expect("the ruling draft sample decodes");
        assert_eq!(view.order, "o-3");
        assert_eq!(view.examples.len(), 1);
        assert_eq!(view.examples[0].run, "r-1");
        assert_eq!(
            view.examples[0].normalization_version.to_string(),
            "acan-normalization/1"
        );
    }

    #[test]
    fn usage_rows_decode_and_ignore_latency() {
        let text = r#"[{
            "role": "split",
            "provider": "Meta Model API",
            "model": "muse-spark-1.3",
            "calls": 3,
            "prompt_tokens": 6604,
            "completion_tokens": 5277,
            "latency": { "secs": 12, "nanos": 0 }
        }]"#;
        let rows = usage(text).expect("the usage rows decode");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provider, "Meta Model API");
        assert_eq!(rows[0].prompt_tokens, 6604);
    }
}
