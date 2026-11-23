//! The twin console's capped probe endpoint: a human edits one corpus probe and runs it once on
//! fresh base and candidate instances. API01 implements it; the UI's form decodes these shapes.
//!
//! Rules:
//! - Selected app only. The endpoint runs the receipt's order's app, under the protected app
//!   contract, on the receipt's base and tested merged commits. No request names a command, a
//!   host, a port, a commit or an environment variable.
//! - The form is built from the probe's step schema. v1 edits are a repeat count and a JSON body
//!   per HTTP step, nothing else.
//! - Hard caps ([`TWIN_CAPS`]) on steps after expansion, requests, wall time, kept output and
//!   body size. A request over a cap is refused with [`TwinErrorCode::CapExceeded`] before
//!   anything runs; a run that hits one at run time ends as an error outcome, never a pass.
//! - Every run is a fresh instance with a fresh store ([`crate::Isolation::FreshInstance`]).
//! - Running never mutates the corpus, the ledger, a receipt or an episode. Twin runs are not
//!   corpus runs and never reach a receipt.
//! - Saving is a separate human action ([`TwinSaveRequest`]) that adds a probe with
//!   `origin: human` to the protected corpus. An agent token is refused.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash::{ProbeHash, ReceiptHash};
use crate::observation::Observation;
use crate::probe::{CorpusProbe, ProbeOrigin, Run, Step};
use crate::types::{InvalidValue, MachineId, NonEmpty, StepId, bounded};
use crate::version::TwinV1;

/// The hard caps of one twin run, per side.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TwinCaps {
    /// Steps after repeats are expanded, recipe included.
    pub max_steps: u32,
    /// Requests sent, recipe included.
    pub max_requests: u32,
    /// Wall time, app start included.
    pub max_wall_ms: u64,
    /// Response bytes kept per step.
    pub max_output_bytes: u64,
    /// Bytes of one edited JSON body, as canonical JSON.
    pub max_body_bytes: u64,
}

/// The v1 caps. Frozen: a higher cap is a new version.
pub const TWIN_CAPS: TwinCaps = TwinCaps {
    max_steps: 32,
    max_requests: 64,
    max_wall_ms: 30_000,
    max_output_bytes: 65_536,
    max_body_bytes: 16_384,
};

bounded!(
    /// How many times one HTTP step is sent in a row: 1 to 20.
    RepeatCount(u32),
    1..=20,
    "twin repeat count"
);

/// One edit to one HTTP step of the source probe.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TwinEdit {
    /// The HTTP step.
    pub step_id: StepId,
    /// How many times it is sent.
    pub repeat: RepeatCount,
    /// A replacement JSON body. Absent keeps the probe's; JSON `null` sends `null`.
    #[serde(
        default,
        deserialize_with = "crate::types::present_json",
        skip_serializing_if = "Option::is_none"
    )]
    pub body: Option<Value>,
}

/// Run an edited corpus probe on fresh base and candidate instances.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TwinRunRequest {
    /// Always `acan-twin/1`.
    pub version: TwinV1,
    /// The receipt whose base and merged commits run. A receipt that is no longer the order's
    /// latest is refused as stale.
    pub receipt_hash: ReceiptHash,
    /// The machine the console was opened on.
    pub machine_id: MachineId,
    /// The corpus probe the form was built from.
    pub probe_hash: ProbeHash,
    /// The edits, at most one per step.
    pub edits: Vec<TwinEdit>,
}

/// One side of a twin run: a fresh instance's run record and observations.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TwinRun {
    /// The run. Its `probe` is the hash of the edited spec, not a corpus probe.
    pub run: Run,
    /// Its observations, recipe steps first. An incomplete one is never a pass.
    pub observations: Vec<Observation>,
}

/// The reply to a twin run.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TwinRunResponse {
    /// Always `acan-twin/1`.
    pub version: TwinV1,
    /// The receipt.
    pub receipt_hash: ReceiptHash,
    /// The source probe.
    pub source: ProbeHash,
    /// The edited spec's hash.
    pub probe_hash: ProbeHash,
    /// The steps that ran, repeats expanded.
    pub steps: NonEmpty<Step>,
    /// The caps applied.
    pub caps: TwinCaps,
    /// The fresh base instance.
    pub base: TwinRun,
    /// The fresh candidate instance.
    pub candidate: TwinRun,
}

/// Save an edited sequence as a new corpus probe. A human action only.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TwinSaveRequest {
    /// Always `acan-twin/1`.
    pub version: TwinV1,
    /// The receipt the console was opened from.
    pub receipt_hash: ReceiptHash,
    /// The source probe.
    pub source: ProbeHash,
    /// The edits that make the new probe.
    pub edits: Vec<TwinEdit>,
    /// The new probe's label.
    pub label: String,
}

/// The reply to a save: the probe added to the protected corpus, always `origin: human`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "TwinSaveResponseWire")]
pub struct TwinSaveResponse {
    /// Always `acan-twin/1`.
    pub version: TwinV1,
    /// The new probe's hash.
    pub probe_hash: ProbeHash,
    /// The new probe. Decoding refuses any origin but `human`.
    pub probe: CorpusProbe,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "TwinSaveResponse")]
struct TwinSaveResponseWire {
    version: TwinV1,
    probe_hash: ProbeHash,
    probe: CorpusProbe,
}

impl TryFrom<TwinSaveResponseWire> for TwinSaveResponse {
    type Error = InvalidValue;

    fn try_from(wire: TwinSaveResponseWire) -> Result<Self, Self::Error> {
        if wire.probe.origin != ProbeOrigin::Human {
            return Err(InvalidValue::Malformed {
                what: "saved twin probe",
                raw: format!("origin {:?}", wire.probe.origin),
            });
        }
        Ok(Self {
            version: wire.version,
            probe_hash: wire.probe_hash,
            probe: wire.probe,
        })
    }
}

/// Which cap a request or run exceeded.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TwinCap {
    /// [`TwinCaps::max_steps`].
    Steps,
    /// [`TwinCaps::max_requests`].
    Requests,
    /// [`TwinCaps::max_wall_ms`].
    WallTime,
    /// [`TwinCaps::max_output_bytes`].
    OutputBytes,
    /// [`TwinCaps::max_body_bytes`].
    BodyBytes,
}

/// Why a twin request failed.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "code", rename_all = "snake_case", deny_unknown_fields)]
pub enum TwinErrorCode {
    /// No valid token.
    Unauthorized {},
    /// The caller may not do this. Agent tokens never save.
    Forbidden {
        /// The refused action.
        action: String,
    },
    /// The receipt, machine, probe or step does not exist.
    NotFound {
        /// What was missing.
        what: String,
    },
    /// The request did not decode or failed validation.
    InvalidRequest {
        /// What was wrong.
        detail: String,
    },
    /// The receipt is no longer the order's latest.
    StaleReceipt {
        /// The latest receipt.
        current: ReceiptHash,
    },
    /// The edit targets a step the form does not edit, such as a capture step.
    NotEditable {
        /// The step.
        step_id: StepId,
    },
    /// A cap was exceeded. Nothing ran.
    CapExceeded {
        /// The cap.
        cap: TwinCap,
        /// Its limit.
        limit: u64,
        /// What the request needed.
        requested: u64,
    },
    /// Another twin run is in progress for this order.
    Busy {},
    /// The container runtime was unavailable. The twin never runs unisolated.
    IsolationUnavailable {
        /// What happened.
        detail: String,
    },
    /// The operation is not available in this build.
    Unsupported {
        /// The operation, such as `API01: twin_run`.
        operation: String,
    },
}

/// The error envelope of every twin request.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TwinError {
    /// Always `acan-twin/1`.
    pub version: TwinV1,
    /// Why.
    pub error: TwinErrorCode,
    /// One line for the console.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn request() -> Value {
        serde_json::from_str(include_str!("../fixtures/twin-run-request.json")).expect("fixture")
    }

    #[test]
    fn repeat_counts_are_capped_at_decode() {
        for (repeat, ok) in [(0, false), (1, true), (20, true), (21, false)] {
            let mut value = request();
            value["edits"][0]["repeat"] = json!(repeat);
            assert_eq!(
                serde_json::from_value::<TwinRunRequest>(value).is_ok(),
                ok,
                "repeat {repeat}"
            );
        }
    }

    #[test]
    fn a_run_request_cannot_select_another_app_or_save() {
        for field in ["commit", "command", "base_url", "save", "env"] {
            let mut value = request();
            value[field] = json!("x");
            assert!(
                serde_json::from_value::<TwinRunRequest>(value).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn a_saved_probe_is_always_human() {
        let saved: Value =
            serde_json::from_str(include_str!("../fixtures/twin-save-response.json"))
                .expect("fixture");
        assert!(serde_json::from_value::<TwinSaveResponse>(saved.clone()).is_ok());
        let mut laundered = saved;
        laundered["probe"]["origin"] = json!("traffic");
        assert!(serde_json::from_value::<TwinSaveResponse>(laundered).is_err());
    }

    #[test]
    fn the_response_fixture_echoes_the_frozen_caps() {
        let response: TwinRunResponse =
            serde_json::from_str(include_str!("../fixtures/twin-run-response.json"))
                .expect("fixture");
        assert_eq!(response.caps, TWIN_CAPS);
        assert!(response.steps.len() <= TWIN_CAPS.max_steps as usize);
    }
}
