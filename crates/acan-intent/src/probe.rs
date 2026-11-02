//! Probes, fixtures, the app runner contract, and runs.
//!
//! A probe is a fixed sequence of typed steps against a fresh app instance. Its identity is the
//! hash of its [`ProbeSpec`]. Probe generators choose inputs only: no step carries an expected
//! value. Expected values come from base, a ruling, or a scope example.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::container::{BUN_RUNTIME, NODE_RUNTIME};
use crate::hash::{
    ArtifactHash, CanonicalError, ContentHash, EvidenceHash, FixtureHash, ProbeHash, RunnerHash,
};
use crate::observation::NormalizationPlan;
use crate::types::{
    ActorName, CaptureName, ClientName, CommitSha, FieldPath, FixtureId, HttpMethod, InvalidValue,
    MachineId, NonEmpty, RouteTemplate, RunId, RuntimeId, StepId, Surface, TrustedSink, UnixMillis,
};
use crate::version::{ProbeCorpusV1, RunV1, RunnerContractV1};

/// Text with `${capture}` placeholders the runner fills from run-local captures.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Template(String);

impl Template {
    /// The raw template.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The capture names the template refers to, in order.
    pub fn placeholders(&self) -> impl Iterator<Item = &str> {
        self.0
            .split("${")
            .skip(1)
            .filter_map(|part| part.split_once('}').map(|(name, _)| name))
    }
}

impl TryFrom<String> for Template {
    type Error = InvalidValue;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let malformed = || InvalidValue::Malformed {
            what: "template",
            raw: raw.clone(),
        };
        let mut rest = raw.as_str();
        while let Some(open) = rest.find("${") {
            let after = &rest[open + 2..];
            let close = after.find('}').ok_or_else(malformed)?;
            CaptureName::from_str(&after[..close]).map_err(|_| malformed())?;
            rest = &after[close + 1..];
        }
        Ok(Self(raw))
    }
}

impl fmt::Display for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Template {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Template {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Template {
    fn schema_name() -> Cow<'static, str> {
        "Template".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({ "type": "string", "description": "text with ${capture} placeholders" })
    }
}

/// How the runner treats a redirect response.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RedirectPolicy {
    /// Record the redirect response itself. The default for probes.
    Manual,
    /// Follow up to five redirects on the app's own origin.
    FollowSameOrigin,
}

/// One HTTP request template.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestTemplate {
    /// The method.
    pub method: HttpMethod,
    /// The path and query, such as `/api/links/${code}`.
    pub path: Template,
    /// Extra headers by lowercase name. The runner sets host, content type and session itself.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, Template>,
    /// A JSON body. String leaves may hold `${capture}` placeholders. Absent sends no body;
    /// JSON `null` is a present body and sends `null`.
    #[serde(
        default,
        deserialize_with = "crate::types::present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub body: Option<serde_json::Value>,
}

/// Sends one request.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpStep {
    /// The step.
    pub id: StepId,
    /// The fixture actor whose captured session the request carries, or none for anonymous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActorName>,
    /// The client identity the runner presents, such as a client address.
    pub client: ClientName,
    /// The request.
    pub request: RequestTemplate,
    /// Redirect handling.
    pub redirect: RedirectPolicy,
    /// How long the response may take.
    pub deadline_ms: u64,
}

/// Where a capture reads its value.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "part", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureSource {
    /// A response header.
    Header {
        /// The lowercase header name.
        name: String,
    },
    /// A cookie the response set.
    Cookie {
        /// The cookie name.
        name: String,
    },
    /// A JSON body field.
    Body {
        /// The field.
        path: FieldPath,
    },
}

/// What kind of value a capture holds. Identity kinds are canonicalized across steps.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    /// A session cookie or bearer token the runner attaches for the actor.
    Session,
    /// An entity identifier, such as a short code or user id.
    Identifier,
    /// Another secret-like token, kept only as a hash in evidence.
    Token,
    /// Any other value.
    Opaque,
}

/// Binds a value from an earlier step's response to a run-local name.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureStep {
    /// The step.
    pub id: StepId,
    /// The HTTP step whose response is read.
    pub from: StepId,
    /// Where in that response.
    pub source: CaptureSource,
    /// The binding.
    pub binding: CaptureName,
    /// What kind of value.
    pub kind: CaptureKind,
    /// The actor a session capture belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActorName>,
}

/// Advances the app's controlled clock.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AdvanceTimeStep {
    /// The step.
    pub id: StepId,
    /// How far.
    pub by_ms: u64,
}

/// When a side-effect observation is complete.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompletionBarrier {
    /// The sink must report the correlated request as finished within this time.
    pub timeout_ms: u64,
    /// And then stay quiet this long.
    pub quiet_ms: u64,
}

/// Reads a trusted sink for the effects of one earlier request.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SideEffectStep {
    /// The step.
    pub id: StepId,
    /// The sink.
    pub sink: TrustedSink,
    /// The HTTP step whose effects are read.
    pub correlate: StepId,
    /// When the observation is complete. Incomplete is never a pass.
    pub barrier: CompletionBarrier,
}

/// One step of a probe or fixture recipe, tagged `step` so a capture's own `kind` stays free.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// Send a request.
    Http(HttpStep),
    /// Bind a value from a response.
    Capture(CaptureStep),
    /// Advance the controlled clock.
    AdvanceTime(AdvanceTimeStep),
    /// Read a trusted sink.
    ObserveSideEffect(SideEffectStep),
}

impl Step {
    /// The step's id.
    #[must_use]
    pub fn id(&self) -> &StepId {
        match self {
            Self::Http(step) => &step.id,
            Self::Capture(step) => &step.id,
            Self::AdvanceTime(step) => &step.id,
            Self::ObserveSideEffect(step) => &step.id,
        }
    }
}

/// How fixture state is isolated. v1 has one answer.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Debug,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A fresh app process with a fresh store for every probe sequence.
    #[default]
    FreshInstance,
}

/// The state a probe starts from: recipe steps run on the fresh instance before the probe's.
///
/// A recipe step that fails makes the run [`RunOutcome::SetupFailed`], never a pass.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    /// The fixture.
    pub id: FixtureId,
    /// The seed steps.
    pub recipe: Vec<Step>,
    /// The isolation boundary.
    pub isolation: Isolation,
    /// The runner contract the recipe is written against.
    pub runner_contract: RunnerContractV1,
}

impl Fixture {
    /// The fixture's identity, bound into the gate context.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn fixture_hash(&self) -> Result<FixtureHash, CanonicalError> {
        ContentHash::of(self).map(FixtureHash::from)
    }
}

/// The hashed part of a probe: what it sends, in what order, from which fixture.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProbeSpec {
    /// The interface kind.
    pub surface: Surface,
    /// The starting state.
    pub fixture: FixtureId,
    /// The steps.
    pub steps: NonEmpty<Step>,
}

impl ProbeSpec {
    /// The probe's identity.
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn probe_hash(&self) -> Result<ProbeHash, CanonicalError> {
        ContentHash::of(self).map(ProbeHash::from)
    }
}

/// Why a probe exists.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProbeOrigin {
    /// From a confirmed scope example.
    ScopeExample,
    /// From a ruling.
    Ruling,
    /// From the diff of a candidate.
    DiffAware,
    /// From recorded traffic.
    Traffic,
    /// Written or edited by a human.
    Human,
}

/// How a route path was found.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RouteSource {
    /// From the route table and the middleware chain. Labelled static on screen.
    Static,
    /// From a trace of the run.
    Traced,
}

/// The machines a request passes through, in order.
#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct RoutePath {
    /// The machines.
    pub machines: NonEmpty<MachineId>,
    /// How the path was found.
    pub source: RouteSource,
}

/// One probe of the protected corpus.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CorpusProbe {
    /// A short human label, such as `login-rate-limit`.
    pub label: String,
    /// The hashed part.
    pub spec: ProbeSpec,
    /// Why it exists.
    pub origin: ProbeOrigin,
    /// The route path of its main request.
    pub route_path: RoutePath,
    /// How its observations are normalized.
    pub normalization: NormalizationPlan,
}

/// Whether a baseline route has probes.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteStatus {
    /// At least one corpus probe exercises it.
    Covered {},
    /// None does, and the receipt lists it.
    Uncovered {
        /// Why not yet.
        reason: String,
    },
}

/// One method-and-route pattern of the app's route table.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteEntry {
    /// The method.
    pub method: HttpMethod,
    /// The route.
    pub route_template: RouteTemplate,
    /// Its static machine path.
    pub route_path: RoutePath,
    /// Whether it has probes.
    pub status: RouteStatus,
}

/// The protected probe corpus, `.acan/probes/*.json`. Only a human edits it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProbeCorpus {
    /// Always `acan-probes/1`.
    pub version: ProbeCorpusV1,
    /// The fixtures.
    pub fixtures: Vec<Fixture>,
    /// The probes.
    pub probes: Vec<CorpusProbe>,
    /// Every baseline route, covered or not.
    pub routes: Vec<RouteEntry>,
}

/// How the controlled clock is driven.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClockControl {
    /// No controlled clock. An `advance_time` step fails the run's setup.
    None {},
    /// The boot fixture serves a clock endpoint on the app port that only the runner calls.
    HttpEndpoint {
        /// The endpoint path.
        path: RouteTemplate,
    },
}

/// The readiness check the runner polls before the first step.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadyCheck {
    /// The path, such as `/health`.
    pub path: RouteTemplate,
    /// The status that means ready.
    pub status: u16,
    /// How long to wait.
    pub timeout_ms: u64,
}

/// How a run's store is isolated. v1 has one answer.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Debug,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum StoreIsolation {
    /// Every sequence gets a fresh store.
    #[default]
    FreshPerSequence,
}

/// Bounds on one probe sequence. Exceeding one ends the run as an error, never a pass.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunLimits {
    /// Requests per sequence, recipe included.
    pub max_requests: u32,
    /// Response bytes kept per step.
    pub max_output_bytes: u64,
    /// Wall time per sequence, app start included.
    pub sequence_timeout_ms: u64,
}

/// The version of the protected app contract. `acan-app/1` is the bun contract; `acan-app/2` adds
/// the Node 24 runtime and the install phase. A v1 reader refuses a v2 file, so a node app can never
/// run under bun by accident.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, JsonSchema)]
pub enum AppContractVersion {
    /// `acan-app/1`: a bun runtime and no install phase.
    #[serde(rename = "acan-app/1")]
    V1,
    /// `acan-app/2`: a bun or Node 24 runtime, and an optional install phase for Node 24.
    #[serde(rename = "acan-app/2")]
    V2,
}

impl<'de> Deserialize<'de> for AppContractVersion {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "acan-app/1" => Ok(Self::V1),
            "acan-app/2" => Ok(Self::V2),
            other => Err(serde::de::Error::custom(format_args!(
                "unsupported version {other:?}, expected \"acan-app/1\" or \"acan-app/2\""
            ))),
        }
    }
}

/// How to start the app for one run: the protected `.acan/app.json`. Only a human edits it.
///
/// The boot command imports the app from the selected commit, uses a fresh store and the
/// port in `port_env`, and never replaces the candidate's own code. No secret goes in `env`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppContract {
    /// `acan-app/1` or `acan-app/2`.
    pub version: AppContractVersion,
    /// The runner contract it is written against.
    pub runner_contract: RunnerContractV1,
    /// The runtime: `bun-1.4.4`, or `node-24` with `acan-app/2`.
    pub runtime: RuntimeId,
    /// The boot command, as argv, run in the prepared worktree.
    pub boot: NonEmpty<String>,
    /// The install phase, as argv: `npm ci --ignore-scripts ...` for a Node app with a lockfile.
    /// It runs once per lockfile in a container with network, before any run, and its
    /// `node_modules` is mounted read-only into every run. Omitted when the app has no install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install: Option<NonEmpty<String>>,
    /// The environment variable that carries the port.
    pub port_env: String,
    /// The readiness check.
    pub ready: ReadyCheck,
    /// Store isolation.
    pub store: StoreIsolation,
    /// Clock control.
    pub clock: ClockControl,
    /// The trusted sinks the boot fixture exposes.
    pub sinks: Vec<TrustedSink>,
    /// The environment the app gets on top of the policy's. A credential-named variable may carry
    /// only a placeholder, and an app cannot replace a variable the policy sets.
    #[serde(deserialize_with = "crate::container::environment_names")]
    pub env: BTreeMap<String, String>,
    /// Per-sequence bounds.
    pub limits: RunLimits,
}

/// Install argv flags an `install` phase may carry. `--ignore-scripts` is required, so no
/// package's install script runs.
const INSTALL_FLAGS: [&str; 3] = ["--ignore-scripts", "--no-audit", "--no-fund"];

impl AppContract {
    /// Checks what decoding cannot: the runtime and the install phase agree with the version.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] when a v1 contract names a runtime other than bun or has an
    /// install phase, when a v2 runtime is unknown, or when an install argv is not `npm ci` with
    /// the allowed flags and `--ignore-scripts`.
    pub fn check(&self) -> Result<(), InvalidValue> {
        let runtime = self.runtime.as_str();
        let refused = |what: &'static str| InvalidValue::Malformed {
            what,
            raw: runtime.to_owned(),
        };
        match (self.version, runtime, self.install.as_ref()) {
            (AppContractVersion::V1, BUN_RUNTIME, None) => Ok(()),
            (AppContractVersion::V1, BUN_RUNTIME, Some(_)) => {
                Err(refused("install phase, which needs acan-app/2"))
            }
            (AppContractVersion::V1, _, _) => {
                Err(refused("runtime for acan-app/1, which only runs bun-1.4.4"))
            }
            (AppContractVersion::V2, BUN_RUNTIME, None) => Ok(()),
            (AppContractVersion::V2, BUN_RUNTIME, Some(_)) => {
                Err(refused("install phase, which only node-24 has"))
            }
            (AppContractVersion::V2, NODE_RUNTIME, install) => {
                install.map_or(Ok(()), |argv| check_install(argv.as_slice()))
            }
            (AppContractVersion::V2, _, _) => Err(refused("runtime, which acan does not know")),
        }
    }
}

/// Checks an install argv: `npm ci`, then only the allowed flags, with `--ignore-scripts`.
fn check_install(argv: &[String]) -> Result<(), InvalidValue> {
    let well_formed = argv.len() >= 3
        && argv[0] == "npm"
        && argv[1] == "ci"
        && argv[2..]
            .iter()
            .all(|arg| INSTALL_FLAGS.contains(&arg.as_str()))
        && argv.iter().any(|arg| arg == "--ignore-scripts");
    if well_formed {
        Ok(())
    } else {
        Err(InvalidValue::Malformed {
            what: "install argv, which must be npm ci with --ignore-scripts and the allowed flags",
            raw: argv.join(" "),
        })
    }
}

/// Input-only probe steps a model proposed. They carry no expected value.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProbeProposal {
    /// The starting state.
    pub fixture: FixtureId,
    /// The proposed steps.
    pub steps: NonEmpty<Step>,
    /// The route the model meant to exercise.
    pub route_template: RouteTemplate,
    /// Why, in one sentence.
    pub rationale: String,
}

/// Which side of a contrast a run is.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RunRole {
    /// The first base run.
    BaseA,
    /// The second base run, which decides what may be masked.
    BaseB,
    /// The candidate, on the prepared merged commit.
    Candidate,
}

/// The controlled inputs of a run.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunEnv {
    /// The artifact's lockfile, recorded apart from the controlled environment.
    pub lockfile_hash: ArtifactHash,
    /// The fixture.
    pub fixture_hash: FixtureHash,
    /// The runtime.
    pub runtime: RuntimeId,
}

/// How a run ended. Everything but `completed` is an explicit non-pass state.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunOutcome {
    /// Every step ran. Observations may still be incomplete.
    Completed {},
    /// The app did not become ready, or a recipe step failed.
    SetupFailed {
        /// The failing recipe step, if one failed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        step_id: Option<StepId>,
        /// What happened.
        detail: String,
    },
    /// A deadline or the sequence timeout passed.
    TimedOut {
        /// The step that was running.
        step_id: StepId,
    },
    /// The app process exited during the sequence.
    Crashed {
        /// What happened.
        detail: String,
    },
    /// The run was cancelled. The app was stopped.
    Cancelled {},
    /// The container runtime was unavailable. The gate refuses; it never runs unisolated.
    IsolationUnavailable {
        /// What happened.
        detail: String,
    },
    /// Any other runner failure.
    Error {
        /// What happened.
        detail: String,
    },
}

/// Why a capture is missing.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MissingCapture {
    /// The response had no such header, cookie or field.
    Absent,
    /// The value had the wrong type.
    Malformed,
    /// The source step did not complete.
    StepFailed,
}

/// The result of one capture step.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureOutcome {
    /// The value was bound. Only its hash is kept in the run record.
    Captured {
        /// The binding.
        binding: CaptureName,
        /// The capture step.
        step_id: StepId,
        /// The hash of the captured value: SHA-256 over the RFC 8785 canonical JSON of the
        /// extracted JSON value. Not the raw bytes of the value, and not an `Evidence` wrapper.
        /// `fixtures/value-hash-vectors.json` holds the shared vectors every producer must match.
        value_hash: EvidenceHash,
    },
    /// The value could not be bound. Never a pass.
    Missing {
        /// The binding.
        binding: CaptureName,
        /// The capture step.
        step_id: StepId,
        /// Why.
        reason: MissingCapture,
    },
}

impl CaptureOutcome {
    /// The capture step.
    #[must_use]
    pub fn step_id(&self) -> &StepId {
        match self {
            Self::Captured { step_id, .. } | Self::Missing { step_id, .. } => step_id,
        }
    }

    /// The binding.
    #[must_use]
    pub fn binding(&self) -> &CaptureName {
        match self {
            Self::Captured { binding, .. } | Self::Missing { binding, .. } => binding,
        }
    }
}

/// One run of one probe against one commit in one role. Written by the runner, never a model.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// Always `acan-run/1`.
    pub version: RunV1,
    /// The run.
    pub id: RunId,
    /// The probe.
    pub probe: ProbeHash,
    /// The commit.
    pub commit: CommitSha,
    /// The role.
    pub role: RunRole,
    /// The controlled inputs.
    pub env: RunEnv,
    /// The runner build and app contract.
    pub runner: RunnerHash,
    /// How it ended.
    pub outcome: RunOutcome,
    /// One entry per capture step the run reached, in sequence order. See [`crate::admit`].
    pub captures: Vec<CaptureOutcome>,
    /// When the app was started.
    pub started_at: UnixMillis,
    /// When the app was stopped.
    pub finished_at: UnixMillis,
}
