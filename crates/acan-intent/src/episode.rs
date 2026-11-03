//! Discrepancy episodes, contests, and readback predictions.
//!
//! An episode is one failure the repair loop is working on. Its identity, the [`EpisodeId`], is
//! the hash of its [`EpisodeKey`]: the order, the difference's selector, and the obligation. The
//! key never includes a probe hash, a step id, a commit, a worker or a run, so a new probe
//! spelling, a new commit or a new worker continues the same episode. The repair budget is the
//! order's, not the episode's: see [`crate::RepairRound`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::hash::{
    CanonicalError, ContentHash, EpisodeId, EvidenceHash, ObservationHash, ProbeHash, ScopeHash,
};
use crate::scope::{Change, Principal, Selector, StepContext};
use crate::types::{
    AgentRef, Channel, CommitSha, ContestId, Evidence, ObligationId, ObservationPath, OrderRef,
    Scalar, ScopeRev, StepId, TaskRef, UnixMillis,
};
use crate::verdict::ItemClass;
use crate::version::{ContestV1, EpisodeV1};

/// The stable identity of a discrepancy.
///
/// The core builds `selector` from the difference, not from the probe: the request's surface,
/// method and route template, the channel, and the field with every array index written `[*]`.
/// `step_context` is always `any`, and an actor principal is widened to `authenticated`.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct EpisodeKey {
    /// The order.
    pub order: OrderRef,
    /// Where the difference is.
    pub selector: Selector,
    /// The failing obligation, or none for a difference no item allows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation_id: Option<ObligationId>,
}

impl EpisodeKey {
    /// The episode identity.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn episode_id(&self) -> Result<EpisodeId, CanonicalError> {
        ContentHash::of(self).map(EpisodeId::from)
    }
}

/// The selector an episode is keyed by: the authored selector with `step_context` set to `any`
/// and an actor principal widened to `authenticated`, as [`EpisodeKey`] documents.
#[must_use]
pub fn canonical_episode_selector(selector: &Selector) -> Selector {
    let mut canonical = selector.clone();
    canonical.step_context = StepContext::Any {};
    if matches!(canonical.principal, Principal::Actor { .. }) {
        canonical.principal = Principal::Authenticated {};
    }
    canonical
}

/// The selector an out-of-scope episode is keyed by: the canonical selector with its field
/// dropped for the body, header and cookie channels. Every changed field of one channel on one
/// route is then one finding, so eight changed fields are one episode. A side-effect path is
/// kept, because the decoder requires it.
#[must_use]
pub fn canonical_out_of_scope_selector(selector: &Selector) -> Selector {
    let mut canonical = canonical_episode_selector(selector);
    if canonical.channel != Channel::SideEffect {
        canonical.field_path = None;
    }
    canonical
}

/// One scope revision an episode lived through.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ScopeRevision {
    /// The revision.
    pub rev: ScopeRev,
    /// Its hash.
    pub scope_hash: ScopeHash,
}

/// The evidence one gate run produced for an episode.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRevision {
    /// 1 for the run that opened the episode, then one more per run.
    pub revision: u32,
    /// The tested merged commit.
    pub merged_commit: CommitSha,
    /// The probe that reproduced it.
    pub probe_hash: ProbeHash,
    /// The step.
    pub step_id: StepId,
    /// The candidate observation.
    pub observation_hash: ObservationHash,
    /// The raw evidence.
    pub evidence_hash: EvidenceHash,
    /// The class the core assigned.
    pub class: ItemClass,
}

/// One candidate an agent offered for an episode.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CandidateAttempt {
    /// 1 for the first build, then one more per repair.
    pub attempt: u32,
    /// The prepared merged commit.
    pub merged_commit: CommitSha,
    /// The agent.
    pub agent: AgentRef,
    /// The task.
    pub task: TaskRef,
    /// When it was reported built.
    pub at: UnixMillis,
}

/// Where an episode stands.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeState {
    /// Found, not yet sent back.
    Open,
    /// Sent back to the agent with a reproducer.
    Bounced,
    /// The original obligation passed on a fresh run.
    Fixed,
    /// Waiting for a human ruling.
    Residue,
    /// An agent contested it. Waiting for the human. Never fixed, never a pass.
    Contested,
    /// A human ruling settled it.
    Ruled,
    /// The repair budget ran out.
    Stopped,
}

/// One failure the repair loop tracks across commits, workers and probe spellings.
///
/// Missing evidence, a new mask, a contest, or a deferral never makes it `fixed`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscrepancyEpisode {
    /// Always `acan-episode/1`.
    pub version: EpisodeV1,
    /// The hash of [`DiscrepancyEpisode::key`].
    pub id: EpisodeId,
    /// The order.
    pub order: OrderRef,
    /// The scope revisions it lived through, oldest first.
    pub scope_lineage: Vec<ScopeRevision>,
    /// The probe that reproduces it now. Not part of its identity.
    pub probe_hash: ProbeHash,
    /// The step that reproduces it now. Not part of its identity.
    pub step_id: StepId,
    /// Where the difference is.
    pub selector: Selector,
    /// The failing obligation, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation_id: Option<ObligationId>,
    /// Every gate run's evidence, oldest first.
    pub evidence_revisions: Vec<EvidenceRevision>,
    /// Every candidate offered, oldest first.
    pub candidate_attempts: Vec<CandidateAttempt>,
    /// How many of the order's repair rounds included it. Subordinate to the order's round
    /// count: it never exceeds the rounds the order has opened, and never adds one.
    pub bounce_count: u32,
    /// Where it stands.
    pub state: EpisodeState,
}

impl DiscrepancyEpisode {
    /// The key its identity is the hash of.
    #[must_use]
    pub fn key(&self) -> EpisodeKey {
        EpisodeKey {
            order: self.order.clone(),
            selector: self.selector.clone(),
            obligation_id: self.obligation_id.clone(),
        }
    }
}

/// What an agent claims in a contest.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContestKind {
    /// The scope blocks work the order needs.
    Blocked,
    /// Two obligations or rulings conflict.
    SpecConflict,
    /// The agent proposes a different behavior.
    ProposedChange,
}

/// Where a contest stands. Only a human moves it out of `open`.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ContestStatus {
    /// Waiting for the human.
    Open,
    /// The human agreed and ruled or revised the scope.
    Upheld,
    /// The human disagreed.
    Rejected,
}

/// An agent's objection to one episode, with a reproducer. A contest asks the human; it never
/// widens the scope and never counts as fixed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Contest {
    /// Always `acan-contest/1`.
    pub version: ContestV1,
    /// The contest.
    pub id: ContestId,
    /// The order.
    pub order: OrderRef,
    /// The contesting agent.
    pub agent: AgentRef,
    /// Its task.
    pub task: TaskRef,
    /// The contested episode.
    pub target: EpisodeId,
    /// What it claims.
    pub kind: ContestKind,
    /// A probe that shows the problem.
    pub reproducer: ProbeHash,
    /// The agent's reason. Model text, shown labelled.
    pub rationale: String,
    /// Where it stands.
    pub status: ContestStatus,
    /// When it was filed.
    pub filed_at: UnixMillis,
}

/// What an agent predicts its change does at one selector.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicted {
    /// No observable difference.
    Unchanged {},
    /// A difference of this kind.
    Change {
        /// The kind.
        change: Change,
    },
    /// This value at this field.
    Value {
        /// The step.
        step_id: StepId,
        /// The field.
        path: ObservationPath,
        /// The value with its structure, or [`Evidence::Missing`] for a field that will not exist.
        value: Evidence,
    },
}

/// One prediction in a readback. The core scores it against the observations; a wrong
/// prediction is a finding, and a right one is never a pass.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Prediction {
    /// Where.
    pub selector: Selector,
    /// What.
    pub predicted: Predicted,
}

/// A value a model suggests, such as a probe input. A human must adopt it before it is used.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProposedValue {
    /// The value.
    pub value: Scalar,
    /// Why, in one sentence.
    pub rationale: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn selector(channel: &str, field: Option<&str>) -> Selector {
        let mut value = json!({
            "surface": "http",
            "method": "POST",
            "route_template": "/api/links",
            "step_context": {"kind": "step", "step_id": "s6"},
            "principal": {"kind": "actor", "name": "alice"},
            "channel": channel,
        });
        if let Some(field) = field {
            value["field_path"] = json!(field);
        }
        serde_json::from_value(value).expect("the selector decodes")
    }

    #[test]
    fn a_body_field_collapses_to_its_channel() {
        let collapsed = canonical_out_of_scope_selector(&selector("body", Some("clicks")));
        assert_eq!(
            collapsed,
            canonical_out_of_scope_selector(&selector("body", Some("error")))
        );
        assert_eq!(collapsed.field_path, None);
        assert_eq!(collapsed.channel, Channel::Body);
        assert_eq!(collapsed.step_context, StepContext::Any {});
        assert_eq!(collapsed.principal, Principal::Authenticated {});
    }

    #[test]
    fn a_side_effect_path_is_kept_because_the_decoder_requires_it() {
        let path = "store.audit[*].route";
        let collapsed = canonical_out_of_scope_selector(&selector("side_effect", Some(path)));
        assert_eq!(
            collapsed.field_path.as_ref().map(|field| field.as_str()),
            Some(path)
        );
    }
}
