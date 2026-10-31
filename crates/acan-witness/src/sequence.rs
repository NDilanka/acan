//! Runs one probe sequence in a fresh container and admits its record.
//!
//! The steps run in order: the fixture's recipe first, then the probe's steps, which is the
//! order `acan_intent::admit` reads records in. Each step leaves the records the admission rule
//! asks of it, and the run's outcome names how far the sequence got. Every app or container
//! failure becomes a [`RunOutcome`] inside the record; only a bug in this runner fails admission.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use acan_intent::admit::{self, AdmitError};
use acan_intent::{
    ActorName, AdvanceTimeStep, AppContract, ArtifactHash, CanonicalError, CaptureKind,
    CaptureName, CaptureOutcome, CaptureStep, Channels, ClientName, ClockControl, CommitSha,
    Completeness, ContainerPolicy, ContentHash, CorpusProbe, EvidenceHash, Fixture, FixtureHash,
    HttpStep, IncompleteReason, MissingCapture, NonEmpty, Observation, ObservationHash,
    ObservationV1, ObservedRun, ProbeHash, RedirectPolicy, RequestRecord, RouteEntry, Run, RunEnv,
    RunId, RunOutcome, RunRole, RunV1, RunnerBuild, RunnerHash, RunnerManifest, SideEffectStep,
    Step, StepId, TrustedSink, UnixMillis,
};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout, timeout_at};

use crate::docker::{self, MountSources};
use crate::driver::{self, CallError, Container, CreateError, DRIVER_SOURCE, Op, Ready, Reply};
use crate::record::{self, MAX_REDIRECTS, method_text, route_template, same_origin_path};

/// Directory under the run's workspace that holds the harness mounted at `/harness`.
const HARNESS_DIR: &str = "harness";
/// Directory under the run's workspace that masks missing protected paths.
const EMPTY_DIR: &str = "empty";
/// How long cleanup waits for a container create that is still in flight.
const CREATE_GRACE: Duration = Duration::from_secs(10);
/// The store snapshot path the boot fixture serves to the runner.
const STORE_PATH: &str = "/__acan/store";
/// Stderr markers of a container runtime that cannot run the pinned image: the daemon is not
/// reachable, or the image is not in the local store and pulls are off. The runner never pulls,
/// so a missing image is isolation unavailable, not a setup failure of the app.
const RUNTIME_UNAVAILABLE: [&str; 7] = [
    "Cannot connect to the Docker daemon",
    "error during connect",
    "failed to connect to the docker API",
    "Is the docker daemon running",
    "docker_engine",
    "Unable to find image",
    "No such image",
];

/// How the host reaches docker and the app, and which clients and paths the probes name.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// The docker program, a path or a name resolved on `PATH`.
    pub docker: PathBuf,
    /// The port the app listens on inside its container. Every container has its own network
    /// namespace, so two runs may use the same port without colliding.
    pub port: u16,
    /// The loopback source address each client name is sent from.
    pub clients: BTreeMap<ClientName, Ipv4Addr>,
    /// The path the boot fixture serves the store snapshot on.
    pub store_path: String,
}

impl RunnerConfig {
    /// A config that runs `docker`, serves the app on `port`, and sends `client-a` from
    /// `127.0.0.1` and `client-b` from `127.0.0.2`.
    #[must_use]
    pub fn new(docker: impl Into<PathBuf>, port: u16) -> Self {
        let mut clients = BTreeMap::new();
        for (name, address) in [
            ("client-a", Ipv4Addr::LOCALHOST),
            ("client-b", Ipv4Addr::new(127, 0, 0, 2)),
        ] {
            let name = ClientName::from_str(name).expect("the built-in client names are valid");
            clients.insert(name, address);
        }
        Self {
            docker: docker.into(),
            port,
            clients,
            store_path: STORE_PATH.to_owned(),
        }
    }
}

/// Everything one run starts from: the probe and its fixture, the app contract and container
/// policy, the commit and role, and the host paths the container mounts.
#[derive(Debug)]
pub struct SequenceInput<'a> {
    /// The probe to run.
    pub probe: &'a CorpusProbe,
    /// The fixture whose recipe runs first.
    pub fixture: &'a Fixture,
    /// The route table, used to label each request with its route-table pattern.
    pub routes: &'a [RouteEntry],
    /// The protected app contract, `.acan/app.json`, read from the snapshot.
    pub app: &'a AppContract,
    /// The container policy the run applies.
    pub policy: &'a ContainerPolicy,
    /// The commit the worktree is at.
    pub commit: CommitSha,
    /// The role this run plays in its contrast.
    pub role: RunRole,
    /// The hash of the lockfile the app was installed from.
    pub lockfile_hash: ArtifactHash,
    /// The worktree, mounted read-write at the policy's working directory.
    pub worktree: &'a Path,
    /// The directory the protected paths are mounted from, read-only.
    pub snapshot: &'a Path,
    /// How the host reaches docker and the app.
    pub config: &'a RunnerConfig,
}

/// Runs one probe sequence and returns its admitted record.
///
/// `cancel` ends the run as soon as it resolves: the container is removed and the record keeps
/// the steps reached so far, with [`RunOutcome::Cancelled`].
///
/// # Errors
///
/// Returns [`AdmitError`] when the record this runner built does not satisfy the admission rule,
/// which is a bug in the runner. Container, app and request failures are outcomes inside the
/// returned record.
pub async fn run_sequence(
    input: &SequenceInput<'_>,
    cancel: impl Future<Output = ()>,
) -> Result<ObservedRun, AdmitError> {
    let identity = Identity::of(input)?;
    let mut sequence = Sequence::new(input, identity);
    let outcome = {
        let execution = sequence.execute();
        tokio::pin!(execution);
        tokio::pin!(cancel);
        // Cancellation is checked first, so a run cancelled before it starts never spawns docker.
        tokio::select! {
            biased;
            () = &mut cancel => RunOutcome::Cancelled {},
            outcome = &mut execution => outcome,
        }
    };
    sequence.finish(outcome).await
}

/// The identities a record carries, computed once from the inputs.
struct Identity {
    probe: ProbeHash,
    fixture: FixtureHash,
    runner: RunnerHash,
}

impl Identity {
    fn of(input: &SequenceInput<'_>) -> Result<Self, CanonicalError> {
        let runner = RunnerManifest {
            build: runner_build(),
            app: input.app.clone(),
        }
        .runner_hash()?;
        Ok(Self {
            probe: input.probe.spec.probe_hash()?,
            fixture: input.fixture.fixture_hash()?,
            runner,
        })
    }
}

fn runner_build() -> RunnerBuild {
    RunnerBuild::from_str(concat!("acan-witness/", env!("CARGO_PKG_VERSION")))
        .expect("the runner build name is a valid identifier")
}

/// The name of a run's container. The label on it carries the run id, which is what tests find.
fn container_name(run_id: &RunId) -> String {
    format!("acan-witness-{run_id}")
}

/// Why a driver call produced no usable reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// The deadline passed. `own_step` is true when the step's own deadline was the one.
    Deadline { own_step: bool },
    /// The container's driver stopped answering: the app or the container ended.
    Lost,
    /// A reply line was not the driver's protocol.
    Malformed,
}

/// A deadline and whether it belongs to the step.
#[derive(Debug, Clone, Copy)]
struct Limit {
    until: Instant,
    own_step: bool,
}

/// A request template with its captures filled in.
struct Prepared {
    path: String,
    headers: BTreeMap<String, String>,
    body: Option<String>,
}

/// Why a template could not be filled.
enum Unresolved {
    /// A placeholder names a capture the run has not bound.
    Unbound(CaptureName),
    /// The probe cannot be run as written, such as an actor with no session capture.
    Misconfigured(String),
}

/// The host directory one run writes its harness and empty mount points into. It is removed
/// when the run ends, after its container has been removed.
struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn create(run_id: &str) -> std::io::Result<Self> {
        let root = std::env::temp_dir().join(format!("acan-witness-{run_id}"));
        std::fs::create_dir_all(root.join(HARNESS_DIR))?;
        std::fs::create_dir_all(root.join(EMPTY_DIR))?;
        std::fs::write(root.join(HARNESS_DIR).join("driver.ts"), DRIVER_SOURCE)?;
        Ok(Self { root })
    }

    fn harness(&self) -> PathBuf {
        self.root.join(HARNESS_DIR)
    }

    fn empty(&self) -> PathBuf {
        self.root.join(EMPTY_DIR)
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The mutable state of one run.
struct Sequence<'a> {
    input: &'a SequenceInput<'a>,
    identity: Identity,
    run_id: RunId,
    started_at: UnixMillis,
    deadline: Instant,
    steps: Vec<&'a Step>,
    recipe_len: usize,
    /// The controlled clock in milliseconds: the sum of the `advance_time` steps that succeeded.
    /// The app reads the same clock, so a request's `sent_at_ms` is the app's time, not wall time.
    clock_ms: u64,
    workspace: Option<Workspace>,
    /// The create of the container, until it has been awaited.
    pending_create: Option<JoinHandle<Result<String, CreateError>>>,
    /// The id of a container that was created. It is cleared once the container is attached.
    created: Option<String>,
    container: Option<Container>,
    requests: u32,
    observations: Vec<(ObservationHash, Observation)>,
    captures: Vec<CaptureOutcome>,
    values: BTreeMap<CaptureName, Value>,
}

impl<'a> Sequence<'a> {
    fn new(input: &'a SequenceInput<'a>, identity: Identity) -> Self {
        let started = Instant::now();
        let cap_ms = input
            .policy
            .timeout_ms
            .get()
            .min(input.app.limits.sequence_timeout_ms);
        let steps = input
            .fixture
            .recipe
            .iter()
            .chain(input.probe.spec.steps.iter())
            .collect();
        let run_id =
            RunId::from_str(&uuid::Uuid::new_v4().to_string()).expect("a uuid is a valid run id");
        Self {
            input,
            identity,
            run_id,
            started_at: unix_millis(),
            deadline: started
                .checked_add(Duration::from_millis(cap_ms))
                .unwrap_or(started),
            steps,
            recipe_len: input.fixture.recipe.len(),
            clock_ms: 0,
            workspace: None,
            pending_create: None,
            created: None,
            container: None,
            requests: 0,
            observations: Vec::new(),
            captures: Vec::new(),
            values: BTreeMap::new(),
        }
    }

    async fn execute(&mut self) -> RunOutcome {
        if let Err(outcome) = self.start().await {
            return outcome;
        }
        for index in 0..self.steps.len() {
            let step = self.steps[index];
            let result = match step {
                Step::Http(http) => self.http(index, http).await,
                Step::Capture(capture) => self.capture(index, capture),
                Step::AdvanceTime(advance) => self.advance(index, advance).await,
                Step::ObserveSideEffect(observe) => self.observe(index, observe).await,
            };
            if let Err(outcome) = result {
                return outcome;
            }
        }
        RunOutcome::Completed {}
    }

    /// Checks the run can be started as written, then starts the container and the app.
    async fn start(&mut self) -> Result<(), RunOutcome> {
        let input = self.input;
        if !valid_env_name(&input.app.port_env) {
            return Err(setup_failed(
                "the app's port variable is not a valid environment name",
            ));
        }
        // The policy may name the port variable. It must name the port the app is served on,
        // because the driver sets that value and the policy's would otherwise be a lie.
        if let Some(value) = input.policy.environment.get(&input.app.port_env)
            && *value != input.config.port.to_string()
        {
            return Err(setup_failed(format!(
                "the policy sets {} to {value}, but the app is served on port {}",
                input.app.port_env, input.config.port
            )));
        }
        for step in &self.steps {
            if let Step::Http(http) = step
                && !input.config.clients.contains_key(&http.client)
            {
                return Err(setup_failed(format!(
                    "client {} has no address",
                    http.client
                )));
            }
        }
        let workspace =
            Workspace::create(self.run_id.as_str()).map_err(|err| RunOutcome::Error {
                detail: format!("cannot prepare the run directory: {err}"),
            })?;
        let mounts = docker::mount_values(
            input.policy,
            &MountSources {
                worktree: input.worktree,
                snapshot: input.snapshot,
                harness: &workspace.harness(),
                empty: &workspace.empty(),
            },
        )
        .map_err(setup_failed)?;
        self.workspace = Some(workspace);

        let args = docker::create_args(
            &container_name(&self.run_id),
            self.run_id.as_str(),
            input.policy,
            &mounts,
        )
        .map_err(setup_failed)?;
        // The create runs in its own task. If this run stops waiting, cleanup still finds the id.
        let until = self.deadline;
        let pending = self.pending_create.insert(tokio::spawn(driver::create(
            input.config.docker.clone(),
            args,
        )));
        let id = match timeout_at(until, pending).await {
            Err(_elapsed) => {
                return Err(setup_failed(
                    "docker did not create the container before the sequence deadline",
                ));
            }
            Ok(joined) => {
                self.pending_create = None;
                match joined {
                    Err(join) => {
                        return Err(setup_failed(format!(
                            "docker create did not finish: {join}"
                        )));
                    }
                    Ok(Err(CreateError::Spawn(err))) => {
                        return Err(RunOutcome::IsolationUnavailable {
                            detail: format!(
                                "cannot start {}: {err}",
                                input.config.docker.display()
                            ),
                        });
                    }
                    Ok(Err(CreateError::Refused(stderr))) => return Err(start_refusal(&stderr)),
                    Ok(Err(CreateError::Malformed)) => {
                        return Err(setup_failed("docker create printed no container id"));
                    }
                    Ok(Ok(id)) => id,
                }
            }
        };
        self.created = Some(id.clone());
        let container = Container::attach(&input.config.docker, &id).map_err(|err| {
            RunOutcome::IsolationUnavailable {
                detail: format!("cannot start {}: {err}", input.config.docker.display()),
            }
        })?;
        self.container = Some(container);

        let op = Op::Start {
            argv: input.app.boot.as_slice(),
            cwd: input.policy.mounts.workdir.as_str(),
            environment: &input.policy.environment,
            port_env: &input.app.port_env,
            port: input.config.port,
            max_output_bytes: input.app.limits.max_output_bytes,
            ready: Ready {
                path: input.app.ready.path.as_str(),
                status: input.app.ready.status,
                timeout_ms: input.app.ready.timeout_ms,
            },
        };
        let limit = self.limit(None);
        match self.call_driver(&op, limit).await {
            Ok(reply) if reply.ok => Ok(()),
            Ok(reply) => Err(setup_failed(reply.detail)),
            Err(Failure::Deadline { .. }) => Err(setup_failed(
                "the app did not become ready before the sequence deadline",
            )),
            Err(Failure::Malformed) => {
                Err(setup_failed("the driver sent a malformed reply to start"))
            }
            Err(Failure::Lost) => {
                let stderr = match self.container.as_mut() {
                    Some(container) => container.diagnose().await,
                    None => String::new(),
                };
                Err(start_refusal(&stderr))
            }
        }
    }

    async fn http(&mut self, index: usize, step: &HttpStep) -> Result<(), RunOutcome> {
        let recipe = index < self.recipe_len;
        let prepared = match self.prepare(step) {
            Ok(prepared) => prepared,
            Err(Unresolved::Misconfigured(detail)) => {
                return Err(failed(&step.id, recipe, detail));
            }
            Err(Unresolved::Unbound(binding)) => {
                let reasons = vec![IncompleteReason::CaptureMissing { binding }];
                let observation = self.observation(&step.id, None, Channels::default(), reasons);
                self.push(observation)?;
                return if recipe {
                    Err(failed(
                        &step.id,
                        recipe,
                        "a capture the request needs is missing".to_owned(),
                    ))
                } else {
                    Ok(())
                };
            }
        };
        let method = step.request.method;
        let Some(route) = route_template(method, &prepared.path, self.input.routes) else {
            return Err(failed(
                &step.id,
                recipe,
                format!(
                    "{} {} matches no route of the table",
                    method_text(method),
                    prepared.path
                ),
            ));
        };
        let address = self.address(step)?;
        let record = RequestRecord {
            method,
            route_template: route,
            actor: step.actor.clone(),
            client: step.client.clone(),
            sent_at_ms: self.clock_ms,
        };
        self.spend()?;
        let limit = self.limit(Some(step.deadline_ms));
        let sent = self
            .send(
                &address,
                method_text(method),
                &prepared.path,
                &prepared.headers,
                prepared.body.as_deref(),
                limit,
            )
            .await;
        let reply = match sent {
            Ok(reply) => reply,
            Err(Failure::Deadline { own_step: true }) => {
                let reasons = vec![IncompleteReason::DeadlineExceeded {}];
                let observation =
                    self.observation(&step.id, Some(record), Channels::default(), reasons);
                self.push(observation)?;
                return Err(RunOutcome::TimedOut {
                    step_id: step.id.clone(),
                });
            }
            Err(failure) => return Err(outcome_for(&step.id, recipe, failure)),
        };
        if !reply.ok {
            return Err(reply_failure(&step.id, recipe, reply.crashed, reply.detail));
        }
        let reply = if step.redirect == RedirectPolicy::FollowSameOrigin {
            self.follow(step, recipe, &prepared, &address, reply)
                .await?
        } else {
            reply
        };
        let (channels, reasons) = record::channels(&reply);
        let status = reply.status;
        let observation = self.observation(&step.id, Some(record), channels, reasons);
        self.push(observation)?;
        match status {
            Some(code) if recipe && code >= 500 => Err(RunOutcome::SetupFailed {
                step_id: Some(step.id.clone()),
                detail: format!("the request returned status {code}"),
            }),
            _ => Ok(()),
        }
    }

    /// Follows same-origin redirects for a step whose policy asks for it. Each hop is one more
    /// request against the sequence's budget. The step's observation is the final response.
    async fn follow(
        &mut self,
        step: &HttpStep,
        recipe: bool,
        prepared: &Prepared,
        address: &str,
        mut reply: Reply,
    ) -> Result<Reply, RunOutcome> {
        let method = method_text(step.request.method);
        for _ in 0..MAX_REDIRECTS {
            let Some(status) = reply.status.filter(|code| (300..400).contains(code)) else {
                break;
            };
            let location = record::header(&reply, "location");
            let Some(next) =
                location.and_then(|location| same_origin_path(&location, self.input.config.port))
            else {
                break;
            };
            // 307 and 308 keep the method and body; the others become a GET without a body.
            let (verb, body) = if matches!(status, 307 | 308) {
                (method, prepared.body.as_deref())
            } else {
                ("GET", None)
            };
            self.spend()?;
            let limit = self.limit(Some(step.deadline_ms));
            let sent = self
                .send(address, verb, &next, &prepared.headers, body, limit)
                .await;
            reply = match sent {
                Ok(reply) => reply,
                Err(failure) => return Err(outcome_for(&step.id, recipe, failure)),
            };
            if !reply.ok {
                return Err(reply_failure(&step.id, recipe, reply.crashed, reply.detail));
            }
        }
        Ok(reply)
    }

    /// Records a capture from its source step's channels, and binds the value when it is there.
    fn capture(&mut self, index: usize, step: &CaptureStep) -> Result<(), RunOutcome> {
        let read = match self
            .observations
            .iter()
            .rev()
            .find(|(_, observation)| observation.step_id == step.from)
        {
            Some((_, observation)) => {
                record::read_capture(&observation.channels, &step.source, step.kind)
            }
            None => Err(MissingCapture::StepFailed),
        };
        match read {
            Ok(value) => {
                let hash = ContentHash::of(&value).map_err(|err| RunOutcome::Error {
                    detail: err.to_string(),
                })?;
                self.captures.push(CaptureOutcome::Captured {
                    binding: step.binding.clone(),
                    step_id: step.id.clone(),
                    value_hash: EvidenceHash::from(hash),
                });
                self.values.insert(step.binding.clone(), value);
                Ok(())
            }
            Err(reason) => {
                self.captures.push(CaptureOutcome::Missing {
                    binding: step.binding.clone(),
                    step_id: step.id.clone(),
                    reason,
                });
                if index < self.recipe_len {
                    Err(step_failed(
                        &step.id,
                        format!("capture {} is missing", step.binding),
                    ))
                } else {
                    Ok(())
                }
            }
        }
    }

    async fn advance(&mut self, index: usize, step: &AdvanceTimeStep) -> Result<(), RunOutcome> {
        let recipe = index < self.recipe_len;
        let input = self.input;
        let ClockControl::HttpEndpoint { path } = &input.app.clock else {
            return Err(failed(
                &step.id,
                recipe,
                "the app has no controlled clock".to_owned(),
            ));
        };
        self.spend()?;
        let limit = self.limit(None);
        let op = Op::Clock {
            path: path.as_str(),
            by_ms: step.by_ms,
        };
        let answered = self.call_driver(&op, limit).await;
        let reply = match answered {
            Ok(reply) => reply,
            Err(failure) => return Err(outcome_for(&step.id, recipe, failure)),
        };
        if !reply.ok {
            return Err(reply_failure(&step.id, recipe, reply.crashed, reply.detail));
        }
        match reply.status {
            Some(code) if (200..300).contains(&code) => {
                self.clock_ms = self.clock_ms.saturating_add(step.by_ms);
                Ok(())
            }
            code => Err(failed(
                &step.id,
                recipe,
                format!("the clock returned status {code:?}"),
            )),
        }
    }

    async fn observe(&mut self, index: usize, step: &SideEffectStep) -> Result<(), RunOutcome> {
        let recipe = index < self.recipe_len;
        let settled = if step.sink == TrustedSink::Store
            && self.input.app.sinks.contains(&TrustedSink::Store)
        {
            self.settle(step).await?
        } else {
            None
        };
        let (channels, reasons) = match settled {
            Some(value) => {
                let mut channels = Channels::default();
                channels.side_effects.insert(step.sink, value);
                (channels, Vec::new())
            }
            None => (
                Channels::default(),
                vec![IncompleteReason::SinkIncomplete { sink: step.sink }],
            ),
        };
        let incomplete = !reasons.is_empty();
        let observation = self.observation(&step.id, None, channels, reasons);
        self.push(observation)?;
        if recipe && incomplete {
            return Err(step_failed(
                &step.id,
                format!("the {:?} sink did not settle", step.sink),
            ));
        }
        Ok(())
    }

    /// Reads the store snapshot until two reads a quiet period apart agree. The barrier's timeout
    /// only ends the reads: `None` means the sink did not settle in time, which is an incomplete
    /// observation, not a timed-out run. Only the sequence's deadline ends a read early.
    async fn settle(&mut self, step: &SideEffectStep) -> Result<Option<Value>, RunOutcome> {
        let quiet = Duration::from_millis(step.barrier.quiet_ms);
        let until = Instant::now()
            .checked_add(Duration::from_millis(step.barrier.timeout_ms))
            .unwrap_or(self.deadline);
        let mut last = self.snapshot(&step.id).await?;
        while Instant::now() < until {
            sleep(quiet).await;
            let next = self.snapshot(&step.id).await?;
            if next.is_some() && next == last {
                return Ok(next);
            }
            last = next;
        }
        Ok(None)
    }

    async fn snapshot(&mut self, step_id: &StepId) -> Result<Option<Value>, RunOutcome> {
        self.spend()?;
        let input = self.input;
        let op = Op::Get {
            path: input.config.store_path.as_str(),
        };
        let limit = self.limit(None);
        let answered = self.call_driver(&op, limit).await;
        let reply = match answered {
            Ok(reply) => reply,
            Err(failure) => return Err(outcome_for(step_id, false, failure)),
        };
        if !reply.ok {
            return if reply.crashed {
                Err(RunOutcome::Crashed {
                    detail: reply.detail,
                })
            } else {
                Ok(None)
            };
        }
        if reply.status != Some(200) {
            return Ok(None);
        }
        Ok(reply
            .body
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok()))
    }

    /// Sends one request to the driver, which sends it from the client's address.
    async fn send(
        &mut self,
        address: &str,
        method: &str,
        path: &str,
        headers: &BTreeMap<String, String>,
        body: Option<&str>,
        limit: Limit,
    ) -> Result<Reply, Failure> {
        let op = Op::Request {
            client: address,
            method,
            path,
            headers,
            body,
        };
        self.call_driver(&op, limit).await
    }

    async fn call_driver(&mut self, op: &Op<'_>, limit: Limit) -> Result<Reply, Failure> {
        let Some(container) = self.container.as_mut() else {
            return Err(Failure::Lost);
        };
        match timeout_at(limit.until, container.call(op)).await {
            Err(_) => Err(Failure::Deadline {
                own_step: limit.own_step,
            }),
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(CallError::Lost)) => Err(Failure::Lost),
            Ok(Err(CallError::Malformed)) => Err(Failure::Malformed),
        }
    }

    /// The deadline a call may run until: the sequence's, or the step's own when it is earlier.
    fn limit(&self, step_ms: Option<u64>) -> Limit {
        let own = step_ms.and_then(|ms| Instant::now().checked_add(Duration::from_millis(ms)));
        match own {
            Some(until) if until < self.deadline => Limit {
                until,
                own_step: true,
            },
            _ => Limit {
                until: self.deadline,
                own_step: false,
            },
        }
    }

    /// Counts one request against the sequence's bound. Exceeding it ends the run as an error.
    fn spend(&mut self) -> Result<(), RunOutcome> {
        let max = self.input.app.limits.max_requests;
        if self.requests >= max {
            return Err(RunOutcome::Error {
                detail: format!("the sequence exceeded {max} requests"),
            });
        }
        self.requests = self.requests.saturating_add(1);
        Ok(())
    }

    fn address(&self, step: &HttpStep) -> Result<String, RunOutcome> {
        self.input
            .config
            .clients
            .get(&step.client)
            .map(Ipv4Addr::to_string)
            .ok_or_else(|| RunOutcome::Error {
                detail: format!("client {} has no address", step.client),
            })
    }

    /// Fills a request's templates from the run's captures. A session header carries the
    /// actor's bound token as a bearer credential, the same way the app reads it.
    fn prepare(&self, step: &HttpStep) -> Result<Prepared, Unresolved> {
        let mut headers = BTreeMap::new();
        if let Some(actor) = &step.actor {
            let binding = self.session_binding(actor).ok_or_else(|| {
                Unresolved::Misconfigured(format!(
                    "no session capture is declared for actor {actor}"
                ))
            })?;
            let token = self
                .values
                .get(&binding)
                .and_then(Value::as_str)
                .ok_or_else(|| Unresolved::Unbound(binding.clone()))?;
            headers.insert("authorization".to_owned(), format!("Bearer {token}"));
        }
        for (name, template) in &step.request.headers {
            let value =
                record::fill(template.as_str(), &self.values).map_err(Unresolved::Unbound)?;
            headers.insert(name.clone(), value);
        }
        let path =
            record::fill(step.request.path.as_str(), &self.values).map_err(Unresolved::Unbound)?;
        let body = match &step.request.body {
            None => None,
            Some(body) => {
                let filled = record::fill_json(body, &self.values).map_err(Unresolved::Unbound)?;
                headers.insert("content-type".to_owned(), "application/json".to_owned());
                Some(filled.to_string())
            }
        };
        Ok(Prepared {
            path,
            headers,
            body,
        })
    }

    /// The session capture an actor's requests carry: the latest one the run has bound, or the
    /// latest one declared when none is bound yet.
    fn session_binding(&self, actor: &ActorName) -> Option<CaptureName> {
        let declared: Vec<&CaptureName> = self
            .steps
            .iter()
            .filter_map(|step| match step {
                Step::Capture(capture)
                    if capture.kind == CaptureKind::Session
                        && capture.actor.as_ref() == Some(actor) =>
                {
                    Some(&capture.binding)
                }
                _ => None,
            })
            .collect();
        declared
            .iter()
            .rev()
            .copied()
            .find(|binding| self.values.contains_key(*binding))
            .or(declared.last().copied())
            .cloned()
    }

    fn observation(
        &self,
        step_id: &StepId,
        request: Option<RequestRecord>,
        channels: Channels,
        reasons: Vec<IncompleteReason>,
    ) -> Observation {
        Observation {
            version: ObservationV1,
            run_id: self.run_id.clone(),
            commit: self.input.commit.clone(),
            probe: self.identity.probe,
            step_id: step_id.clone(),
            completeness: completeness(reasons),
            request,
            channels,
            raw_ref: None,
        }
    }

    fn push(&mut self, observation: Observation) -> Result<(), RunOutcome> {
        let hash = observation
            .observation_hash()
            .map_err(|err| RunOutcome::Error {
                detail: err.to_string(),
            })?;
        self.observations.push((hash, observation));
        Ok(())
    }

    /// Removes a container that never reached the attach step: one whose create is still in
    /// flight, or one that was created but not started. A create that does not finish within the
    /// grace period is removed by name, which is harmless when nothing was created.
    async fn remove_unattached(&mut self) {
        let docker = self.input.config.docker.clone();
        let target = match (self.created.take(), self.pending_create.take()) {
            (Some(id), _) => id,
            (None, Some(handle)) => match timeout(CREATE_GRACE, handle).await {
                Ok(Ok(Ok(id))) => id,
                _ => container_name(&self.run_id),
            },
            (None, None) => return,
        };
        driver::remove(&docker, &target).await;
    }

    /// Ends the container, removes the workspace, and admits the record.
    async fn finish(mut self, outcome: RunOutcome) -> Result<ObservedRun, AdmitError> {
        let force = !matches!(outcome, RunOutcome::Completed {});
        match self.container.take() {
            Some(container) => container.finish(force).await,
            None => self.remove_unattached().await,
        }
        let run = Run {
            version: RunV1,
            id: self.run_id.clone(),
            probe: self.identity.probe,
            commit: self.input.commit.clone(),
            role: self.input.role,
            env: RunEnv {
                lockfile_hash: self.input.lockfile_hash,
                fixture_hash: self.identity.fixture,
                runtime: self.input.app.runtime.clone(),
            },
            runner: self.identity.runner,
            outcome,
            captures: std::mem::take(&mut self.captures),
            started_at: self.started_at,
            finished_at: unix_millis(),
        };
        let observations = std::mem::take(&mut self.observations);
        admit::observed_run(self.input.probe, self.input.fixture, run, observations)
    }
}

/// The outcome of a step that failed: a setup failure in the recipe, an error in the probe.
fn failed(step_id: &StepId, recipe: bool, detail: String) -> RunOutcome {
    if recipe {
        step_failed(step_id, detail)
    } else {
        RunOutcome::Error { detail }
    }
}

fn step_failed(step_id: &StepId, detail: String) -> RunOutcome {
    RunOutcome::SetupFailed {
        step_id: Some(step_id.clone()),
        detail,
    }
}

/// A failure before any step ran, or while none is the cause.
fn setup_failed(detail: impl Into<String>) -> RunOutcome {
    RunOutcome::SetupFailed {
        step_id: None,
        detail: detail.into(),
    }
}

fn outcome_for(step_id: &StepId, recipe: bool, failure: Failure) -> RunOutcome {
    match failure {
        Failure::Deadline { .. } => RunOutcome::TimedOut {
            step_id: step_id.clone(),
        },
        Failure::Lost => RunOutcome::Crashed {
            detail: "the app container stopped answering".to_owned(),
        },
        Failure::Malformed => failed(
            step_id,
            recipe,
            "the driver sent a malformed reply".to_owned(),
        ),
    }
}

fn reply_failure(step_id: &StepId, recipe: bool, crashed: bool, detail: String) -> RunOutcome {
    if crashed {
        RunOutcome::Crashed { detail }
    } else {
        failed(step_id, recipe, detail)
    }
}

/// What docker's stderr says about a container that did not start or answer: from `docker create`
/// or from `docker start`. An unreachable runtime is isolation unavailable; anything else is a
/// setup failure with no step.
fn start_refusal(stderr: &str) -> RunOutcome {
    let first = stderr.lines().next().unwrap_or("").trim();
    if RUNTIME_UNAVAILABLE
        .iter()
        .any(|marker| stderr.contains(marker))
    {
        return RunOutcome::IsolationUnavailable {
            detail: format!("the container runtime is unavailable: {first}"),
        };
    }
    if first.is_empty() {
        setup_failed("the container ended before the driver answered")
    } else {
        setup_failed(format!("docker refused the container: {first}"))
    }
}

/// The record's completeness for one observation: complete with no reasons, else incomplete.
fn completeness(reasons: Vec<IncompleteReason>) -> Completeness {
    let mut reasons = reasons.into_iter();
    match reasons.next() {
        None => Completeness::Complete {},
        Some(first) => Completeness::Incomplete {
            reasons: NonEmpty::new(first, reasons),
        },
    }
}

/// An app environment variable name: upper-case letters, digits and underscores.
fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('A'..='Z' | '_'))
        && chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_'))
}

fn unix_millis() -> UnixMillis {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    UnixMillis(millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreachable_runtime_is_isolation_unavailable() {
        let stderr = "error during connect: Get \"http://%2F%2F.%2Fpipe%2Fdocker_engine/v1.51/containers/json\": open //./pipe/docker_engine: The system cannot find the file specified.";
        assert!(
            matches!(
                start_refusal(stderr),
                RunOutcome::IsolationUnavailable { .. }
            ),
            "a daemon that is down must not run the sequence: {stderr}"
        );
    }

    #[test]
    fn a_missing_pinned_image_is_isolation_unavailable() {
        let stderr = "Unable to find image 'oven/bun@sha256:1d22' locally\ndocker: Error response from daemon";
        assert!(matches!(
            start_refusal(stderr),
            RunOutcome::IsolationUnavailable { .. }
        ));
        let stderr = "docker: Error response from daemon: No such image: oven/bun@sha256:1d22";
        assert!(matches!(
            start_refusal(stderr),
            RunOutcome::IsolationUnavailable { .. }
        ));
    }

    #[test]
    fn another_refusal_is_a_setup_failure_with_no_step() {
        let stderr = "docker: invalid reference format";
        match start_refusal(stderr) {
            RunOutcome::SetupFailed { step_id, detail } => {
                assert_eq!(step_id, None, "a refusal names no step: {detail}");
                assert!(
                    detail.contains("invalid reference format"),
                    "detail: {detail}"
                );
            }
            other => panic!("expected a setup failure, got {other:?}"),
        }
    }
}
