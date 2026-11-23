//! Who authored a value: an observation, the core, a human, or a model.
//!
//! Decoded data and admitted authority are different types. Raw storage and wire records
//! ([`crate::Observation`], [`crate::Run`], [`crate::Mask`], [`crate::Receipt`] and the rest)
//! decode freely and carry no authority. Evidence and core results are [`Observed`],
//! [`crate::ObservedRun`] and [`CoreDerived`]: their fields are private, they have no public
//! constructor and no `Deserialize`, and only [`crate::admit`] and this crate's pure functions
//! make them. Gate and evaluator APIs take only those.
//!
//! A model may propose scope drafts, probe inputs, predictions, predicates and tours, and
//! nothing else: [`ModelProposal`] only wraps a [`Proposable`] payload, and that trait is sealed.
//! Model-facing surfaces decode proposal types only.
//!
//! ```compile_fail,E0277
//! # use acan_intent::{ModelProposal, Observation};
//! // An observation cannot even be wrapped as a model proposal.
//! fn launder(_: ModelProposal<Observation>) {}
//! ```
//!
//! ```compile_fail,E0277
//! # use acan_intent::{ItemClass, ModelProposal};
//! fn launder(_: ModelProposal<ItemClass>) {}
//! ```
//!
//! ```compile_fail,E0277
//! # use acan_intent::{ClassCounts, ModelProposal};
//! fn launder(_: ModelProposal<ClassCounts>) {}
//! ```
//!
//! ```compile_fail,E0277
//! # use acan_intent::{Mask, ModelProposal};
//! fn launder(_: ModelProposal<Mask>) {}
//! ```
//!
//! ```compile_fail,E0277
//! # use acan_intent::{ModelProposal, Observation, Prediction};
//! // No conversion from a proposal to an observation exists.
//! fn launder(p: ModelProposal<Prediction>) -> Observation { p.into() }
//! ```
//!
//! ```compile_fail,E0599
//! # use acan_intent::{ModelProposal, Prediction};
//! // A proposal is read by reference only; it has no by-value accessor.
//! fn launder(p: ModelProposal<Prediction>) -> Prediction { p.into_inner() }
//! ```
//!
//! ```compile_fail
//! # use acan_intent::ClassCounts;
//! // Core-derived counts cannot be built outside the core.
//! let counts = ClassCounts { preserved: 3 };
//! ```
//!
//! ```compile_fail,E0277
//! # use acan_intent::Observed;
//! // Decoding JSON never yields evidence.
//! let forged: Observed = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail,E0277
//! # use acan_intent::ObservedRun;
//! // Nor a whole admitted run.
//! let forged: ObservedRun = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail,E0277
//! # use acan_intent::{CoreDerived, Mask};
//! // Nor a core-derived mask.
//! let forged: CoreDerived<Mask> = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail,E0277
//! # use acan_intent::{HumanRef, HumanSpec, Scope};
//! // Nor a human spec: a human value is built only after the human is authenticated.
//! let forged: HumanSpec<Scope> = serde_json::from_str("{}").unwrap();
//! ```
//!
//! ```compile_fail,E0624
//! # use acan_intent::{Evidence, EvidenceAddress, ModelProposal, Observed, ProposedValue};
//! // A proposed scalar cannot be relabelled as observed.
//! fn launder(p: &ModelProposal<ProposedValue>, at: EvidenceAddress) -> Observed {
//!     let value = serde_json::to_value(&p.proposal().value).unwrap();
//!     Observed::new(Evidence::Present { value }, at)
//! }
//! ```
//!
//! ```compile_fail,E0624
//! # use acan_intent::{ObservedRun, Run};
//! // A run record cannot be admitted without the admission check.
//! fn launder(run: Run) -> ObservedRun { ObservedRun::admitted(run, Vec::new()) }
//! ```
//!
//! ```compile_fail,E0624
//! # use acan_intent::{CoreDerived, Mask};
//! // A decoded mask cannot be promoted to a core-derived one.
//! fn launder(mask: Mask) -> CoreDerived<Mask> { CoreDerived::derived(mask) }
//! ```
//!
//! ```compile_fail,E0308
//! # use acan_intent::compare::{self, Contrast};
//! # use acan_intent::Mask;
//! // Comparison takes core-derived masks only, never decoded ones.
//! fn launder(contrast: &Contrast<'_>, masks: &[Mask]) {
//!     let _ = compare::compare(contrast, masks);
//! }
//! ```
//!
//! Reading a proposal, and passing core-derived masks to comparison, is fine:
//!
//! ```
//! # use acan_intent::compare::{self, Contrast};
//! # use acan_intent::{CoreDerived, Mask, ModelProposal, Prediction};
//! fn read(p: &ModelProposal<Prediction>) -> &Prediction { p.proposal() }
//! fn compare_with(contrast: &Contrast<'_>, masks: &[CoreDerived<Mask>]) {
//!     let _ = compare::compare(contrast, masks);
//! }
//! ```

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::hash::ObservationHash;
use crate::types::{
    CommitSha, Evidence, HumanRef, ModelRef, ObservationPath, RunId, StepId, UnixMillis,
};
use crate::version::literal_tag;

literal_tag!(
    /// The `provenance` field of an [`Observed`] value.
    ObservedTag = "observed",
    "provenance"
);
literal_tag!(
    /// The `provenance` field of a [`CoreDerived`] value.
    CoreDerivedTag = "core_derived",
    "provenance"
);
literal_tag!(
    /// The `provenance` field of a [`HumanSpec`] value.
    HumanSpecTag = "human_spec",
    "provenance"
);
literal_tag!(
    /// The `provenance` field of a [`ModelProposal`].
    ModelProposalTag = "model_proposal",
    "provenance"
);

/// The provenance label the plant and the card draw next to a value.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Recorded by the runner from one run, commit, step and field.
    Observed,
    /// Computed by the core from observations, masks, scope and expectations.
    CoreDerived,
    /// Written or confirmed by a human.
    HumanSpec,
    /// Proposed by a model. Never evidence.
    ModelProposal,
}

mod sealed {
    pub trait Sealed {}
}

/// A payload a model may propose. Sealed: implemented only for scope drafts, probe inputs,
/// predictions, predicate proposals, proposed values and tours.
pub trait Proposable: sealed::Sealed {}

macro_rules! proposable {
    ($($ty:path),+ $(,)?) => {$(
        impl sealed::Sealed for $ty {}
        impl Proposable for $ty {}
    )+};
}

proposable!(
    crate::ScopeBody,
    crate::ProbeProposal,
    crate::Prediction,
    crate::PredicateProposal,
    crate::ProposedValue,
    crate::Tour,
);

/// A value a model proposed, labelled with the model that proposed it.
///
/// It gains authority only through a human: [`HumanSpec::adopt`].
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, bound(deserialize = "T: Deserialize<'de>"))]
pub struct ModelProposal<T: Proposable> {
    provenance: ModelProposalTag,
    model: ModelRef,
    proposal: T,
}

impl<T: Proposable> ModelProposal<T> {
    /// Labels `proposal` as proposed by `model`.
    pub fn new(proposal: T, model: ModelRef) -> Self {
        Self {
            provenance: ModelProposalTag,
            model,
            proposal,
        }
    }

    /// The proposed payload.
    pub fn proposal(&self) -> &T {
        &self.proposal
    }

    /// The model that proposed it.
    pub fn model(&self) -> &ModelRef {
        &self.model
    }
}

/// A value a human wrote or confirmed.
///
/// It serializes for display and storage but never decodes: trusted server code builds one
/// only after authenticating the human member, so no request or model reply can carry one in.
#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct HumanSpec<T> {
    provenance: HumanSpecTag,
    by: HumanRef,
    at: UnixMillis,
    value: T,
}

impl<T> HumanSpec<T> {
    /// A value the authenticated human `by` wrote at `at`.
    pub fn new(value: T, by: HumanRef, at: UnixMillis) -> Self {
        Self {
            provenance: HumanSpecTag,
            by,
            at,
            value,
        }
    }

    /// The human confirmation of a model proposal. The proposal's payload becomes the human's.
    pub fn adopt(proposal: ModelProposal<T>, by: HumanRef, at: UnixMillis) -> Self
    where
        T: Proposable,
    {
        Self::new(proposal.proposal, by, at)
    }

    /// The value.
    pub fn value(&self) -> &T {
        &self.value
    }

    /// Who wrote or confirmed it.
    pub fn by(&self) -> &HumanRef {
        &self.by
    }

    /// When.
    pub fn at(&self) -> UnixMillis {
        self.at
    }
}

/// A value computed by the core. Only this crate's pure functions construct one, and it never
/// decodes: a stored or transmitted copy of the inner value is plain data.
#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct CoreDerived<T> {
    provenance: CoreDerivedTag,
    value: T,
}

impl<T> CoreDerived<T> {
    pub(crate) fn derived(value: T) -> Self {
        Self {
            provenance: CoreDerivedTag,
            value,
        }
    }

    /// The value.
    pub fn value(&self) -> &T {
        &self.value
    }
}

/// One observed value, with its structure or its absence, and the address it was read from.
///
/// Only [`crate::ObservedRun::observed`] makes one, from an admitted run. It serializes for the
/// card and never decodes.
#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct Observed {
    provenance: ObservedTag,
    value: Evidence,
    run_id: RunId,
    commit: CommitSha,
    step_id: StepId,
    path: ObservationPath,
    observation_hash: ObservationHash,
}

/// Where an observed value was recorded: the run, the commit it tested, the step, the field,
/// and the stored observation it was read from.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EvidenceAddress {
    /// The run.
    pub run_id: RunId,
    /// The commit the run tested.
    pub commit: CommitSha,
    /// The step that produced it.
    pub step_id: StepId,
    /// The field.
    pub path: ObservationPath,
    /// The stored observation.
    pub observation_hash: ObservationHash,
}

impl Observed {
    pub(crate) fn new(value: Evidence, address: EvidenceAddress) -> Self {
        Self {
            provenance: ObservedTag,
            value,
            run_id: address.run_id,
            commit: address.commit,
            step_id: address.step_id,
            path: address.path,
            observation_hash: address.observation_hash,
        }
    }

    /// The value, or [`Evidence::Missing`].
    #[must_use]
    pub fn value(&self) -> &Evidence {
        &self.value
    }

    /// Where it was recorded.
    #[must_use]
    pub fn address(&self) -> EvidenceAddress {
        EvidenceAddress {
            run_id: self.run_id.clone(),
            commit: self.commit.clone(),
            step_id: self.step_id.clone(),
            path: self.path.clone(),
            observation_hash: self.observation_hash,
        }
    }
}
