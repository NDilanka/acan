#![allow(
    dead_code,
    reason = "each test binary uses a different subset of the fixture"
)]

use std::collections::{BTreeMap, BTreeSet};

use acan_core::{
    Actor, AgentCmd, AgentId, Barrier, Belt, Check, CodeIndex, Command, Ctx, DeclarationInput,
    Event, Fence, FilePath, HumanCmd, IndexedFile, IndexedSymbol, Judgement, LEASE_TTL_MS, Locus,
    MachineKey, MemberId, OrderId, Origin, QuestionDraft, ReadbackId, ReadbackVerdict, Rejection,
    ReplayError, Role, Seq, Stamped, SymbolRef, SystemCmd, TaskId, TaskSpec, TaskState, Timestamp,
    WitnessId, WitnessSpec, WorldRef,
};

pub fn path(raw: &str) -> FilePath {
    raw.parse().expect("fixture path is valid")
}

pub fn machine(raw: &str) -> MachineKey {
    raw.parse().expect("fixture machine is valid")
}

pub fn sym(file: &str, name: &str) -> SymbolRef {
    SymbolRef {
        file: path(file),
        name: name.to_owned(),
    }
}

fn file(machine_name: &str, symbols: &[(&str, u32, u32, bool)]) -> IndexedFile {
    IndexedFile {
        machine: machine(machine_name),
        symbols: symbols
            .iter()
            .map(|&(name, start, end, exported)| IndexedSymbol {
                name: name.to_owned(),
                bytes: start..end,
                exported,
            })
            .collect(),
    }
}

/// Two machines, `api` imports `auth`, plus a registry file and a lockfile.
pub fn index() -> CodeIndex {
    let mut index = CodeIndex::new("rev-1".parse().expect("valid rev"));
    index.files = BTreeMap::from([
        (
            path("src/api/routes.rs"),
            file(
                "api",
                &[
                    ("router", 0, 100, true),
                    ("rate_limit", 100, 200, false),
                    ("health", 200, 260, false),
                ],
            ),
        ),
        (path("src/api/mod.rs"), file("api", &[])),
        (
            path("src/auth/session.rs"),
            file(
                "auth",
                &[("Session", 0, 80, true), ("verify", 80, 160, true)],
            ),
        ),
        (
            path("src/auth/login.rs"),
            file("auth", &[("login", 0, 50, true)]),
        ),
        (path("Cargo.lock"), file(".", &[])),
    ]);
    index.belts = BTreeSet::from([Belt {
        from: machine("api"),
        to: machine("auth"),
    }]);
    index
}

pub fn events(batch: &[Stamped]) -> Vec<&Event> {
    batch.iter().map(|s| &s.event).collect()
}

/// The log as the host stores it: through JSON and back.
pub fn stored(log: &[Stamped]) -> Vec<Stamped> {
    let json = serde_json::to_string(log).expect("log serializes");
    serde_json::from_str(&json).expect("log parses")
}

/// A world with an owner, a second member `bob`, one agent each, and one order split into two tasks.
pub struct Fx {
    pub origin: Origin,
    pub log: Vec<Stamped>,
    pub index: CodeIndex,
    pub now: Timestamp,
    pub owner: MemberId,
    pub bob: MemberId,
    /// Owned by `owner`.
    pub agent_a: AgentId,
    /// Owned by `bob`.
    pub agent_b: AgentId,
    pub order: OrderId,
    pub task1: TaskId,
    pub task2: TaskId,
}

impl Fx {
    pub fn new() -> Self {
        let owner: MemberId = "owner".parse().expect("valid member");
        let world = WorldRef {
            id: "w".parse().expect("valid world"),
            repo: "demo".to_owned(),
        };
        let now = Timestamp::from_millis(1_000);
        let (origin, opened) = Origin::open(world, owner.clone(), now);
        let mut fx = Self {
            origin,
            log: vec![opened],
            index: index(),
            now,
            owner: owner.clone(),
            bob: "bob".parse().expect("valid member"),
            agent_a: "agent-a".parse().expect("valid agent"),
            agent_b: "agent-b".parse().expect("valid agent"),
            order: "o-0".parse().expect("valid order"),
            task1: "t-0".parse().expect("valid task"),
            task2: "t-1".parse().expect("valid task"),
        };
        let bob = fx.bob.clone();
        fx.human(
            &owner,
            HumanCmd::AddMember {
                member: bob.clone(),
                role: Role::Member,
            },
        )
        .expect("owner adds bob");
        fx.human(
            &owner,
            HumanCmd::RegisterAgent {
                agent: fx.agent_a.clone(),
            },
        )
        .expect("owner registers agent a");
        fx.human(
            &bob,
            HumanCmd::RegisterAgent {
                agent: fx.agent_b.clone(),
            },
        )
        .expect("bob registers agent b");
        fx.human(
            &owner,
            HumanCmd::PostOrder {
                machine: machine("api"),
                sentence: "Add rate limiting to the API, keep login untouched".to_owned(),
                witnesses: vec![WitnessSpec {
                    check: Check::Test {
                        command: "cargo test -p api login".to_owned(),
                    },
                    reason: "login keeps working".to_owned(),
                }],
            },
        )
        .expect("owner posts order");
        fx.order = fx.origin.world().orders().next().expect("order posted").id;
        let rev = fx.origin.world().order(fx.order).expect("order exists").rev;
        let spec = |title: &str| TaskSpec {
            title: title.to_owned(),
            brief: String::new(),
            scope: vec![machine("api")],
        };
        fx.system(SystemCmd::SplitReady {
            order: fx.order,
            rev,
            tasks: vec![spec("limiter"), spec("routes")],
        })
        .expect("split accepted");
        let tasks: Vec<TaskId> = fx.origin.world().tasks_of(fx.order).map(|t| t.id).collect();
        fx.task1 = tasks[0];
        fx.task2 = tasks[1];
        fx
    }

    pub fn run(&mut self, cmd: Command) -> Result<Vec<Stamped>, Rejection> {
        let ctx = Ctx {
            now: self.now,
            index: &self.index,
        };
        let batch = self.origin.execute(&ctx, cmd)?;
        self.log.extend(batch.iter().cloned());
        Ok(batch)
    }

    pub fn human(&mut self, member: &MemberId, cmd: HumanCmd) -> Result<Vec<Stamped>, Rejection> {
        self.run(Command::Human {
            member: member.clone(),
            cmd,
        })
    }

    pub fn agent(&mut self, agent: &AgentId, cmd: AgentCmd) -> Result<Vec<Stamped>, Rejection> {
        self.run(Command::Agent {
            agent: agent.clone(),
            cmd,
        })
    }

    pub fn system(&mut self, cmd: SystemCmd) -> Result<Vec<Stamped>, Rejection> {
        self.run(Command::System(cmd))
    }

    /// Runs `cmd` through the origin's advisory path, which skips every maintenance step.
    pub fn advise(&mut self, cmd: SystemCmd) -> Result<Vec<Stamped>, Rejection> {
        let ctx = Ctx {
            now: self.now,
            index: &self.index,
        };
        let batch = self.origin.execute_advisory(&ctx, Command::System(cmd))?;
        self.log.extend(batch.iter().cloned());
        Ok(batch)
    }

    /// Appends `event` to the stored log as the next system event and replays the origin
    /// from that log, as a host would after restarting on a damaged or forged store.
    pub fn append_stored(&mut self, event: Event) -> Result<(), ReplayError> {
        let last = self.log.last().expect("the log is open").seq;
        let mut log = self.log.clone();
        log.push(Stamped {
            seq: Seq::new(last.get() + 1),
            at: self.now,
            actor: Actor::System,
            event,
        });
        self.origin = Origin::replay(stored(&log))?;
        self.log = log;
        Ok(())
    }

    pub fn advance(&mut self, ms: u64) {
        self.now = self.now.plus_millis(ms);
    }

    pub fn witnesses(&self) -> Vec<WitnessId> {
        self.origin
            .world()
            .order(self.order)
            .expect("order exists")
            .witnesses
            .clone()
    }

    pub fn read_back(&mut self, agent: &AgentId, task: TaskId) -> ReadbackId {
        self.agent(
            agent,
            AgentCmd::PostReadback {
                task,
                summary: "limit /api".to_owned(),
                touches: vec![],
            },
        )
        .expect("readback accepted");
        self.origin
            .world()
            .readbacks_of(task)
            .last()
            .expect("readback recorded")
            .id
    }

    /// Reviews `readback` at the current revision with `judgement` on every witness.
    pub fn review(
        &mut self,
        readback: ReadbackId,
        judgement: Judgement,
    ) -> Result<Vec<Stamped>, Rejection> {
        let order = self.origin.world().order(self.order).expect("order exists");
        let verdict = ReadbackVerdict {
            rev: order.rev,
            per_witness: order
                .witnesses
                .iter()
                .map(|&w| (w, judgement.clone()))
                .collect(),
        };
        let rev = order.rev;
        self.system(SystemCmd::ReadbacksReviewed {
            order: self.order,
            rev,
            verdicts: vec![(readback, verdict)],
            question: None,
        })
    }

    /// Reads back, reviews as matched, and claims. Returns the fence.
    pub fn claim(&mut self, agent: &AgentId, task: TaskId) -> Fence {
        let readback = self.read_back(agent, task);
        self.review(readback, Judgement::Match)
            .expect("review accepted");
        self.agent(agent, AgentCmd::Claim { task })
            .expect("claim accepted");
        self.fence(task).expect("claimed task has a fence")
    }

    pub fn fence(&self, task: TaskId) -> Option<Fence> {
        match &self.origin.world().task(task)?.state {
            TaskState::Claimed { claim, .. } => Some(claim.fence(task)),
            _ => None,
        }
    }

    pub fn declare(
        &mut self,
        agent: &AgentId,
        fence: Fence,
        footprint: DeclarationInput,
    ) -> Result<Vec<Stamped>, Rejection> {
        self.agent(agent, AgentCmd::Declare { fence, footprint })
    }

    /// Reviews `readback` as unclear and pins a question on the API machine.
    pub fn ask(&mut self, readback: ReadbackId) -> Result<Vec<Stamped>, Rejection> {
        let order = self.origin.world().order(self.order).expect("order exists");
        let rev = order.rev;
        let verdict = ReadbackVerdict {
            rev,
            per_witness: order
                .witnesses
                .iter()
                .map(|&w| {
                    (
                        w,
                        Judgement::Unclear {
                            reason: "retry semantics".to_owned(),
                        },
                    )
                })
                .collect(),
        };
        self.system(SystemCmd::ReadbacksReviewed {
            order: self.order,
            rev,
            verdicts: vec![(readback, verdict)],
            question: Some(QuestionDraft {
                at: Locus::Machine {
                    machine: machine("api"),
                },
                text: "Should a 429 be retried?".to_owned(),
                options: ["Never retry".to_owned(), "Retry once".to_owned()],
            }),
        })
    }

    /// Claims `task` and declares a footprint no other fixture task touches, so it clears to
    /// `Building`. When undeclared siblings hold the order's declare barrier, the clock runs out
    /// the barrier first; see [`Fx::run_out_barrier`].
    pub fn build(&mut self, agent: &AgentId, task: TaskId) -> Fence {
        let fence = self.claim(agent, task);
        let footprint = DeclarationInput {
            symbols: vec![sym("src/auth/login.rs", "login")],
            ..DeclarationInput::default()
        };
        self.declare(agent, fence, footprint)
            .expect("declare accepted");
        let order = self.origin.world().task(task).expect("task exists").order;
        if self.origin.world().barrier(order).is_some() {
            self.run_out_barrier(order);
        }
        fence
    }

    /// Runs the clock to `order`'s declare barrier deadline, renewing every live lease on the
    /// way so none expires, then ticks so the barrier lifts.
    pub fn run_out_barrier(&mut self, order: OrderId) {
        let Barrier::Armed { deadline } = self.origin.world().order(order).expect("order").barrier
        else {
            panic!("the barrier of {order} is not armed");
        };
        while self.now < deadline {
            let held: Vec<(AgentId, Fence)> = self
                .origin
                .world()
                .orders()
                .flat_map(|o| self.origin.world().tasks_of(o.id))
                .filter_map(|t| match &t.state {
                    TaskState::Claimed { claim, .. } => {
                        Some((claim.lease.holder.clone(), claim.fence(t.id)))
                    }
                    _ => None,
                })
                .collect();
            for (agent, fence) in held {
                self.agent(&agent, AgentCmd::Renew { fence })
                    .expect("renew accepted");
            }
            let left = deadline.as_millis() - self.now.as_millis();
            self.advance(left.min(LEASE_TTL_MS / 2));
        }
        self.system(SystemCmd::Tick).expect("tick accepted");
    }
}
