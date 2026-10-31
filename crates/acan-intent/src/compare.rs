//! Run-wide normalization, base-only masks, and base-versus-candidate comparison.
//!
//! Every input is admitted ([`ObservedRun`]) and every output is [`CoreDerived`]; a decoded
//! observation, mask or comparison never reaches these functions.
//!
//! [`check_contrast`] comes first: each run must hold the role its contrast gives it, base A and
//! base B must share a commit and lockfile, and the three runs must share a runtime and a runner.
//! A contrast that fails this is refused with an error, never judged.
//!
//! How a comparison decides, in order:
//! - A run that did not complete, a run that recorded a missing capture, an incomplete
//!   observation, or runs whose step sequences differ is [`Outcome::Unknown`]. Nothing is compared
//!   past them.
//! - Each relation of the plan is a constraint on every run, judged on the original values. A
//!   relation that breaks on base A or base B has no valid baseline and is unknown. A relation
//!   that holds on both bases and breaks on the candidate is a difference, at the fields that
//!   break it. A relation that holds on one base and not the other is an unstable base.
//! - Base A and base B must agree on every value outside a mask, and on the type and format of
//!   every captured identity they both read. A disagreement outside a mask is an unstable base,
//!   not a difference.
//! - Candidate against the bases, on canonical values. A value under a mask is not compared, but
//!   its format must match the format of base A or base B, so a masked field keeps its format.
//!   A field the plan declares maskable masks the candidate even when the bases agree on it, as
//!   long as the candidate's value keeps the base's format; absent in a base, or in another
//!   format, it stays a difference. A captured identity keeps its type and alphabet too.
//!   Structure, presence and status are never masked, and neither is an opaque body (a response
//!   that is not JSON), which compares by its content hash.
//!
//! Canonical values replace a captured identity with `<capture:BINDING>` and a redacted secret
//! with `<redacted:LENGTH:CLASS:DIGEST>`. Literal text escapes `\` and `<`, so application text
//! can never spell a placeholder. A difference shows canonical values, so a session token is
//! never printed.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use serde_json::{Map, Value};

use crate::expr::StringFormat;
use crate::hash::{CanonicalError, ContentHash, ProbeHash};
use crate::observation::{
    CanonicalObservation, CanonicalRun, Channels, Completeness, IncompleteReason, Mask,
    MaskSelector, NormalizationPlan, OPAQUE_BODY_KEY, Observation, ObservedRun, ProjectionKind,
    Relation, StepPath,
};
use crate::probe::{
    CaptureKind, CaptureOutcome, CaptureSource, CaptureStep, CorpusProbe, Fixture, RunOutcome,
    RunRole, Step,
};
use crate::provenance::CoreDerived;
use crate::types::{
    CaptureName, Evidence, MaskablePath, ObservationPath, PathPattern, StepId, TrustedSink,
};
use crate::verdict::{Comparison, Difference, Outcome, UnknownReason};

/// Why a comparison could not be made. Never a pass.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CompareError {
    /// The run belongs to another probe or fixture than the one it is compared under.
    #[error("the run is not of this probe and fixture: {detail}")]
    WrongSpec {
        /// What does not match.
        detail: String,
    },
    /// A captured value cannot be read back from its source, or does not hash to the
    /// fingerprint the run recorded.
    #[error("the captured value of {binding} does not match its recorded fingerprint")]
    CaptureUnreadable {
        /// The binding.
        binding: CaptureName,
    },
    /// A value has no canonical JSON form, so it has no hash.
    #[error("a value has no canonical JSON form: {detail}")]
    Unhashable {
        /// The serializer's message.
        detail: String,
    },
    /// A path cannot be written as an observation path.
    #[error("the path {path} cannot be addressed")]
    Unaddressable {
        /// The path.
        path: String,
    },
    /// A mask does not belong to this contrast, or names a field the plan does not allow masking.
    #[error("a mask does not belong to this contrast: {detail}")]
    ForeignMask {
        /// What does not match.
        detail: String,
    },
    /// A run is not in the role its contrast gives it.
    #[error("a run is not in the role its contrast gives it: {detail}")]
    WrongRole {
        /// What does not match.
        detail: String,
    },
    /// The runs cannot be compared with each other: base A and base B differ in commit or
    /// lockfile, or the three runs differ in runtime or runner.
    #[error("the runs cannot be compared with each other: {detail}")]
    Incompatible {
        /// What does not match.
        detail: String,
    },
    /// A comparison was not made from these three runs under this normalization plan.
    #[error("the comparison was not made from these runs: {detail}")]
    ForeignComparison {
        /// What does not match.
        detail: String,
    },
}

impl From<CanonicalError> for CompareError {
    fn from(err: CanonicalError) -> Self {
        Self::Unhashable {
            detail: err.to_string(),
        }
    }
}

/// One run with everything run-wide normalization reads: the probe and its plan, the fixture
/// whose recipe ran first, and the admitted run with every observation of its sequence and its
/// capture fingerprints.
#[derive(Debug, Clone, Copy)]
pub struct RunContext<'a> {
    /// The probe, with its normalization plan.
    pub probe: &'a CorpusProbe,
    /// The fixture whose recipe steps precede the probe's.
    pub fixture: &'a Fixture,
    /// The admitted run.
    pub run: &'a ObservedRun,
}

/// The three runs of one probe: base A, base B and the candidate.
#[derive(Debug, Clone, Copy)]
pub struct Contrast<'a> {
    /// The probe, with its normalization plan.
    pub probe: &'a CorpusProbe,
    /// The fixture all three runs started from.
    pub fixture: &'a Fixture,
    /// The first base run.
    pub base_a: &'a ObservedRun,
    /// The second base run.
    pub base_b: &'a ObservedRun,
    /// The candidate run.
    pub candidate: &'a ObservedRun,
}

/// Normalizes a whole run under its probe's plan.
///
/// Reads the sequence in order, recipe steps first. Every capture binding resolves from the
/// observation of its capture step's `from` step through its [`crate::CaptureSource`], and the
/// resolved value must hash to the run's [`crate::CaptureOutcome::Captured`] fingerprint, so a
/// later observation canonicalizes by what earlier steps bound in this run. Projections keep
/// format, ordering and cardinality; step-qualified relations are checked across steps. Pure:
/// no hidden state and no blob reads.
///
/// # Errors
///
/// Returns [`CompareError`] when the run cannot be normalized, such as a capture whose value
/// does not match its fingerprint.
pub fn canonicalize(context: &RunContext<'_>) -> Result<CoreDerived<CanonicalRun>, CompareError> {
    canonical_run(context).map(CoreDerived::derived)
}

fn canonical_run(context: &RunContext<'_>) -> Result<CanonicalRun, CompareError> {
    let plan = &context.probe.normalization;
    let captures = captured_values(context)?;
    let canon = Canon {
        plan,
        captures: &captures,
    };
    let observations = context
        .run
        .observations()
        .map(|(hash, observation)| {
            Ok(CanonicalObservation {
                observation_hash: *hash,
                step_id: observation.step_id.clone(),
                channels: canonical_channels(&observation.channels, &canon)?,
            })
        })
        .collect::<Result<Vec<_>, CompareError>>()?;
    Ok(CanonicalRun {
        run_id: context.run.run().id.clone(),
        normalization_hash: plan.normalization_hash()?,
        observations,
    })
}

fn context<'a>(contrast: &Contrast<'a>, run: &'a ObservedRun) -> RunContext<'a> {
    RunContext {
        probe: contrast.probe,
        fixture: contrast.fixture,
        run,
    }
}

/// Derives masks from the base runs, and from the candidate on the fields the plan declares
/// maskable.
///
/// A mask is a leaf value that differs between the two base runs and that the plan's maskable
/// paths allow masking. A declared maskable field on which the bases agree also masks the
/// candidate when its value differs from the base in the base's own format: the value is the
/// candidate's to choose, its shape is not. A field absent in a base run is never masked, and
/// neither are structure, presence and status. When the two base runs do not have the same
/// sequence, or either did not complete, there are no masks: the comparison reports the contrast
/// as unknown.
///
/// # Errors
///
/// Returns [`CompareError`] when the contrast is not comparable, or a run cannot be normalized.
pub fn derive_masks(contrast: &Contrast<'_>) -> Result<Vec<CoreDerived<Mask>>, CompareError> {
    check_contrast(contrast)?;
    let base_a = canonical_run(&context(contrast, contrast.base_a))?;
    let base_b = canonical_run(&context(contrast, contrast.base_b))?;
    let comparable = [contrast.base_a, contrast.base_b]
        .iter()
        .all(|run| matches!(run.run().outcome, RunOutcome::Completed {}))
        && shape_mismatch(&base_a, &base_b).is_none();
    if !comparable {
        return Ok(Vec::new());
    }
    let plan = &contrast.probe.normalization;
    let probe_hash = contrast.probe.spec.probe_hash()?;
    let plan_hash = plan.normalization_hash()?;
    // The candidate sits on the bases' steps only: a run that did not complete, or a step the
    // bases do not have, masks nothing.
    let candidate = if matches!(contrast.candidate.run().outcome, RunOutcome::Completed {}) {
        Some(canonical_run(&context(contrast, contrast.candidate))?)
    } else {
        None
    };
    let mut masks = Vec::new();
    for (a, b) in base_a.observations.iter().zip(&base_b.observations) {
        let mut mask = |path: &str| -> Result<(), CompareError> {
            masks.push(CoreDerived::derived(Mask {
                probe_hash,
                selector: MaskSelector {
                    step_id: a.step_id.clone(),
                    path: parse_path(path)?,
                },
                base_a_observation_hash: a.observation_hash,
                base_b_observation_hash: b.observation_hash,
                normalization_version: plan.version.clone(),
                normalization_hash: plan_hash,
            }));
            Ok(())
        };
        // A field the bases disagree on masks from them alone; the candidate pass leaves it alone.
        let mut base_masked = Vec::new();
        for found in step_differences(&a.channels, &b.channels) {
            if found.kind != Kind::Leaf || !maskable(plan, &found.path) || !scalar_pair(&found) {
                continue;
            }
            base_masked.push(found.path.clone());
            mask(&found.path)?;
        }
        let Some(candidate_step) = candidate.as_ref().and_then(|candidate| {
            candidate
                .observations
                .iter()
                .find(|observation| observation.step_id == a.step_id)
        }) else {
            continue;
        };
        for found in step_differences(&a.channels, &candidate_step.channels) {
            if found.kind != Kind::Leaf
                || base_masked.contains(&found.path)
                || !maskable(plan, &found.path)
                || !scalar_pair(&found)
            {
                continue;
            }
            let (Some(base), Some(changed)) = (&found.base, &found.candidate) else {
                continue;
            };
            if format_of(base) != format_of(changed) {
                continue;
            }
            mask(&found.path)?;
        }
    }
    Ok(masks)
}

/// Compares the candidate against base under `masks`.
///
/// # Errors
///
/// Returns [`CompareError`] when the contrast is not comparable, the runs cannot be normalized,
/// or a mask does not belong to this contrast: another probe, another normalization plan, a
/// field the plan does not allow masking, or base observations it was not derived from.
pub fn compare(
    contrast: &Contrast<'_>,
    masks: &[CoreDerived<Mask>],
) -> Result<CoreDerived<Comparison>, CompareError> {
    check_contrast(contrast)?;
    let probe_hash = contrast.probe.spec.probe_hash()?;
    let base_a = canonical_run(&context(contrast, contrast.base_a))?;
    let base_b = canonical_run(&context(contrast, contrast.base_b))?;
    let candidate = canonical_run(&context(contrast, contrast.candidate))?;
    let masked = masked_fields(contrast, masks, &base_a, &base_b)?;
    let outcome = judge(
        contrast,
        [&base_a, &base_b, &candidate],
        &masked,
        probe_hash,
    )?;
    Ok(CoreDerived::derived(Comparison {
        probe_hash,
        base_a,
        base_b,
        candidate,
        outcome,
    }))
}

/// Checks that the three runs of a contrast can be compared at all. Each run holds the role its
/// contrast gives it and belongs to the contrast's probe and fixture. Base A and base B share a
/// commit and a lockfile, and all three share a runtime and a runner. Compatible means equal, so
/// an environment difference is never read as a change in the candidate.
///
/// # Errors
///
/// Returns [`CompareError::WrongRole`] for a run in another role, [`CompareError::WrongSpec`]
/// for a run of another probe or fixture, and [`CompareError::Incompatible`] for runs that
/// differ in commit, lockfile, runtime or runner.
pub fn check_contrast(contrast: &Contrast<'_>) -> Result<(), CompareError> {
    let roles = [
        (contrast.base_a, RunRole::BaseA),
        (contrast.base_b, RunRole::BaseB),
        (contrast.candidate, RunRole::Candidate),
    ];
    for (run, role) in roles {
        if run.run().role != role {
            return Err(CompareError::WrongRole {
                detail: format!("run {} is {:?}, not {role:?}", run.run().id, run.run().role),
            });
        }
        check_spec(&context(contrast, run))?;
    }
    let (a, b, c) = (
        contrast.base_a.run(),
        contrast.base_b.run(),
        contrast.candidate.run(),
    );
    if a.commit != b.commit || a.env.lockfile_hash != b.env.lockfile_hash {
        return Err(CompareError::Incompatible {
            detail: "base A and base B are not on the same commit and lockfile".to_owned(),
        });
    }
    if a.env.runtime != b.env.runtime || a.env.runtime != c.env.runtime {
        return Err(CompareError::Incompatible {
            detail: "the three runs do not share a runtime".to_owned(),
        });
    }
    if a.runner != b.runner || a.runner != c.runner {
        return Err(CompareError::Incompatible {
            detail: "the three runs do not share a runner".to_owned(),
        });
    }
    Ok(())
}

/// Checks that `comparison` is exactly the comparison these three runs make under this
/// contrast's probe and normalization plan, by recomputing it. Run identities, observation
/// hashes and their order, the plan, and the outcome must all agree.
///
/// # Errors
///
/// Returns [`CompareError::ForeignComparison`] when the comparison differs from the recomputed
/// one, and the errors of [`compare`] when the contrast cannot be compared.
pub fn verify_comparison(
    contrast: &Contrast<'_>,
    comparison: &CoreDerived<Comparison>,
) -> Result<(), CompareError> {
    let expected = compare(contrast, &derive_masks(contrast)?)?;
    if expected.value() == comparison.value() {
        Ok(())
    } else {
        Err(CompareError::ForeignComparison {
            detail: "the comparison differs from the one these runs make under this plan"
                .to_owned(),
        })
    }
}

/// Whether the run recorded a capture that did not bind. A missing capture is never a pass,
/// whichever predicate reads it.
pub(crate) fn missing_capture(run: &ObservedRun) -> bool {
    run.run()
        .captures
        .iter()
        .any(|outcome| matches!(outcome, CaptureOutcome::Missing { .. }))
}

/// The bindings a run captured, resolved from their source steps and checked against the
/// fingerprints the run recorded. A capture that did not bind is absent from the map.
pub(crate) fn captured_values(
    context: &RunContext<'_>,
) -> Result<BTreeMap<CaptureName, Value>, CompareError> {
    check_spec(context)?;
    let mut captured = BTreeMap::new();
    for step in context
        .fixture
        .recipe
        .iter()
        .chain(context.probe.spec.steps.iter())
    {
        let Step::Capture(capture) = step else {
            continue;
        };
        let outcome = context.run.run().captures.iter().find(|outcome| {
            outcome.step_id() == &capture.id && outcome.binding() == &capture.binding
        });
        let Some(CaptureOutcome::Captured { value_hash, .. }) = outcome else {
            continue;
        };
        let value = read_source(context.run, capture)?;
        if ContentHash::of(&value)? != value_hash.content() {
            return Err(CompareError::CaptureUnreadable {
                binding: capture.binding.clone(),
            });
        }
        captured.insert(capture.binding.clone(), value);
    }
    Ok(captured)
}

fn check_spec(context: &RunContext<'_>) -> Result<(), CompareError> {
    let run = context.run.run();
    if context.probe.spec.fixture != context.fixture.id {
        return Err(CompareError::WrongSpec {
            detail: "the probe starts from another fixture".to_owned(),
        });
    }
    if run.probe != context.probe.spec.probe_hash()?
        || run.env.fixture_hash != context.fixture.fixture_hash()?
    {
        return Err(CompareError::WrongSpec {
            detail: "the run records another probe or fixture".to_owned(),
        });
    }
    Ok(())
}

fn read_source(run: &ObservedRun, capture: &CaptureStep) -> Result<Value, CompareError> {
    let unreadable = || CompareError::CaptureUnreadable {
        binding: capture.binding.clone(),
    };
    let (_, observation) = run
        .observations()
        .find(|(_, observation)| observation.step_id == capture.from)
        .ok_or_else(unreadable)?;
    let channels = &observation.channels;
    let value = match &capture.source {
        CaptureSource::Header { name } => channels
            .headers
            .get(name)
            .map(|text| Value::String(text.clone())),
        CaptureSource::Cookie { name } => channels
            .cookies
            .get(name)
            .map(|text| Value::String(text.clone())),
        CaptureSource::Body { path } => {
            let separator = if path.as_str().starts_with('[') {
                ""
            } else {
                "."
            };
            let parsed: ObservationPath = format!("body{separator}{}", path.as_str())
                .parse()
                .map_err(|_| unreadable())?;
            match channels.lookup(&parsed) {
                Evidence::Present { value } => Some(value),
                Evidence::Missing {} => None,
            }
        }
    };
    value.ok_or_else(unreadable)
}

/// The inputs canonical values are built from: the plan and the run's captured values.
struct Canon<'a> {
    plan: &'a NormalizationPlan,
    captures: &'a BTreeMap<CaptureName, Value>,
}

/// Shortest captured text a substring replacement may match, so a short value never rewrites
/// unrelated text.
const MIN_IDENTITY_TEXT: usize = 8;

fn canonical_channels(channels: &Channels, canon: &Canon<'_>) -> Result<Channels, CompareError> {
    let text_map = |prefix: &str, map: &BTreeMap<String, String>| {
        map.iter()
            .map(|(name, text)| {
                let path = format!("{prefix}.{name}");
                let value = canonical_tree(&path, Value::String(text.clone()), canon)?;
                let text = match value {
                    Value::String(text) => text,
                    other => other.to_string(),
                };
                Ok((name.clone(), text))
            })
            .collect::<Result<BTreeMap<_, _>, CompareError>>()
    };
    Ok(Channels {
        status: channels.status,
        headers: text_map("header", &channels.headers)?,
        cookies: text_map("cookie", &channels.cookies)?,
        body: channels
            .body
            .as_ref()
            .map(|body| canonical_tree("body", body.clone(), canon))
            .transpose()?,
        side_effects: channels
            .side_effects
            .iter()
            .map(|(sink, snapshot)| {
                let path = format!("side_effect.{}", sink_name(*sink));
                Ok((*sink, canonical_tree(&path, snapshot.clone(), canon)?))
            })
            .collect::<Result<BTreeMap<TrustedSink, Value>, CompareError>>()?,
    })
}

fn canonical_tree(path: &str, value: Value, canon: &Canon<'_>) -> Result<Value, CompareError> {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, child) in map {
                let child_path = format!("{path}.{key}");
                out.insert(key, canonical_tree(&child_path, child, canon)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for (index, child) in items.into_iter().enumerate() {
                out.push(canonical_tree(&format!("{path}[{index}]"), child, canon)?);
            }
            Ok(Value::Array(out))
        }
        leaf => canon_leaf(path, leaf, canon),
    }
}

fn canon_leaf(path: &str, value: Value, canon: &Canon<'_>) -> Result<Value, CompareError> {
    let mut identities: Vec<(&CaptureName, &Value)> = Vec::new();
    for projection in &canon.plan.projections {
        if !matches_pattern(projection.path.as_str(), path) {
            continue;
        }
        match &projection.kind {
            ProjectionKind::CanonicalIdentity { binding } => {
                identities.extend(canon.captures.get_key_value(binding));
            }
            ProjectionKind::RedactSecret {} => return redact(&value),
        }
    }
    for typed in &canon.plan.typed_captures {
        if typed.kind != CaptureKind::Opaque {
            identities.extend(canon.captures.get_key_value(&typed.binding));
        }
    }
    Ok(rewrite_value(value, &identities))
}

/// Rewrites one leaf. A value equal to a captured identity becomes its placeholder. A string
/// has each occurrence of a captured text (at least `MIN_IDENTITY_TEXT` bytes) replaced, and
/// every other character is escaped, so no literal text can read as a placeholder.
fn rewrite_value(value: Value, identities: &[(&CaptureName, &Value)]) -> Value {
    if let Some((binding, _)) = identities.iter().find(|(_, captured)| **captured == value) {
        return Value::String(placeholder(binding));
    }
    match value {
        Value::String(text) => Value::String(rewrite_text(&text, identities)),
        other => other,
    }
}

fn rewrite_text(text: &str, identities: &[(&CaptureName, &Value)]) -> String {
    let secrets: Vec<(&CaptureName, &str)> = identities
        .iter()
        .filter_map(|(binding, captured)| match captured {
            Value::String(secret) if secret.len() >= MIN_IDENTITY_TEXT => {
                Some((*binding, secret.as_str()))
            }
            _ => None,
        })
        .collect();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(next) = rest.chars().next() {
        match secrets.iter().find(|(_, secret)| rest.starts_with(*secret)) {
            Some((binding, secret)) => {
                out.push_str(&placeholder(binding));
                rest = &rest[secret.len()..];
            }
            None => {
                if matches!(next, '\\' | '<') {
                    out.push('\\');
                }
                out.push(next);
                rest = &rest[next.len_utf8()..];
            }
        }
    }
    out
}

fn placeholder(binding: &CaptureName) -> String {
    format!("<capture:{binding}>")
}

/// The bindings a canonical string names through its placeholders, in order.
fn placeholders(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(next) = rest.chars().next() {
        if next == '\\' {
            rest = &rest[1..];
            if let Some(escaped) = rest.chars().next() {
                rest = &rest[escaped.len_utf8()..];
            }
        } else if let Some(after) = rest.strip_prefix("<capture:") {
            let end = after.find('>').unwrap_or(after.len());
            out.push(&after[..end]);
            rest = after.get(end + 1..).unwrap_or_default();
        } else {
            rest = &rest[next.len_utf8()..];
        }
    }
    out
}

/// The type and string format of a captured value. Length is not part of it: identities compare
/// by binding, so two captured tokens of one format compare equal whatever their length.
fn identity_shape(value: &Value) -> String {
    match value {
        Value::String(text) => format!("string:{}", string_shape(text)),
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer".to_owned(),
        Value::Number(_) => "number".to_owned(),
        Value::Bool(_) => "boolean".to_owned(),
        Value::Null => "null".to_owned(),
        Value::Array(_) => "array".to_owned(),
        Value::Object(_) => "object".to_owned(),
    }
}

fn format_name(format: StringFormat) -> &'static str {
    match format {
        StringFormat::Uuid => "uuid",
        StringFormat::DateTime => "datetime",
        StringFormat::Uri => "uri",
        StringFormat::Hex => "hex",
        StringFormat::ShortCode => "shortcode",
    }
}

/// The placeholders of one observation whose captured value reads a different type or format
/// in the other run, as `(path, shape in x, shape in y)`. Two runs can read the same placeholder
/// while their captured values differ in kind, so canonical equality alone does not show it.
fn shape_mismatches(
    x: &Channels,
    x_values: &BTreeMap<CaptureName, Value>,
    y: &Channels,
    y_values: &BTreeMap<CaptureName, Value>,
) -> Vec<(String, String, String)> {
    let (x_nodes, y_nodes) = (node_values(x), node_values(y));
    let mut out = Vec::new();
    for (path, value) in &x_nodes {
        let Value::String(text) = value else {
            continue;
        };
        if y_nodes.get(path) != Some(value) {
            continue;
        }
        for name in placeholders(text) {
            let Ok(binding) = name.parse::<CaptureName>() else {
                continue;
            };
            let (x_shape, y_shape) = (shape_of(x_values, &binding), shape_of(y_values, &binding));
            if x_shape != y_shape {
                out.push((path.clone(), x_shape, y_shape));
            }
        }
    }
    out
}

fn shape_of(values: &BTreeMap<CaptureName, Value>, binding: &CaptureName) -> String {
    values
        .get(binding)
        .map_or_else(|| "absent".to_owned(), identity_shape)
}

fn node_values(channels: &Channels) -> BTreeMap<String, Value> {
    let mut nodes = Vec::new();
    for (path, value) in roots(channels) {
        flatten(&path, &value, &mut nodes);
    }
    nodes.into_iter().collect()
}

/// Replaces a secret with a marker that keeps its length and alphabet class and carries a digest
/// of the value, so two equal secrets still compare equal.
fn redact(value: &Value) -> Result<Value, CompareError> {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let class = match alphabet(&text) {
        Alphabet::Hex => "hex",
        Alphabet::Base64url => "base64url",
        Alphabet::Other => "other",
    };
    let digest = ContentHash::of(value)?;
    Ok(Value::String(format!(
        "<redacted:{}:{class}:{digest}>",
        text.chars().count()
    )))
}

pub(crate) fn sink_name(sink: TrustedSink) -> &'static str {
    match sink {
        TrustedSink::Store => "store",
        TrustedSink::OutboundHttp => "outbound_http",
    }
}

/// The channel roots of one observation: each header, cookie, the status, and each body or
/// sink tree, keyed by its concrete path.
fn roots(channels: &Channels) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    if let Some(status) = channels.status {
        out.insert("status".to_owned(), Value::from(status));
    }
    for (name, text) in &channels.headers {
        out.insert(format!("header.{name}"), Value::String(text.clone()));
    }
    for (name, text) in &channels.cookies {
        out.insert(format!("cookie.{name}"), Value::String(text.clone()));
    }
    if let Some(body) = &channels.body {
        out.insert("body".to_owned(), body.clone());
    }
    for (sink, snapshot) in &channels.side_effects {
        out.insert(
            format!("side_effect.{}", sink_name(*sink)),
            snapshot.clone(),
        );
    }
    out
}

/// One place two observations disagree. A leaf disagreement has a value on both sides; any other
/// disagreement is structural (presence, shape, cardinality) and never masked.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Leaf,
    Structure,
}

struct Found {
    path: String,
    kind: Kind,
    base: Option<Value>,
    candidate: Option<Value>,
}

fn step_differences(base: &Channels, other: &Channels) -> Vec<Found> {
    let (base, other) = (roots(base), roots(other));
    let keys: BTreeSet<&String> = base.keys().chain(other.keys()).collect();
    let mut out = Vec::new();
    for key in keys {
        walk(key, base.get(key), other.get(key), &mut out);
    }
    out
}

fn is_leaf(value: &Value) -> bool {
    !matches!(value, Value::Object(_) | Value::Array(_))
}

fn walk(path: &str, base: Option<&Value>, other: Option<&Value>, out: &mut Vec<Found>) {
    match (base, other) {
        (None, None) => {}
        (Some(x), Some(y)) if x == y => {}
        (Some(Value::Object(x)), Some(Value::Object(y))) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for key in keys {
                walk(&format!("{path}.{key}"), x.get(key), y.get(key), out);
            }
        }
        (Some(Value::Array(x)), Some(Value::Array(y))) if x.len() == y.len() => {
            for (index, (p, q)) in x.iter().zip(y).enumerate() {
                walk(&format!("{path}[{index}]"), Some(p), Some(q), out);
            }
        }
        (Some(x), Some(y)) if is_leaf(x) && is_leaf(y) => out.push(Found {
            path: path.to_owned(),
            kind: Kind::Leaf,
            base: Some(x.clone()),
            candidate: Some(y.clone()),
        }),
        (x, y) => out.push(Found {
            path: path.to_owned(),
            kind: Kind::Structure,
            base: x.cloned(),
            candidate: y.cloned(),
        }),
    }
}

/// A token of a path or pattern: an object key or a bracketed index.
#[derive(PartialEq, Eq)]
enum Token<'a> {
    Key(&'a str),
    Index(&'a str),
}

fn tokens(path: &str) -> Vec<Token<'_>> {
    let mut out = Vec::new();
    let mut rest = path;
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('[') {
            let close = after.find(']').unwrap_or(after.len());
            out.push(Token::Index(&after[..close]));
            rest = after.get(close + 1..).unwrap_or_default();
        } else {
            let key = rest.strip_prefix('.').unwrap_or(rest);
            let end = key.find(['.', '[']).unwrap_or(key.len());
            out.push(Token::Key(&key[..end]));
            rest = &key[end..];
        }
    }
    out
}

/// Whether a concrete path matches a pattern. `[*]` matches any index; every other token
/// matches exactly.
pub(crate) fn matches_pattern(pattern: &str, path: &str) -> bool {
    let (pattern, path) = (tokens(pattern), tokens(path));
    pattern.len() == path.len()
        && pattern.iter().zip(&path).all(|(p, q)| match (p, q) {
            (Token::Index("*"), Token::Index(_)) => true,
            (Token::Key(a), Token::Key(b)) | (Token::Index(a), Token::Index(b)) => a == b,
            _ => false,
        })
}

/// Whether a concrete path lies on, above or below a pattern's matches: one of the two has the
/// other as a prefix. A structural change above a pattern (an element removed from an array the
/// pattern reads into) therefore overlaps it, and an equality check on that pattern fails.
pub(crate) fn overlaps(pattern: &str, path: &str) -> bool {
    let (pattern, path) = (tokens(pattern), tokens(path));
    let shared = pattern.len().min(path.len());
    pattern[..shared]
        .iter()
        .zip(&path[..shared])
        .all(|(p, q)| match (p, q) {
            (Token::Index("*"), Token::Index(_)) => true,
            (Token::Key(a), Token::Key(b)) | (Token::Index(a), Token::Index(b)) => a == b,
            _ => false,
        })
}

/// Whether `path` is `pattern` or lies below it: the pattern's tokens are a prefix of the path's.
pub(crate) fn at_or_below(pattern: &str, path: &str) -> bool {
    let (pattern, path) = (tokens(pattern), tokens(path));
    pattern.len() <= path.len()
        && pattern
            .iter()
            .zip(&path)
            .all(|(wanted, actual)| match (wanted, actual) {
                (Token::Index("*"), Token::Index(_)) => true,
                (Token::Key(a), Token::Key(b)) | (Token::Index(a), Token::Index(b)) => a == b,
                _ => false,
            })
}

/// Whether the plan allows masking `path`. An opaque body is never masked: its hash is compared
/// whole, so any difference in it is a difference.
fn maskable(plan: &NormalizationPlan, path: &str) -> bool {
    !at_or_below(&format!("body.{OPAQUE_BODY_KEY}"), path)
        && plan
            .maskable_paths
            .iter()
            .any(|maskable: &MaskablePath| matches_pattern(maskable.as_str(), path))
}

/// Whether both sides of a disagreement are strings or numbers. Booleans carry control flow and
/// nulls carry presence, so neither ever masks.
fn scalar_pair(found: &Found) -> bool {
    [found.base.as_ref(), found.candidate.as_ref()]
        .iter()
        .all(|value| matches!(value, Some(Value::String(_) | Value::Number(_))))
}

fn parse_path(path: &str) -> Result<ObservationPath, CompareError> {
    ObservationPath::from_str(path).map_err(|_| CompareError::Unaddressable {
        path: path.to_owned(),
    })
}

fn shape_mismatch(a: &CanonicalRun, b: &CanonicalRun) -> Option<String> {
    if a.observations.len() != b.observations.len() {
        return Some(format!(
            "{} and {} observations",
            a.observations.len(),
            b.observations.len()
        ));
    }
    a.observations
        .iter()
        .zip(&b.observations)
        .find(|(x, y)| x.step_id != y.step_id)
        .map(|(x, y)| format!("step {} is not step {}", x.step_id, y.step_id))
}

/// The masked fields of a contrast, checked against the base runs they were derived from.
fn masked_fields(
    contrast: &Contrast<'_>,
    masks: &[CoreDerived<Mask>],
    base_a: &CanonicalRun,
    base_b: &CanonicalRun,
) -> Result<BTreeSet<(StepId, String)>, CompareError> {
    let plan = &contrast.probe.normalization;
    let probe_hash = contrast.probe.spec.probe_hash()?;
    let plan_hash = plan.normalization_hash()?;
    let mut fields = BTreeSet::new();
    for mask in masks {
        let mask = mask.value();
        let foreign = |detail: &str| CompareError::ForeignMask {
            detail: detail.to_owned(),
        };
        if mask.probe_hash != probe_hash {
            return Err(foreign("the mask is for another probe"));
        }
        if mask.normalization_hash != plan_hash || mask.normalization_version != plan.version {
            return Err(foreign("the mask is for another normalization plan"));
        }
        let path = mask.selector.path.as_str();
        if !maskable(plan, path) {
            return Err(foreign("the field is not maskable under the plan"));
        }
        let step = &mask.selector.step_id;
        let scalar = canonical_evidence(base_a, step, &mask.selector.path);
        if !matches!(
            scalar,
            Evidence::Present {
                value: Value::String(_) | Value::Number(_)
            }
        ) {
            return Err(foreign(
                "the masked field is not a string or number in base A",
            ));
        }
        let base_hash = |run: &CanonicalRun| {
            run.observations
                .iter()
                .find(|observation| &observation.step_id == step)
                .map(|observation| observation.observation_hash)
        };
        if base_hash(base_a) != Some(mask.base_a_observation_hash)
            || base_hash(base_b) != Some(mask.base_b_observation_hash)
        {
            return Err(foreign("the mask names other base observations"));
        }
        fields.insert((step.clone(), path.to_owned()));
    }
    Ok(fields)
}

/// Decides the outcome of a contrast from its canonical runs, in the order the module docs
/// give.
fn judge(
    contrast: &Contrast<'_>,
    [a, b, c]: [&CanonicalRun; 3],
    masked: &BTreeSet<(StepId, String)>,
    probe_hash: ProbeHash,
) -> Result<Outcome, CompareError> {
    let observed = [contrast.base_a, contrast.base_b, contrast.candidate];
    if observed
        .iter()
        .any(|run| !matches!(run.run().outcome, RunOutcome::Completed {}))
    {
        return Ok(unknown(UnknownReason::RunFailed {}));
    }
    if observed.iter().any(|run| missing_capture(run)) {
        return Ok(unknown(UnknownReason::Incomplete {}));
    }
    if let Some(reason) = observed.iter().find_map(|run| incomplete_reason(run)) {
        return Ok(unknown(reason));
    }
    if let Some(detail) = shape_mismatch(a, b).or_else(|| shape_mismatch(a, c)) {
        return Ok(unknown(UnknownReason::Incomparable { detail }));
    }
    let [a_values, b_values, c_values] =
        observed.map(|run| captured_values(&context(contrast, run)));
    let (a_values, b_values, c_values) = (a_values?, b_values?, c_values?);
    let mut differences = Differences::default();
    for relation in &contrast.probe.normalization.relations {
        let (Some(in_a), Some(in_b), Some(in_c)) = (
            violations(contrast.base_a, relation)?,
            violations(contrast.base_b, relation)?,
            violations(contrast.candidate, relation)?,
        ) else {
            return Ok(unknown(UnknownReason::Incomparable {
                detail: "a relation names a step the run does not have".to_owned(),
            }));
        };
        if in_a.is_empty() != in_b.is_empty() {
            return Ok(unstable());
        }
        if !in_a.is_empty() {
            return Ok(unknown(UnknownReason::Incomparable {
                detail: "a declared relation does not hold on the base runs".to_owned(),
            }));
        }
        for located in &in_c {
            differences.add(Difference {
                probe_hash,
                step_id: located.step_id.clone(),
                path: located.path.clone(),
                base: canonical_evidence(a, &located.step_id, &located.path),
                candidate: canonical_evidence(c, &located.step_id, &located.path),
                episode_id: None,
            });
        }
    }
    for (x, y) in a.observations.iter().zip(&b.observations) {
        for found in step_differences(&x.channels, &y.channels) {
            let is_masked = found.kind == Kind::Leaf
                && masked.contains(&(x.step_id.clone(), found.path.clone()));
            if !is_masked {
                return Ok(unstable());
            }
        }
        if !shape_mismatches(&x.channels, &a_values, &y.channels, &b_values).is_empty() {
            return Ok(unstable());
        }
    }
    for (x, z) in a.observations.iter().zip(&c.observations) {
        for found in step_differences(&x.channels, &z.channels) {
            let is_masked = found.kind == Kind::Leaf
                && masked.contains(&(x.step_id.clone(), found.path.clone()));
            if !is_masked {
                differences.add(found_difference(probe_hash, &x.step_id, &found)?);
            }
        }
        for (path, base_shape, candidate_shape) in
            shape_mismatches(&x.channels, &a_values, &z.channels, &c_values)
        {
            differences.add(Difference {
                probe_hash,
                step_id: x.step_id.clone(),
                path: parse_path(&path)?,
                base: Evidence::Present {
                    value: Value::String(base_shape),
                },
                candidate: Evidence::Present {
                    value: Value::String(candidate_shape),
                },
                episode_id: None,
            });
        }
    }
    for (step, path) in masked {
        let (Some(base_a), Some(base_b), Some(candidate)) = (
            original_at(contrast.base_a, step, path)?,
            original_at(contrast.base_b, step, path)?,
            original_at(contrast.candidate, step, path)?,
        ) else {
            continue;
        };
        let base_formats = [format_of(&base_a), format_of(&base_b)];
        if !base_formats.contains(&format_of(&candidate)) {
            differences.add(Difference {
                probe_hash,
                step_id: step.clone(),
                path: parse_path(path)?,
                base: canonical_evidence(a, step, &parse_path(path)?),
                candidate: canonical_evidence(c, step, &parse_path(path)?),
                episode_id: None,
            });
        }
    }
    let differences = differences.into_vec();
    Ok(if differences.is_empty() {
        Outcome::Unchanged {}
    } else {
        Outcome::Changed { differences }
    })
}

fn unknown(reason: UnknownReason) -> Outcome {
    Outcome::Unknown { reason }
}

fn unstable() -> Outcome {
    unknown(UnknownReason::UnstableBase {})
}

/// Why a run cannot be judged because one of its observations is incomplete. A skipped step
/// is named as such, whichever observation comes first.
pub(crate) fn incomplete_reason(run: &ObservedRun) -> Option<UnknownReason> {
    let mut found = None;
    for (_, observation) in run.observations() {
        match incomplete_observation(observation) {
            Some(UnknownReason::Skipped {}) => return Some(UnknownReason::Skipped {}),
            Some(reason) => found = Some(reason),
            None => {}
        }
    }
    found
}

/// Why one observation cannot be judged, if it is incomplete.
pub(crate) fn incomplete_observation(observation: &Observation) -> Option<UnknownReason> {
    let Completeness::Incomplete { reasons } = &observation.completeness else {
        return None;
    };
    if reasons
        .iter()
        .any(|reason| matches!(reason, IncompleteReason::StepSkipped {}))
    {
        Some(UnknownReason::Skipped {})
    } else {
        Some(UnknownReason::Incomplete {})
    }
}

fn found_difference(
    probe_hash: ProbeHash,
    step_id: &StepId,
    found: &Found,
) -> Result<Difference, CompareError> {
    Ok(Difference {
        probe_hash,
        step_id: step_id.clone(),
        path: parse_path(&found.path)?,
        base: evidence(found.base.as_ref()),
        candidate: evidence(found.candidate.as_ref()),
        episode_id: None,
    })
}

fn evidence(value: Option<&Value>) -> Evidence {
    value.map_or(Evidence::Missing {}, |value| Evidence::Present {
        value: value.clone(),
    })
}

fn canonical_evidence(run: &CanonicalRun, step_id: &StepId, path: &ObservationPath) -> Evidence {
    run.observations
        .iter()
        .find(|observation| &observation.step_id == step_id)
        .map_or(Evidence::Missing {}, |observation| {
            observation.channels.lookup(path)
        })
}

/// The original value at `path` in the step's observation of `run`, if both exist.
fn original_at(
    run: &ObservedRun,
    step: &StepId,
    path: &str,
) -> Result<Option<Value>, CompareError> {
    let parsed = parse_path(path)?;
    Ok(
        match run
            .observed(step, &parsed)
            .map(|observed| observed.value().clone())
        {
            Some(Evidence::Present { value }) => Some(value),
            Some(Evidence::Missing {}) | None => None,
        },
    )
}

/// The differences the comparison reports, once per field.
#[derive(Default)]
struct Differences {
    list: Vec<Difference>,
    seen: BTreeSet<(StepId, ObservationPath)>,
}

impl Differences {
    fn add(&mut self, difference: Difference) {
        if self
            .seen
            .insert((difference.step_id.clone(), difference.path.clone()))
        {
            self.list.push(difference);
        }
    }

    fn into_vec(self) -> Vec<Difference> {
        self.list
    }
}

/// A value a relation's fields hold, with the field it came from.
#[derive(Clone)]
struct Located {
    step_id: StepId,
    path: ObservationPath,
    value: Value,
}

/// A concrete path and value, for every node of the observed tree that the pattern matches.
fn expand(
    channels: &Channels,
    pattern: &PathPattern,
) -> Result<Vec<(ObservationPath, Value)>, CompareError> {
    let mut nodes = Vec::new();
    for (path, value) in roots(channels) {
        flatten(&path, &value, &mut nodes);
    }
    nodes
        .into_iter()
        .filter(|(path, _)| matches_pattern(pattern.as_str(), path))
        .map(|(path, value)| Ok((parse_path(&path)?, value)))
        .collect()
}

pub(crate) fn flatten(path: &str, value: &Value, out: &mut Vec<(String, Value)>) {
    out.push((path.to_owned(), value.clone()));
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                flatten(&format!("{path}.{key}"), child, out);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                flatten(&format!("{path}[{index}]"), child, out);
            }
        }
        _ => {}
    }
}

fn locate<'a>(
    run: &ObservedRun,
    paths: impl Iterator<Item = &'a StepPath>,
) -> Result<Option<Vec<Located>>, CompareError> {
    let mut out = Vec::new();
    for step_path in paths {
        let Some((_, observation)) = run
            .observations()
            .find(|(_, observation)| observation.step_id == step_path.step_id)
        else {
            return Ok(None);
        };
        for (path, value) in expand(&observation.channels, &step_path.path)? {
            out.push(Located {
                step_id: step_path.step_id.clone(),
                path,
                value,
            });
        }
    }
    Ok(Some(out))
}

/// The fields that break `relation` in one run, judged on original values. `None` when the
/// relation names a step the run does not have.
fn violations(
    run: &ObservedRun,
    relation: &Relation,
) -> Result<Option<Vec<Located>>, CompareError> {
    Ok(match relation {
        Relation::Equal { paths } => locate(run, paths.iter())?.map(|values| {
            values.first().map_or_else(Vec::new, |first| {
                values
                    .iter()
                    .filter(|located| located.value != first.value)
                    .cloned()
                    .collect()
            })
        }),
        Relation::Unique { paths } => locate(run, paths.iter())?.map(|values| {
            let mut seen: Vec<&Value> = Vec::new();
            let mut broken = Vec::new();
            for located in &values {
                if seen.contains(&&located.value) {
                    broken.push(located.clone());
                } else {
                    seen.push(&located.value);
                }
            }
            broken
        }),
        Relation::References { from, to } => {
            let from_values = locate(run, std::iter::once(from))?;
            let to_values = locate(run, std::iter::once(to))?;
            match (from_values, to_values) {
                (Some(from_values), Some(to_values)) => Some(
                    from_values
                        .into_iter()
                        .filter(|from| !to_values.iter().any(|to| to.value == from.value))
                        .collect(),
                ),
                _ => None,
            }
        }
    })
}

/// A coarse format class: JSON type, plus the string format or token class where one applies.
#[derive(Debug, PartialEq, Eq)]
enum Format {
    Null,
    Boolean,
    Integer,
    Number,
    /// A structured string (uuid, date-time, uri), or other text when `None`.
    Text(Option<StringFormat>),
    /// A token: text in the hex or base64url alphabet, with the length band it falls in.
    Token {
        alphabet: Alphabet,
        band: u32,
    },
    Array,
    Object,
}

fn format_of(value: &Value) -> Format {
    match value {
        Value::Null => Format::Null,
        Value::Bool(_) => Format::Boolean,
        Value::Number(number) if number.is_i64() || number.is_u64() => Format::Integer,
        Value::Number(_) => Format::Number,
        Value::String(text) => match string_format(text) {
            Some(format) => Format::Text(Some(format)),
            None => match token_alphabet(text) {
                Alphabet::Other => Format::Text(None),
                token => Format::Token {
                    alphabet: token,
                    band: length_band(text),
                },
            },
        },
        Value::Array(_) => Format::Array,
        Value::Object(_) => Format::Object,
    }
}

/// The alphabet a string's characters are drawn from, narrowest first. Alphanumeric text is
/// base64url text, so a random base64url token does not change class with the characters it
/// draws. Only a token made entirely of hex digits is hex: for 43 characters that is about one
/// in 10^26.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alphabet {
    Hex,
    Base64url,
    Other,
}

impl Alphabet {
    fn name(self) -> &'static str {
        match self {
            Self::Hex => "hex",
            Self::Base64url => "base64url",
            Self::Other => "text",
        }
    }
}

fn alphabet(text: &str) -> Alphabet {
    if text.is_empty() {
        Alphabet::Other
    } else if text
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Alphabet::Hex
    } else if text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Alphabet::Base64url
    } else {
        Alphabet::Other
    }
}

/// The shortest text that a masked token is classed as hex. A shorter base64url token is all hex
/// digits by chance too often: a 7-character code from the demo's base62 alphabet is, about one
/// time in 13,000. Identity shapes take any hex text as hex, whatever its length.
const HEX_MIN_LENGTH: usize = 16;

/// The alphabet a masked token's format is judged by: [`alphabet`], except that text under
/// [`HEX_MIN_LENGTH`] characters is never hex.
fn token_alphabet(text: &str) -> Alphabet {
    match alphabet(text) {
        Alphabet::Hex if text.len() < HEX_MIN_LENGTH => Alphabet::Base64url,
        other => other,
    }
}

/// The length band of a text: the number of bits its length takes, so every length from
/// 2^(n-1) to 2^n - 1 falls in band n. Tokens of one generator share a band, and a short value
/// lands in a band of its own.
fn length_band(text: &str) -> u32 {
    usize::BITS - text.len().leading_zeros()
}

/// The name of a string's format for an identity: its structured format when it has one,
/// otherwise its alphabet. Length is left out.
fn string_shape(text: &str) -> &'static str {
    string_format(text).map_or_else(|| alphabet(text).name(), format_name)
}

/// The structured format a string has, if any: uuid, date-time or uri. Alphabets are not
/// formats; see [`alphabet`].
fn string_format(text: &str) -> Option<StringFormat> {
    [
        StringFormat::Uuid,
        StringFormat::DateTime,
        StringFormat::Uri,
    ]
    .into_iter()
    .find(|format| text_has_format(text, *format))
}

/// Whether `text` has `format`. A string can have several formats (`1234` is both hex and a
/// short code), so a schema check asks about one format at a time.
pub(crate) fn text_has_format(text: &str, format: StringFormat) -> bool {
    match format {
        StringFormat::Uuid => is_uuid(text),
        StringFormat::DateTime => is_datetime(text),
        StringFormat::Uri => is_uri(text),
        StringFormat::Hex => {
            !text.is_empty()
                && text
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        StringFormat::ShortCode => {
            !text.is_empty() && text.bytes().all(|b| b.is_ascii_alphanumeric())
        }
    }
}

fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// RFC 3339: `YYYY-MM-DDTHH:MM:SS`, an optional fraction, then `Z` or a numeric offset.
fn is_datetime(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || !bytes[..19]
            .iter()
            .enumerate()
            .all(|(index, byte)| match index {
                4 | 7 => *byte == b'-',
                10 => *byte == b'T',
                13 | 16 => *byte == b':',
                _ => byte.is_ascii_digit(),
            })
    {
        return false;
    }
    let mut rest = &bytes[19..];
    if let Some(fraction) = rest.strip_prefix(b".") {
        let digits = fraction.iter().take_while(|b| b.is_ascii_digit()).count();
        if digits == 0 {
            return false;
        }
        rest = &fraction[digits..];
    }
    match rest {
        b"Z" => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => [h1, h2, m1, m2].iter().all(|b| b.is_ascii_digit()),
        _ => false,
    }
}

fn is_uri(text: &str) -> bool {
    text.split_once("://").is_some_and(|(scheme, rest)| {
        !scheme.is_empty() && scheme.bytes().all(|b| b.is_ascii_alphanumeric()) && !rest.is_empty()
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hash::ObservationHash;
    use crate::observation::Observation;
    use crate::probe::{ProbeCorpus, Run};
    use serde_json::json;

    pub(crate) fn corpus() -> ProbeCorpus {
        serde_json::from_str(include_str!("../fixtures/probe-corpus.json")).expect("corpus")
    }

    pub(crate) fn admitted_run() -> ObservedRun {
        let run: Run = serde_json::from_str(include_str!("../fixtures/run.json")).expect("run");
        let observation: Observation =
            serde_json::from_str(include_str!("../fixtures/observation.json")).expect("obs");
        let hash: ObservationHash = observation.observation_hash().expect("hash");
        ObservedRun::admitted(run, vec![(hash, observation)])
    }

    #[test]
    fn patterns_match_every_index_and_nothing_else() {
        assert!(matches_pattern("body.links[*].code", "body.links[3].code"));
        assert!(!matches_pattern("body.links[*].code", "body.links.code"));
        assert!(!matches_pattern("body.links[*]", "body.links[3].code"));
        assert!(matches_pattern("header.retry-after", "header.retry-after"));
        assert!(!matches_pattern("cookie.session", "cookie.sessions"));
    }

    #[test]
    fn a_value_that_names_no_identity_is_kept() {
        let captured = Value::String("abcdefghijklmnop".to_owned());
        let binding: CaptureName = "alice".parse().expect("a binding");
        assert_eq!(
            rewrite_value(captured.clone(), &[(&binding, &captured)]),
            Value::String("<capture:alice>".to_owned())
        );
        let short = Value::String("abc".to_owned());
        assert_eq!(
            rewrite_value(Value::String("xabcx".to_owned()), &[(&binding, &short)]),
            Value::String("xabcx".to_owned())
        );
    }

    #[test]
    fn literal_text_cannot_spell_a_placeholder() {
        let captured = Value::String("abcdefghijklmnop".to_owned());
        let binding: CaptureName = "alice".parse().expect("a binding");
        let identities = [(&binding, &captured)];
        let literal = Value::String("<capture:alice>".to_owned());
        assert_eq!(
            rewrite_value(literal, &identities),
            Value::String("\\<capture:alice>".to_owned())
        );
        assert_eq!(
            rewrite_value(Value::String("a\\b".to_owned()), &[]),
            Value::String("a\\\\b".to_owned())
        );
        assert!(placeholders("\\<capture:alice>").is_empty());
        assert_eq!(placeholders("<capture:alice>"), vec!["alice"]);
    }

    #[test]
    fn every_occurrence_of_a_long_identity_is_replaced_and_only_it() {
        let captured = Value::String("abcdefghijklmnop".to_owned());
        let binding: CaptureName = "alice".parse().expect("a binding");
        let text = Value::String("tok=abcdefghijklmnop;next=abcdefghijklmnop;x=1".to_owned());
        assert_eq!(
            rewrite_value(text, &[(&binding, &captured)]),
            Value::String("tok=<capture:alice>;next=<capture:alice>;x=1".to_owned())
        );
    }

    #[test]
    fn identity_shape_keeps_type_and_format_but_not_length() {
        let hex = Value::String("3f2b8c1e5d4a4b6c8d9e0f1a2b3c4d5e".to_owned());
        let other_hex = Value::String("0a1b2c3d4e5f60718293a4b5c6d7e8f9".to_owned());
        let short = Value::String("x".to_owned());
        assert_eq!(identity_shape(&hex), identity_shape(&other_hex));
        assert_ne!(identity_shape(&hex), identity_shape(&short));
        assert_ne!(identity_shape(&Value::from(7)), identity_shape(&short));
        assert_eq!(identity_shape(&Value::from(7)), "integer");
    }

    #[test]
    fn a_number_and_a_text_under_one_placeholder_differ_in_shape() {
        let binding: CaptureName = "alice".parse().expect("a binding");
        let (number, text) = (Value::from(7), Value::String("x".to_owned()));
        let x_values = BTreeMap::from([(binding.clone(), number.clone())]);
        let y_values = BTreeMap::from([(binding.clone(), text.clone())]);
        let placed = |value: &Value, values: &BTreeMap<CaptureName, Value>| {
            let identities: Vec<(&CaptureName, &Value)> = values.iter().collect();
            json!({ "id": rewrite_value(value.clone(), &identities) })
        };
        let channels = |body: Value| Channels {
            status: None,
            headers: BTreeMap::new(),
            cookies: BTreeMap::new(),
            body: Some(body),
            side_effects: BTreeMap::new(),
        };
        let x = channels(placed(&number, &x_values));
        let y = channels(placed(&text, &y_values));
        assert_eq!(x.body, y.body, "both read the same placeholder");
        assert_eq!(shape_mismatches(&x, &x_values, &y, &y_values).len(), 1);
    }

    #[test]
    fn formats_separate_the_shapes_a_mask_must_keep() {
        assert_eq!(
            format_of(&Value::String(
                "3f2b8c1e-5d4a-4b6c-8d9e-0f1a2b3c4d5e".to_owned()
            )),
            Format::Text(Some(StringFormat::Uuid))
        );
        assert_ne!(
            format_of(&Value::String("hello world".to_owned())),
            format_of(&Value::String(
                "3f2b8c1e-5d4a-4b6c-8d9e-0f1a2b3c4d5e".to_owned()
            ))
        );
        assert_ne!(format_of(&Value::Null), format_of(&Value::from(0)));
    }

    #[test]
    fn a_token_keeps_one_format_whatever_characters_it_draws() {
        let alphanumeric = format!("{}Z", "Ab".repeat(21));
        let dashed = format!("{}Z", "Ab-".repeat(14));
        assert_eq!(alphanumeric.len(), 43);
        assert_eq!(dashed.len(), 43);
        let token = Format::Token {
            alphabet: Alphabet::Base64url,
            band: 6,
        };
        assert_eq!(format_of(&Value::String(alphanumeric.clone())), token);
        assert_eq!(format_of(&Value::String(dashed.clone())), token);
        assert_eq!(
            identity_shape(&Value::String(alphanumeric)),
            identity_shape(&Value::String(dashed))
        );
    }

    #[test]
    fn a_token_in_another_length_band_or_alphabet_is_another_format() {
        let token = format!("{}Z", "Ab".repeat(21));
        assert_ne!(
            format_of(&Value::String("x".to_owned())),
            format_of(&Value::String(token.clone()))
        );
        let standard = format!("{}+", "Ab".repeat(21));
        assert_eq!(format_of(&Value::String(standard)), Format::Text(None));
        assert_eq!(
            format_of(&Value::String(
                "3f2b8c1e5d4a4b6c8d9e0f1a2b3c4d5e".to_owned()
            )),
            Format::Token {
                alphabet: Alphabet::Hex,
                band: 6,
            }
        );
    }

    #[test]
    fn a_short_text_of_hex_digits_keeps_the_base64url_format() {
        assert_eq!(
            format_of(&Value::String("3f2b8c1".to_owned())),
            Format::Token {
                alphabet: Alphabet::Base64url,
                band: 3,
            }
        );
    }

    #[test]
    fn a_change_in_shape_or_presence_is_structure_and_never_a_leaf() {
        let mut out = Vec::new();
        walk(
            "body.items",
            Some(&json!([1, 2])),
            Some(&json!([1, 2, 3])),
            &mut out,
        );
        walk("body.gone", Some(&json!("x")), None, &mut out);
        walk(
            "body.kind",
            Some(&json!({"a": 1})),
            Some(&json!("a")),
            &mut out,
        );
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|found| found.kind == Kind::Structure));

        let mut value = Vec::new();
        walk(
            "body.list",
            Some(&json!([1, 2])),
            Some(&json!([1, 3])),
            &mut value,
        );
        assert_eq!(value.len(), 1);
        assert_eq!(value[0].path, "body.list[1]");
        assert!(value[0].kind == Kind::Leaf);
    }

    #[test]
    fn only_a_string_or_number_pair_can_be_masked() {
        let pair = |base: Option<Value>, candidate: Option<Value>| Found {
            path: "body.x".to_owned(),
            kind: Kind::Leaf,
            base,
            candidate,
        };
        assert!(scalar_pair(&pair(Some(json!("a")), Some(json!("b")))));
        assert!(scalar_pair(&pair(Some(json!(1)), Some(json!(2)))));
        assert!(!scalar_pair(&pair(Some(json!(true)), Some(json!(false)))));
        assert!(!scalar_pair(&pair(Some(Value::Null), Some(json!("b")))));
        assert!(!scalar_pair(&pair(Some(json!("a")), None)));
    }

    #[test]
    fn a_redacted_secret_hides_its_value_and_equal_secrets_stay_equal() {
        let secret = Value::String("0123456789abcdef".to_owned());
        let marked = redact(&secret).expect("a hash");
        let text = marked.as_str().expect("text");
        assert!(text.starts_with("<redacted:16:hex:"), "{text}");
        assert!(!text.contains("0123456789abcdef"), "{text}");
        assert_eq!(redact(&secret).expect("a hash"), marked);
        assert_ne!(
            redact(&Value::String("fedcba9876543210".to_owned())).expect("a hash"),
            marked
        );
    }

    #[test]
    fn a_path_above_or_below_a_pattern_overlaps_it() {
        assert!(overlaps("body.links[*].code", "body.links"));
        assert!(overlaps("body.links[*].code", "body.links[2].code.extra"));
        assert!(!overlaps("body.links[*].code", "body.other"));
        assert!(!overlaps("body.links[*].code", "header.links"));
    }

    #[test]
    fn a_string_format_is_checked_one_format_at_a_time() {
        assert!(text_has_format(
            "2024-01-02T03:04:05.678+05:30",
            StringFormat::DateTime
        ));
        assert!(!text_has_format("2024-01-02", StringFormat::DateTime));
        assert!(text_has_format("https://example.test/a", StringFormat::Uri));
        assert!(!text_has_format("example.test", StringFormat::Uri));
        assert!(text_has_format("1234", StringFormat::Hex));
        assert!(text_has_format("1234", StringFormat::ShortCode));
    }
}
