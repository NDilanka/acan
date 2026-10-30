//! Shared setup for the H01 runtime tests.
//!
//! Each [`Setup`] checks the pinned acan-demo commit out with `git archive` into its own
//! directories, so a run never shares a worktree with another run or with the demo checkout.
//! The demo checkout is only read. Its location comes from the `ACAN_DEMO_DIR` variable.

#![expect(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use acan_intent::{
    AppContract, ArtifactHash, CommitSha, ContainerPolicy, ContentHash, CorpusProbe, Fixture,
    ProbeCorpus, RunId, RunRole,
};
use acan_witness::{RunnerConfig, SequenceInput};
use serde_json::Value;
use tempfile::TempDir;

/// The acan-demo commit the base runs check out.
pub const DEMO_COMMIT: &str = "97f6a0102dbda9ea01f283505d2c18fed0236c81";

/// The commit label the candidate runs carry. No git object needs to exist for it: the runner
/// records the label, and compare only requires the two base runs to share a commit.
pub const CANDIDATE_COMMIT: &str = "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00";

/// The port the demo app listens on inside its own container.
pub const APP_PORT: u16 = 3000;

/// The environment variable that names the acan-demo checkout. The tests read it and never write
/// to it.
const DEMO_DIR_VAR: &str = "ACAN_DEMO_DIR";

/// Serializes the Docker tests of one test binary. Tests in a binary run in parallel by default,
/// so without this lock a run that is still starting can spend its 30 second budget while the
/// others hold containers. One test at a time keeps each run inside the budget.
pub static DOCKER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The signup response line the changed candidate rewrites: the username in the response is
/// upper-cased, and the stored user is not, so only the response differs.
const SIGNUP_RESPONSE: &str = "return { id: user.id, username: user.username };";
const CHANGED_SIGNUP_RESPONSE: &str =
    "return { id: user.id, username: user.username.toUpperCase() };";

/// The container policy the tests run under: the contract default, which pins the image digest
/// the spike ran and names the demo's variables. Its sequence budget is the spike's 30 seconds.
const POLICY: &str = r#"{
  "image": {
    "name": "oven/bun:1.4.4",
    "digest": "sha256:1d22548f177f4ff38e0c15baa3c2c46c6c2060ba2d1bc573bb4720b407548f1d"
  },
  "user": "1000:1000",
  "cpus_milli": 2000,
  "memory_bytes": 1073741824,
  "pids": 256,
  "timeout_ms": 30000,
  "role_timeout_ms": 3600000,
  "tmp_bytes": 268435456,
  "environment": { "HOME": "/tmp", "PORT": "3000" },
  "network": { "kind": "none" },
  "root": "read_only",
  "privileges": "dropped",
  "mounts": {
    "workdir": "/work",
    "protected": [".acan"],
    "missing_protected": "refused"
  }
}"#;

/// One checkout of the demo app and everything a run reads from it.
pub struct Setup {
    _dir: TempDir,
    /// The worktree the container mounts read-write at `/work`.
    pub worktree: PathBuf,
    /// The protected `.acan` directory the container mounts read-only.
    pub snapshot: PathBuf,
    /// The commit the run records.
    pub commit: &'static str,
    /// The probe corpus from `.acan/probes/core.json`.
    pub corpus: ProbeCorpus,
    /// The app contract from `.acan/app.json`. Tests may change its limits.
    pub app: AppContract,
    /// The container policy.
    pub policy: ContainerPolicy,
    /// The hash of the lockfile the app was installed from.
    pub lockfile: ArtifactHash,
}

impl Setup {
    /// The protected head, as base A and base B run it.
    pub fn base() -> Self {
        Self::checkout(DEMO_COMMIT, Change::None)
    }

    /// The candidate with no change: the demo at the pinned commit under the candidate label.
    pub fn unchanged_candidate() -> Self {
        Self::checkout(CANDIDATE_COMMIT, Change::None)
    }

    /// The candidate with one deliberate response change: the signup response upper-cases the
    /// username.
    pub fn changed_candidate() -> Self {
        Self::checkout(CANDIDATE_COMMIT, Change::SignupUsername)
    }

    fn checkout(commit: &'static str, change: Change) -> Self {
        let dir = tempfile::tempdir().expect("a temporary directory for the checkout");
        let worktree = dir.path().join("worktree");
        let snapshot = dir.path().join("snapshot");
        let demo = demo_dir();
        extract_archive(
            &demo,
            DEMO_COMMIT,
            &[],
            &dir.path().join("worktree.tar"),
            &worktree,
        );
        extract_archive(
            &demo,
            DEMO_COMMIT,
            &[".acan"],
            &dir.path().join("snapshot.tar"),
            &snapshot,
        );
        if change == Change::SignupUsername {
            change_signup_response(&worktree);
        }
        let corpus = read_json(&snapshot.join(".acan").join("probes").join("core.json"));
        let app = read_json(&snapshot.join(".acan").join("app.json"));
        let policy = serde_json::from_str(POLICY).expect("the test policy decodes");
        let lockfile = ArtifactHash::from(ContentHash::of_bytes(&read_bytes(
            &worktree.join("bun.lock"),
        )));
        Self {
            _dir: dir,
            worktree,
            snapshot,
            commit,
            corpus,
            app,
            policy,
            lockfile,
        }
    }

    /// The probe with `label`, as the corpus names it.
    pub fn probe(&self, label: &str) -> CorpusProbe {
        self.corpus
            .probes
            .iter()
            .find(|probe| probe.label == label)
            .cloned()
            .unwrap_or_else(|| panic!("the corpus has no probe named {label}"))
    }

    /// The probe with `label`, every HTTP step's deadline set to `deadline_ms`.
    pub fn probe_with_deadline(&self, label: &str, deadline_ms: u64) -> CorpusProbe {
        let mut value = serde_json::to_value(self.probe(label)).expect("a probe serializes");
        let steps = value
            .pointer_mut("/spec/steps")
            .and_then(Value::as_array_mut)
            .expect("a probe has steps");
        for step in steps.iter_mut().filter(|step| step["step"] == "http") {
            step["deadline_ms"] = Value::from(deadline_ms);
        }
        serde_json::from_value(value).expect("the probe with the new deadline decodes")
    }

    /// The fixture the probe starts from.
    pub fn fixture(&self, probe: &CorpusProbe) -> &Fixture {
        self.corpus
            .fixtures
            .iter()
            .find(|fixture| fixture.id == probe.spec.fixture)
            .expect("the probe's fixture is in the corpus")
    }

    /// The sequence input for `probe` in `role`, run through `config`.
    pub fn input<'a>(
        &'a self,
        probe: &'a CorpusProbe,
        role: RunRole,
        config: &'a RunnerConfig,
    ) -> SequenceInput<'a> {
        SequenceInput {
            probe,
            fixture: self.fixture(probe),
            routes: &self.corpus.routes,
            app: &self.app,
            policy: &self.policy,
            commit: CommitSha::from_str(self.commit).expect("the commit label is a valid sha"),
            role,
            lockfile_hash: self.lockfile,
            worktree: &self.worktree,
            snapshot: &self.snapshot,
            config,
        }
    }
}

/// The runner's configuration for these tests: the `docker` on `PATH`, the app on [`APP_PORT`].
pub fn runner_config() -> RunnerConfig {
    RunnerConfig::new("docker", APP_PORT)
}

/// Fails the test when a container labelled with `run` is still present in any state.
pub fn assert_no_container(run: &RunId) {
    let output = Command::new("docker")
        .args(["ps", "--all", "--quiet", "--filter"])
        .arg(format!("label=acan.witness={}", run.as_str()))
        .output()
        .expect("docker can list containers");
    assert!(
        output.status.success(),
        "docker ps failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listed = String::from_utf8_lossy(&output.stdout);
    assert!(
        listed.trim().is_empty(),
        "run {} left containers behind: {listed}",
        run.as_str()
    );
}

/// The deliberate change a checkout carries, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    None,
    SignupUsername,
}

fn demo_dir() -> PathBuf {
    let value = std::env::var_os(DEMO_DIR_VAR).unwrap_or_else(|| {
        panic!(
            "set {DEMO_DIR_VAR} to a checkout of the acan-demo repository at commit {DEMO_COMMIT}"
        )
    });
    PathBuf::from(value)
}

/// Writes the commit's files (or the `paths` subset) to a tar in `tar`, then extracts it into
/// `into`.
fn extract_archive(demo: &Path, commit: &str, paths: &[&str], tar: &Path, into: &Path) {
    let archived = Command::new("git")
        .arg("-C")
        .arg(demo)
        .args(["archive", "--format=tar", "--output"])
        .arg(tar)
        .arg(commit)
        .args(paths)
        .status()
        .expect("git can run");
    assert!(archived.success(), "git archive {commit} failed");
    fs::create_dir_all(into).expect("the checkout directory can be created");
    let extracted = Command::new("tar")
        .arg("-xf")
        .arg(tar)
        .arg("-C")
        .arg(into)
        .status()
        .expect("tar can run");
    assert!(extracted.success(), "extracting {} failed", tar.display());
}

fn change_signup_response(worktree: &Path) {
    let path = worktree.join("src").join("auth.ts");
    let text = fs::read_to_string(&path).expect("the demo's auth.ts is readable");
    assert_eq!(
        text.matches(SIGNUP_RESPONSE).count(),
        1,
        "the signup response line is not where the candidate change expects it"
    );
    let changed = text.replace(SIGNUP_RESPONSE, CHANGED_SIGNUP_RESPONSE);
    fs::write(&path, changed).expect("the candidate's auth.ts can be written");
}

fn read_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> T {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("cannot decode {}: {err}", path.display()))
}
