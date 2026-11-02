//! The container policy of every isolated run: the candidate app, every probe sequence, and the
//! worker's test command.
//!
//! Policy rules, binding on every runner:
//! - The command runs as a non-root user on a read-only root filesystem with a bounded `/tmp`,
//!   with no capabilities and no privilege gain, under the CPU, memory and process caps below.
//!   A run is killed and removed when it ends or when its time limit passes: `timeout_ms` for one
//!   probe sequence or test command, `role_timeout_ms` for a long-lived role container.
//! - The worktree is mounted read-write at the working directory. Every protected path is
//!   mounted read-only on top of it from a snapshot of the protected head, never from the
//!   worktree. A protected path the snapshot does not have is still covered as
//!   [`MissingProtected`] says; it is never left writable.
//! - The container has no network and publishes no port. Probes run inside it from the harness
//!   mounted at `mounts.harness`; the host drives them over the container's standard input and
//!   output.
//! - The container gets only the variables `environment` names, with the values it gives them.
//!   No provider key, owner token or host secret enters it, and a name that could carry one is
//!   refused when the policy is decoded, unless its value is a placeholder: a value that starts
//!   with [`PLACEHOLDER_PREFIX`] and carries no secret. An app that reads a credential at boot
//!   gets the placeholder, and the human who owns the app replaces the review marker.
//! - A runner that cannot apply every field refuses the run. It never runs with less isolation.
//!   The harness is a probe-run field: the worker's test command runs no probe and mounts none.
//!
//! The policy's [`SandboxPolicyHash`] is part of the [`crate::GateContext`], so changing any
//! field voids a clearance obtained under the old policy.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::num::{NonZeroU32, NonZeroU64};
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::hash::{CanonicalError, ContentHash, ImageDigest, SandboxPolicyHash};
use crate::types::{ContainerPath, ImageName, InvalidValue, RepoPath};

/// The digest of `oven/bun:1.4.4`, the image the H01 spike ran. Every policy that names this image
/// pins this digest, and the runners refuse an image whose content differs.
const PINNED_IMAGE_DIGEST: &str =
    "sha256:1d22548f177f4ff38e0c15baa3c2c46c6c2060ba2d1bc573bb4720b407548f1d";

/// The runtime of a bun app. Its image is `oven/bun:1.4.4`, and its `node` is bun itself.
pub const BUN_RUNTIME: &str = "bun-1.4.4";
/// The runtime of a Node app, run in the pinned Node 24 image.
pub const NODE_RUNTIME: &str = "node-24";
/// The Node 24 image the node runtime runs in, pinned by digest. Verified on the acan machine:
/// `node` v24.22.2 and npm 11.19.1, running as uid 1000 with no network.
const NODE_IMAGE_NAME: &str = "node:24.22.2-bookworm-slim";
/// The digest of `node:24.22.2-bookworm-slim`.
const NODE_IMAGE_DIGEST: &str =
    "sha256:c86ecead9e55a5210ac1727de38a4ca840bf03dcaf34dc9749addbf383b1920c";
/// The `PATH` the Node image gives its processes. The policy names it, so the controlled
/// environment is the same on every run.
const NODE_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// Where a run mounts the install phase's `node_modules`, inside the worktree.
const DEPENDENCIES_TARGET: &str = "/work/node_modules";
/// The one registry an install fetches packages from. The install names it explicitly, and every
/// `resolved` URL in a lockfile must start with it, so no other source is ever fetched.
pub const NPM_REGISTRY: &str = "https://registry.npmjs.org/";

/// Fragments that mark a variable as a credential. No variable whose name contains one reaches a
/// container.
const CREDENTIAL_NAME_PARTS: [&str; 7] = [
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
];
/// The prefix of a placeholder value. A credential-named variable may carry a placeholder: this
/// prefix, then one or more letters, digits or hyphens. It is never a secret, so an app that reads
/// the variable at boot can start with it. Init writes one for each such variable, marked for a
/// human to review.
pub const PLACEHOLDER_PREFIX: &str = "acan-placeholder-";
/// Longest placeholder text after [`PLACEHOLDER_PREFIX`].
const PLACEHOLDER_TAIL_MAX: usize = 48;
/// Longest variable name accepted.
const ENVIRONMENT_NAME_MAX: usize = 64;
/// Longest variable value accepted, in bytes.
const ENVIRONMENT_VALUE_MAX: usize = 4096;

/// The image a run starts from.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContainerImage {
    /// The name and tag, such as `oven/bun:1.4.4`.
    pub name: ImageName,
    /// The registry digest. When present the runner starts exactly this content and refuses an
    /// image whose digest differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<ImageDigest>,
}

/// The numeric user and group a run executes as, written `uid:gid`. Neither is ever 0, so a run
/// is never root and never in the root group.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ContainerUser {
    /// The user id.
    pub uid: NonZeroU32,
    /// The group id.
    pub gid: NonZeroU32,
}

impl fmt::Display for ContainerUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.uid, self.gid)
    }
}

impl FromStr for ContainerUser {
    type Err = InvalidValue;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // Digits only, no sign and no leading zero, so the text re-encodes to itself.
        let id = |part: &str| {
            let canonical = part.starts_with(|c: char| matches!(c, '1'..='9'))
                && part.bytes().all(|b| b.is_ascii_digit());
            canonical.then(|| part.parse::<NonZeroU32>().ok()).flatten()
        };
        raw.split_once(':')
            .and_then(|(uid, gid)| {
                Some(Self {
                    uid: id(uid)?,
                    gid: id(gid)?,
                })
            })
            .ok_or_else(|| InvalidValue::Malformed {
                what: "container user",
                raw: raw.to_owned(),
            })
    }
}

impl Serialize for ContainerUser {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContainerUser {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ContainerUser {
    fn schema_name() -> Cow<'static, str> {
        "ContainerUser".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[1-9][0-9]*:[1-9][0-9]*$",
            "description": "uid:gid, neither of them 0"
        })
    }
}

/// What a run's container may reach. Closed: the container has no network at all, so nothing is
/// published and probes run inside it.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContainerNetwork {
    /// No network at all. Probes and the test command run this way.
    None {},
    /// Outbound network through the Docker bridge. Only the install phase of a node app has it, to
    /// fetch its dependencies. A probe run that names it is refused.
    Egress {},
}

/// The program that runs the probe driver inside the container, with its file in the harness.
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
pub enum Driver {
    /// `bun` runs `driver.ts`. The default, with the pinned bun image.
    #[default]
    Bun,
    /// `node` runs `driver.mjs`, with the pinned Node 24 image.
    Node,
}

impl Driver {
    /// Whether this is the default driver, which the policy's serialized form leaves out.
    fn is_default(&self) -> bool {
        *self == Self::Bun
    }
}

/// The container's root filesystem. Only `/tmp` and the worktree are writable.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RootFilesystem {
    /// Mounted read-only.
    ReadOnly,
}

/// The privileges the run keeps.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Privileges {
    /// Every capability dropped, and no privilege gain through setuid or file capabilities.
    Dropped,
}

/// How a protected path the protected-head snapshot does not have is covered.
#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum MissingProtected {
    /// An empty read-only mount covers the path, so the run can neither read a copy the
    /// candidate added nor create one.
    Masked,
    /// The run is refused.
    Refused,
}

/// What is mounted where.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MountPolicy {
    /// Where the worktree is mounted read-write. Also the working directory.
    pub workdir: ContainerPath,
    /// The worktree paths mounted read-only from the protected-head snapshot over the worktree:
    /// `.acan` and the protected tests.
    pub protected: BTreeSet<RepoPath>,
    /// How a protected path missing from the snapshot is covered.
    pub missing_protected: MissingProtected,
    /// Whether `.git` is mounted read-only over the worktree. Hooks written there would run on
    /// the host at the next commit, so a policy keeps it read-only unless it says otherwise.
    #[serde(default = "readonly_git_by_default")]
    pub readonly_git: bool,
    /// Where the probe driver's directory is mounted read-only. The driver runs inside the
    /// container and the host drives it over standard input. Only probe runs mount it.
    #[serde(default = "harness_by_default")]
    pub harness: ContainerPath,
    /// Where the install phase's `node_modules` is mounted read-only, for a node app with an
    /// install. The host mounts the cache entry for the run's lockfile there. Omitted when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dependencies: Option<ContainerPath>,
}

/// The default for [`MountPolicy::readonly_git`]: `.git` is mounted read-only.
const fn readonly_git_by_default() -> bool {
    true
}

/// The default for [`MountPolicy::harness`]: the probe driver is mounted at `/harness`.
fn harness_by_default() -> ContainerPath {
    "/harness"
        .parse()
        .expect("the default harness path is a container path")
}

/// The default for [`ContainerPolicy::role_timeout_ms`]: one hour.
fn role_timeout_by_default() -> NonZeroU64 {
    nonzero64(3_600_000)
}

/// The default for [`ContainerPolicy::environment`]: only `HOME`, which is the writable `/tmp`.
fn environment_by_default() -> BTreeMap<String, String> {
    BTreeMap::from([("HOME".to_owned(), "/tmp".to_owned())])
}

/// Decodes the environment and refuses any variable a container must not receive.
pub(crate) fn environment_names<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    let environment = BTreeMap::<String, String>::deserialize(deserializer)?;
    for (name, value) in &environment {
        check_variable(name, value).map_err(serde::de::Error::custom)?;
    }
    Ok(environment)
}

/// Whether a variable name carries a credential: it contains one of the credential fragments.
#[must_use]
pub fn is_credential_name(name: &str) -> bool {
    CREDENTIAL_NAME_PARTS.iter().any(|part| name.contains(part))
}

/// Whether `value` is a placeholder: [`PLACEHOLDER_PREFIX`] and then letters, digits or hyphens.
fn is_placeholder(value: &str) -> bool {
    value.strip_prefix(PLACEHOLDER_PREFIX).is_some_and(|tail| {
        !tail.is_empty()
            && tail.len() <= PLACEHOLDER_TAIL_MAX
            && tail
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// Refuses a variable whose name is not `A-Z 0-9 _` from an uppercase letter, whose name could
/// carry a credential unless its value is a placeholder, or whose value is not short printable
/// text. The refusal names the variable and never its value.
fn check_variable(name: &str, value: &str) -> Result<(), InvalidValue> {
    let well_formed = name.len() <= ENVIRONMENT_NAME_MAX
        && name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if !well_formed {
        return Err(InvalidValue::Malformed {
            what: "environment variable name",
            raw: name.to_owned(),
        });
    }
    if is_credential_name(name) && !is_placeholder(value) {
        return Err(InvalidValue::Malformed {
            what: "credential-named environment variable, which no container may receive unless \
                   its value is a placeholder",
            raw: name.to_owned(),
        });
    }
    if value.len() > ENVIRONMENT_VALUE_MAX || value.chars().any(char::is_control) {
        return Err(InvalidValue::Malformed {
            what: "environment variable value, which must be printable text",
            raw: name.to_owned(),
        });
    }
    Ok(())
}

/// The container policy of every isolated run. Its hash is the [`SandboxPolicyHash`].
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContainerPolicy {
    /// The image.
    pub image: ContainerImage,
    /// Who the command runs as.
    pub user: ContainerUser,
    /// CPU cap, in thousandths of a core.
    pub cpus_milli: NonZeroU32,
    /// Memory cap. Swap is not allowed beyond it.
    pub memory_bytes: NonZeroU64,
    /// Process cap.
    pub pids: NonZeroU32,
    /// Wall-clock cap of one probe sequence or one test command, from container start. At it
    /// the runner kills and removes the container.
    pub timeout_ms: NonZeroU64,
    /// Wall-clock cap of a long-lived role container. It is separate from `timeout_ms`, so a
    /// role container is not cut at the sequence cap.
    #[serde(default = "role_timeout_by_default")]
    pub role_timeout_ms: NonZeroU64,
    /// Size of the writable `/tmp`, which is also `HOME`.
    pub tmp_bytes: NonZeroU64,
    /// The environment variables the container gets, by name, with their values. A variable not
    /// listed never reaches the container.
    #[serde(
        default = "environment_by_default",
        deserialize_with = "environment_names"
    )]
    pub environment: BTreeMap<String, String>,
    /// What the container may reach.
    pub network: ContainerNetwork,
    /// The root filesystem.
    pub root: RootFilesystem,
    /// The privileges kept.
    pub privileges: Privileges,
    /// The mounts.
    pub mounts: MountPolicy,
    /// The program that runs the probe driver. Omitted when it is the default, `bun`.
    #[serde(default, skip_serializing_if = "Driver::is_default")]
    pub driver: Driver,
}

impl ContainerPolicy {
    /// The policy a run starts under when none is named: the contract fixture
    /// `container-policy.json`, which the worker's test command also runs under. The contract
    /// test `the_fixture_is_the_default_run` holds the two equal.
    #[must_use]
    pub fn default_run() -> Self {
        Self {
            image: ContainerImage {
                name: "oven/bun:1.4.4"
                    .parse()
                    .expect("the pinned image name is valid"),
                digest: Some(
                    PINNED_IMAGE_DIGEST
                        .parse()
                        .expect("the pinned image digest is valid"),
                ),
            },
            user: "1000:1000".parse().expect("the run user is valid"),
            cpus_milli: nonzero32(2000),
            memory_bytes: nonzero64(1 << 30),
            pids: nonzero32(256),
            timeout_ms: nonzero64(30_000),
            role_timeout_ms: role_timeout_by_default(),
            tmp_bytes: nonzero64(256 << 20),
            environment: BTreeMap::from([
                ("HOME".to_owned(), "/tmp".to_owned()),
                ("PORT".to_owned(), "3000".to_owned()),
            ]),
            network: ContainerNetwork::None {},
            root: RootFilesystem::ReadOnly,
            privileges: Privileges::Dropped,
            mounts: MountPolicy {
                workdir: "/work".parse().expect("the workdir is a container path"),
                protected: BTreeSet::from([".acan".parse().expect("a repo path")]),
                missing_protected: MissingProtected::Masked,
                readonly_git: true,
                harness: harness_by_default(),
                dependencies: None,
            },
            driver: Driver::Bun,
        }
    }

    /// The policy a Node 24 app's runs apply: the pinned Node image, the Node driver, and the
    /// install's `node_modules` mounted read-only when `with_dependencies` is set. Network none.
    #[must_use]
    pub fn node_run(with_dependencies: bool) -> Self {
        let mut policy = Self::node_base();
        if with_dependencies {
            policy.mounts.dependencies = Some(
                DEPENDENCIES_TARGET
                    .parse()
                    .expect("the dependency target is a container path"),
            );
        }
        policy
    }

    /// The policy of a node app's install phase: the pinned Node image with network egress, and
    /// no protected paths, because the install reads manifests only. Install scripts never run.
    #[must_use]
    pub fn node_install() -> Self {
        let mut policy = Self::node_base();
        policy.network = ContainerNetwork::Egress {};
        policy.timeout_ms = nonzero64(600_000);
        policy.tmp_bytes = nonzero64(1 << 30);
        policy.environment = BTreeMap::from([
            ("HOME".to_owned(), "/tmp".to_owned()),
            ("NPM_CONFIG_GLOBALCONFIG".to_owned(), "/dev/null".to_owned()),
            ("NPM_CONFIG_REGISTRY".to_owned(), NPM_REGISTRY.to_owned()),
            ("NPM_CONFIG_USERCONFIG".to_owned(), "/dev/null".to_owned()),
            ("PATH".to_owned(), NODE_PATH.to_owned()),
        ]);
        policy.mounts.protected = BTreeSet::new();
        policy
    }

    /// The policy every run of an app applies, chosen by its runtime and whether it has an install.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] when the runtime has no policy, or when a bun app names an install,
    /// which only node apps have.
    pub fn for_runtime(runtime: &str, install: bool) -> Result<Self, InvalidValue> {
        match runtime {
            BUN_RUNTIME if !install => Ok(Self::default_run()),
            NODE_RUNTIME => Ok(Self::node_run(install)),
            _ => Err(InvalidValue::Malformed {
                what: "runtime, which has no run policy (or an install for a bun app)",
                raw: runtime.to_owned(),
            }),
        }
    }

    /// The shared Node 24 base: the pinned image, the Node driver, and the environment with `PATH`
    /// and the port named.
    fn node_base() -> Self {
        let mut policy = Self::default_run();
        policy.image = ContainerImage {
            name: NODE_IMAGE_NAME
                .parse()
                .expect("the pinned node image name is valid"),
            digest: Some(
                NODE_IMAGE_DIGEST
                    .parse()
                    .expect("the pinned node image digest is valid"),
            ),
        };
        policy.memory_bytes = nonzero64(2 << 30);
        policy.environment = BTreeMap::from([
            ("HOME".to_owned(), "/tmp".to_owned()),
            ("PATH".to_owned(), NODE_PATH.to_owned()),
            ("PORT".to_owned(), "3000".to_owned()),
        ]);
        policy.driver = Driver::Node;
        policy
    }

    /// Refuses an environment a container must not get. The decoder applies the same rule, so a
    /// policy built in code is checked before any runner passes its variables on.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] naming the first variable that fails.
    pub fn check_environment(&self) -> Result<(), InvalidValue> {
        self.environment
            .iter()
            .try_for_each(|(name, value)| check_variable(name, value))
    }

    /// Adds the variables an app's contract names to the policy's environment, so every run gives
    /// them to the app. The same rules apply, and a placeholder is the only secret-shaped value
    /// allowed.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidValue`] when a variable is refused, or when the policy already sets it. An
    /// app cannot replace a variable the policy owns, such as `HOME`, `PATH` or the port.
    pub fn with_app_environment(
        &mut self,
        app: &BTreeMap<String, String>,
    ) -> Result<(), InvalidValue> {
        check_app_environment(app)?;
        for (name, value) in app {
            if self.environment.contains_key(name) {
                return Err(InvalidValue::Malformed {
                    what: "app environment variable, which the policy already sets",
                    raw: name.clone(),
                });
            }
            self.environment.insert(name.clone(), value.clone());
        }
        self.check_environment()
    }

    /// The policy's identity, carried in [`crate::GateContext::sandbox_policy_hash`].
    ///
    /// # Errors
    ///
    /// Returns [`CanonicalError`] only if serialization fails.
    pub fn sandbox_policy_hash(&self) -> Result<SandboxPolicyHash, CanonicalError> {
        ContentHash::of(self).map(SandboxPolicyHash::from)
    }
}

/// Refuses an app's environment a container must not get. Each entry passes the rule a policy
/// variable passes, so the decoder and the runners agree on it.
///
/// # Errors
///
/// Returns [`InvalidValue`] naming the first variable that fails.
pub fn check_app_environment(environment: &BTreeMap<String, String>) -> Result<(), InvalidValue> {
    environment
        .iter()
        .try_for_each(|(name, value)| check_variable(name, value))
}

fn nonzero32(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("a container limit is never zero")
}

fn nonzero64(n: u64) -> NonZeroU64 {
    NonZeroU64::new(n).expect("a container limit is never zero")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn fixture() -> ContainerPolicy {
        serde_json::from_str(include_str!("../fixtures/container-policy.json"))
            .expect("the policy fixture")
    }

    fn with(edit: impl FnOnce(&mut Value)) -> bool {
        let mut value: Value =
            serde_json::from_str(include_str!("../fixtures/container-policy.json")).expect("JSON");
        edit(&mut value);
        serde_json::from_value::<ContainerPolicy>(value).is_ok()
    }

    #[test]
    fn every_policy_field_changes_the_hash() {
        let base = fixture();
        // No `..`: a new field breaks this pattern, so its author has to add an edit below.
        let ContainerPolicy {
            image: _,
            user: _,
            cpus_milli: _,
            memory_bytes: _,
            pids: _,
            timeout_ms: _,
            role_timeout_ms: _,
            tmp_bytes: _,
            environment: _,
            network: _,
            root: _,
            privileges: _,
            mounts: _,
            driver: _,
        } = &base;
        type Edit = fn(&mut ContainerPolicy);
        let edits: [(&str, Edit); 20] = [
            ("image name", |p| {
                p.image.name = "oven/bun:2".parse().expect("a name");
            }),
            ("image digest", |p| p.image.digest = None),
            ("uid", |p| p.user.uid = nonzero32(1001)),
            ("gid", |p| p.user.gid = nonzero32(1001)),
            ("cpus", |p| p.cpus_milli = nonzero32(1000)),
            ("memory", |p| p.memory_bytes = nonzero64(512 << 20)),
            ("pids", |p| p.pids = nonzero32(128)),
            ("timeout", |p| p.timeout_ms = nonzero64(60_000)),
            ("role timeout", |p| p.role_timeout_ms = nonzero64(7_200_000)),
            ("tmp", |p| p.tmp_bytes = nonzero64(64 << 20)),
            ("an added variable", |p| {
                p.environment.insert("TZ".to_owned(), "UTC".to_owned());
            }),
            ("a variable's value", |p| {
                p.environment
                    .insert("HOME".to_owned(), "/var/tmp".to_owned());
            }),
            ("workdir", |p| {
                p.mounts.workdir = "/app".parse().expect("a path");
            }),
            ("protected paths", |p| {
                p.mounts
                    .protected
                    .insert("src/limits.ts".parse().expect("a path"));
            }),
            ("missing protected", |p| {
                p.mounts.missing_protected = MissingProtected::Refused;
            }),
            ("readonly git", |p| p.mounts.readonly_git = false),
            ("harness", |p| {
                p.mounts.harness = "/sandbox".parse().expect("a path");
            }),
            ("network egress", |p| {
                p.network = ContainerNetwork::Egress {}
            }),
            ("dependencies", |p| {
                p.mounts.dependencies = Some("/work/node_modules".parse().expect("a path"));
            }),
            ("driver", |p| p.driver = Driver::Node),
        ];
        let original = base.sandbox_policy_hash().expect("hashing");
        let mut seen = BTreeSet::from([original]);
        for (field, edit) in &edits {
            let mut changed = base.clone();
            edit(&mut changed);
            let hash = changed.sandbox_policy_hash().expect("hashing");
            assert!(seen.insert(hash), "changing the {field} left a known hash");
        }
    }

    #[test]
    fn a_run_is_never_root() {
        assert!(with(|_| {}));
        for user in [
            "0:1000",
            "1000:0",
            "root",
            "1000",
            "+1000:1000",
            "01000:1000",
            "1000:1000:1",
            "-1:1",
            "99999999999:1",
        ] {
            assert!(!with(|v| v["user"] = json!(user)), "{user} decoded");
        }
        let user: ContainerUser = "1000:1000".parse().expect("a user");
        assert_eq!(user.to_string(), "1000:1000");
    }

    #[test]
    fn image_names_cannot_read_as_flags_or_carry_a_digest() {
        for name in [
            "",
            "-v",
            "--privileged",
            "bun latest",
            "oven/bun@sha256:00",
            "a,b",
        ] {
            assert!(
                !with(|v| v["image"]["name"] = json!(name)),
                "{name:?} decoded"
            );
        }
        assert!(!with(|v| v["image"]["digest"] = json!("sha256:short")));
        assert!(with(|v| {
            v["image"]
                .as_object_mut()
                .expect("an image")
                .remove("digest");
        }));
    }

    #[test]
    fn caps_and_paths_are_bounded() {
        for cap in [
            "cpus_milli",
            "memory_bytes",
            "pids",
            "timeout_ms",
            "role_timeout_ms",
            "tmp_bytes",
        ] {
            assert!(!with(|v| v[cap] = json!(0)), "a zero {cap} decoded");
        }
        for workdir in [
            "work",
            "/",
            "/work/..",
            "/work/./x",
            "/work dir",
            "/work//x",
        ] {
            assert!(
                !with(|v| v["mounts"]["workdir"] = json!(workdir)),
                "{workdir} decoded"
            );
        }
        for harness in ["harness", "/harness/..", "/harness dir"] {
            assert!(
                !with(|v| v["mounts"]["harness"] = json!(harness)),
                "{harness} decoded"
            );
        }
        assert!(!with(|v| v["mounts"]["protected"] = json!(["../outside"])));
        assert!(!with(
            |v| v["mounts"]["missing_protected"] = json!("writable")
        ));
        assert!(!with(|v| v["mounts"]["readonly_git"] = json!("no")));
    }

    #[test]
    fn environment_names_refuse_credentials_and_control_text() {
        assert!(with(
            |v| v["environment"] = json!({ "HOME": "/tmp", "PORT": "3000" })
        ));
        assert!(with(|v| v["environment"] = json!({})));
        for name in [
            "OPENAI_API_KEY",
            "META_TOKEN",
            "DB_PASSWORD",
            "GITHUB_SECRET",
            "AUTH_HEADER",
            "my_var",
            "1PORT",
            "PORT=3000",
            "A B",
            "",
        ] {
            assert!(
                !with(|v| v["environment"] = json!({ name: "x" })),
                "{name:?} decoded"
            );
        }
        for value in ["3000\n", "a\u{7}b"] {
            assert!(
                !with(|v| v["environment"] = json!({ "PORT": value })),
                "{value:?} decoded"
            );
        }
    }

    #[test]
    fn a_missing_environment_or_harness_takes_the_probe_defaults() {
        let mut absent: Value =
            serde_json::from_str(include_str!("../fixtures/container-policy.json")).expect("JSON");
        absent
            .as_object_mut()
            .expect("a policy")
            .remove("environment");
        absent["mounts"]
            .as_object_mut()
            .expect("mounts")
            .remove("harness");
        absent
            .as_object_mut()
            .expect("a policy")
            .remove("role_timeout_ms");
        let decoded: ContainerPolicy = serde_json::from_value(absent).expect("the policy decodes");
        assert_eq!(
            decoded.environment,
            BTreeMap::from([("HOME".to_owned(), "/tmp".to_owned())]),
            "only HOME when the policy names nothing"
        );
        assert_eq!(decoded.mounts.harness.as_str(), "/harness");
        assert_eq!(decoded.role_timeout_ms, nonzero64(3_600_000));
    }

    #[test]
    fn readonly_git_defaults_to_true_and_is_hashed() {
        let base = fixture();
        assert!(base.mounts.readonly_git);
        let mut absent: Value =
            serde_json::from_str(include_str!("../fixtures/container-policy.json")).expect("JSON");
        absent["mounts"]
            .as_object_mut()
            .expect("mounts")
            .remove("readonly_git");
        let decoded: ContainerPolicy = serde_json::from_value(absent).expect("the policy decodes");
        assert_eq!(decoded, base, "an absent readonly_git is true");
        let mut opened = base.clone();
        opened.mounts.readonly_git = false;
        assert_ne!(
            opened.sandbox_policy_hash().expect("hashing"),
            base.sandbox_policy_hash().expect("hashing"),
            "a writable .git kept the policy hash"
        );
    }

    #[test]
    fn the_network_is_closed() {
        assert!(with(|v| v["network"] = json!({ "kind": "none" })));
        assert!(!with(
            |v| v["network"] = json!({ "kind": "none", "port": 3000 })
        ));
        assert!(!with(|v| v["network"] = json!({ "kind": "host" })));
        assert!(!with(
            |v| v["network"] = json!({ "kind": "loopback_published", "port": 3000 })
        ));
        assert!(!with(|v| v["root"] = json!("read_write")));
    }

    #[test]
    fn a_placeholder_may_carry_a_credential_name_and_nothing_else_may() {
        assert!(with(|v| v["environment"] = json!({
            "PAYPAL_CLIENT_SECRET": "acan-placeholder-paypal-client-secret",
            "PAYPAL_CLIENT_ID": "acan-placeholder-paypal-client-id",
        })));
        for value in [
            "live-value-without-the-marker",
            "acan-placeholder-",
            "acan-placeholder-has space",
            "acan-placeholder-x_y",
            "acan-placeholder-x\ny",
            "placeholder",
            "xacan-placeholder-x",
        ] {
            assert!(
                !with(|v| v["environment"] = json!({ "DB_PASSWORD": value })),
                "a credential-named value {value:?} decoded"
            );
        }
        let placeholder = "acan-placeholder-x".to_owned();
        assert!(
            check_app_environment(&BTreeMap::from([(
                "OPENAI_API_KEY".to_owned(),
                placeholder,
            )]))
            .is_ok()
        );
    }

    #[test]
    fn an_app_adds_its_environment_but_not_the_policys_own_variables() {
        let mut policy = ContainerPolicy::default_run();
        let app = BTreeMap::from([
            ("ACQUIT_DEV".to_owned(), "1".to_owned()),
            (
                "PAYPAL_CLIENT_SECRET".to_owned(),
                "acan-placeholder-paypal-client-secret".to_owned(),
            ),
        ]);
        policy
            .with_app_environment(&app)
            .expect("the app adds its variables");
        assert_eq!(
            policy.environment.get("ACQUIT_DEV").map(String::as_str),
            Some("1")
        );
        let mut replaces_home = ContainerPolicy::default_run();
        let refused = BTreeMap::from([("HOME".to_owned(), "/srv/app".to_owned())]);
        assert!(replaces_home.with_app_environment(&refused).is_err());
        let mut leaks = ContainerPolicy::default_run();
        let secret = BTreeMap::from([("GITHUB_TOKEN".to_owned(), "real-value".to_owned())]);
        assert!(
            leaks.with_app_environment(&secret).is_err(),
            "a real token was added"
        );
    }

    #[test]
    fn the_default_run_pins_the_digest_the_spike_ran() {
        let image = ContainerPolicy::default_run().image;
        assert_eq!(image.name.as_str(), "oven/bun:1.4.4");
        assert_eq!(
            image.digest.map(|digest| digest.to_string()).as_deref(),
            Some(PINNED_IMAGE_DIGEST)
        );
    }
}
