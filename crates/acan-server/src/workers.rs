//! Local workers. `acan serve --workers N` runs N worker loops in this process. Each loop runs the
//! acan-agent worker against this origin's `/mcp` with the loop's own agent token, one task per
//! run, and starts the next run after a short pause. A loop waits, and says so in its state, while
//! no model key is set, so a key saved on the setup page starts work without a restart.
//!
//! The loops take the path an outside worker takes: the agent token authenticates each MCP call,
//! and the origin checks it. The model client is the server's own, so a loop uses the same keys
//! and routes as every other model job, including keys saved on the setup page.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use acan_agent::{Exit, WorkerConfig, run as run_worker};
use acan_core::AgentId;
use acan_llm::ModelClient;
use serde::Serialize;
use tokio::task::JoinHandle;

use crate::host::{Host, now};

/// How many model calls one run may make. The same default `acan-worker` uses.
const MAX_STEPS: usize = 30;
/// How long one wait for work or build clearance may last. The same default `acan-worker` uses.
const WAIT: Duration = Duration::from_secs(600);
/// How often a run renews its lease. Well under the origin's 60 second lease.
const HEARTBEAT: Duration = Duration::from_secs(20);
/// How often a wait polls the origin.
const POLL: Duration = Duration::from_millis(500);
/// The pause before the next run after a run that ended with something to act on.
const NEXT_RUN: Duration = Duration::from_secs(2);
/// The pause before the next run after a run that failed, so a broken setup does not spin.
const FAILED_RUN: Duration = Duration::from_secs(30);
/// How often a loop checks for a model key while none is set.
const NO_KEY_CHECK: Duration = Duration::from_secs(5);

/// The agent id of local worker `index`, counted from zero: `local-1` for the first.
///
/// # Errors
///
/// Returns an error when the name is not a valid agent id.
pub(crate) fn agent(index: usize) -> anyhow::Result<AgentId> {
    Ok(AgentId::from_str(&agent_name(index))?)
}

fn agent_name(index: usize) -> String {
    format!("local-{}", index + 1)
}

/// The base URL the loops reach this server at. A wildcard address is reached through loopback.
#[must_use]
pub(crate) fn origin_url(addr: SocketAddr) -> String {
    let ip = match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    format!("http://{}", SocketAddr::new(ip, addr.port()))
}

/// The state of one loop, as `GET /api/workers` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for work or doing it.
    Running,
    /// Idle until a model key is set. No run starts meanwhile.
    NoKey,
}

/// How the last run of a loop ended.
#[derive(Debug, Clone, Serialize)]
pub struct Last {
    /// A short name for the outcome, such as `built` or `no_task`.
    pub outcome: &'static str,
    /// The outcome in words.
    pub text: String,
    /// When the run ended, in the origin's clock.
    pub at: acan_core::Timestamp,
}

/// One local worker.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// The agent's id.
    pub id: String,
    /// What the loop is doing.
    pub state: State,
    /// How the last run ended. `None` until a run ends.
    pub last: Option<Last>,
}

/// What `GET /api/workers` returns.
#[derive(Debug, Clone, Serialize)]
pub struct WorkersView {
    /// Every local worker this server runs. Empty when it runs none.
    pub workers: Vec<Status>,
}

/// The loops' state, shared with the HTTP route.
#[derive(Debug, Clone, Default)]
pub struct Board(Arc<Mutex<Vec<Status>>>);

impl Board {
    /// A board for `count` loops, each waiting for its first run.
    #[must_use]
    pub fn new(count: usize) -> Self {
        let statuses = (0..count)
            .map(|index| Status {
                id: agent_name(index),
                state: State::Running,
                last: None,
            })
            .collect();
        Self(Arc::new(Mutex::new(statuses)))
    }

    /// The state of every loop.
    #[must_use]
    pub fn view(&self) -> WorkersView {
        WorkersView {
            workers: self.lock().clone(),
        }
    }

    fn set_state(&self, index: usize, state: State) {
        if let Some(status) = self.lock().get_mut(index) {
            status.state = state;
        }
    }

    fn record(&self, index: usize, exit: &Exit) {
        if let Some(status) = self.lock().get_mut(index) {
            status.last = Some(Last {
                outcome: outcome_name(exit),
                text: exit.to_string(),
                at: now(),
            });
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Status>> {
        self.0.lock().expect("worker board lock poisoned")
    }
}

/// The short name of an outcome, for the status.
fn outcome_name(exit: &Exit) -> &'static str {
    match exit {
        Exit::Built { .. } => "built",
        Exit::NoTask => "no_task",
        Exit::NotCleared { .. } => "not_cleared",
        Exit::Lost { .. } => "lost",
        Exit::NoCommit { .. } => "no_commit",
        Exit::Narrowed { .. } => "narrowed",
        Exit::Residue { .. } => "residue",
        Exit::Failed { .. } => "failed",
    }
}

/// Starts one loop per token. Each loop runs until its task is aborted.
pub(crate) fn spawn<C: ModelClient + 'static>(
    host: Host<C>,
    board: Board,
    tokens: Vec<String>,
    repo: PathBuf,
    server: String,
) -> Vec<JoinHandle<()>> {
    tokens
        .into_iter()
        .enumerate()
        .map(|(index, token)| {
            let config = WorkerConfig {
                server: server.clone(),
                token,
                repo: repo.clone(),
                test_command: None,
                max_steps: MAX_STEPS,
                wait: WAIT,
                receipt_wait: WAIT,
                heartbeat: HEARTBEAT,
                poll: POLL,
            };
            tokio::spawn(drive(host.clone(), board.clone(), index, config))
        })
        .collect()
}

/// One loop: a run while a model key is set, then the next run after a pause.
async fn drive<C: ModelClient>(host: Host<C>, board: Board, index: usize, config: WorkerConfig) {
    loop {
        if !host.client().key_status().configured {
            board.set_state(index, State::NoKey);
            tokio::time::sleep(NO_KEY_CHECK).await;
            continue;
        }
        board.set_state(index, State::Running);
        let outcome = run_worker(&config, host.client()).await;
        board.record(index, &outcome.exit);
        let pause = match outcome.exit {
            Exit::Failed { .. } => FAILED_RUN,
            _ => NEXT_RUN,
        };
        tokio::time::sleep(pause).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_workers_are_named_from_one() {
        assert_eq!(agent(0).expect("valid id").to_string(), "local-1");
        assert_eq!(agent(2).expect("valid id").to_string(), "local-3");
    }

    #[test]
    fn a_wildcard_address_is_reached_through_loopback() {
        let v4: SocketAddr = "0.0.0.0:7311".parse().expect("address");
        let v6: SocketAddr = "[::]:7311".parse().expect("address");
        let lan: SocketAddr = "192.168.1.20:7311".parse().expect("address");
        assert_eq!(origin_url(v4), "http://127.0.0.1:7311");
        assert_eq!(origin_url(v6), "http://[::1]:7311");
        assert_eq!(origin_url(lan), "http://192.168.1.20:7311");
    }

    #[test]
    fn the_board_starts_each_loop_waiting_and_records_the_last_outcome() {
        let board = Board::new(2);
        let view = board.view();
        assert_eq!(view.workers.len(), 2);
        assert_eq!(view.workers[1].id, "local-2");
        assert_eq!(view.workers[0].state, State::Running);
        assert!(view.workers[0].last.is_none());

        board.set_state(1, State::NoKey);
        board.record(1, &Exit::NoTask);
        let view = board.view();
        assert_eq!(view.workers[1].state, State::NoKey);
        let last = view.workers[1].last.as_ref().expect("an outcome");
        assert_eq!(last.outcome, "no_task");
        assert_eq!(last.text, "no task could be claimed");
        assert_eq!(view.workers[0].state, State::Running);
    }

    #[test]
    fn every_outcome_has_a_short_name() {
        assert_eq!(
            outcome_name(&Exit::Built {
                task: "t".to_owned(),
                generation: 1,
                head: "h".to_owned(),
            }),
            "built"
        );
        assert_eq!(
            outcome_name(&Exit::Failed {
                reason: "r".to_owned()
            }),
            "failed"
        );
    }
}
