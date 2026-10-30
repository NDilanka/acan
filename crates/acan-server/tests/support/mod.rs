//! An in-memory world that runs the real origin. The model tests and the live example share it,
//! so every event here is one the origin accepts.

use std::collections::BTreeSet;
use std::str::FromStr;

use acan_core::{
    AgentCmd, AgentId, Belt, Check, ClashId, CodeIndex, CodeRev, Command, Ctx, DeclarationInput,
    Event, Fence, FilePath, HumanCmd, IndexedFile, IndexedSymbol, Locus, MachineKey, MemberId,
    OrderId, Origin, ReadbackId, Rejection, Stamped, SymbolRef, TaskId, TaskState, Timestamp,
    WitnessSpec, World, WorldId, WorldRef,
};

pub const UI: &str = "ui";
pub const API: &str = "api";
pub const GREET_FILE: &str = "src/ui/greet.rs";
pub const GREET: &str = "greet";

const WITNESS_COMMAND: &str = "cargo test -p greet";
const WITNESS_REASON: &str = "the greeting keeps its default";
const SUMMARIES: [&str; 2] = [
    "Read the greeting from the settings file, keep the default when the file has no entry, and run the greet tests.",
    "Add the greeting field to the settings file, keep the default when the file has no entry, and run the greet tests.",
];

/// The code index: a `ui` machine and an `api` machine, a belt from `api` to `ui`, and one
/// exported symbol per file.
pub fn code_index() -> CodeIndex {
    let ui = machine(UI);
    let api = machine(API);
    let mut index = CodeIndex::new(CodeRev::from_str("demo-rev-1").expect("valid code rev"));
    index.files.insert(
        FilePath::from_str(GREET_FILE).expect("valid file path"),
        IndexedFile {
            machine: ui.clone(),
            symbols: vec![IndexedSymbol {
                name: GREET.to_owned(),
                bytes: 0..120,
                exported: true,
            }],
        },
    );
    index.files.insert(
        FilePath::from_str("src/api/handler.rs").expect("valid file path"),
        IndexedFile {
            machine: api.clone(),
            symbols: vec![IndexedSymbol {
                name: "handle".to_owned(),
                bytes: 0..200,
                exported: true,
            }],
        },
    );
    index.belts.insert(Belt { from: api, to: ui });
    index
}

pub fn machine(name: &str) -> MachineKey {
    MachineKey::from_str(name).expect("valid machine key")
}

/// The symbol both demo tasks declare, which makes their declarations overlap.
pub fn greet_symbol() -> SymbolRef {
    SymbolRef {
        file: FilePath::from_str(GREET_FILE).expect("valid file path"),
        name: GREET.to_owned(),
    }
}

fn agent_pair() -> [AgentId; 2] {
    [
        AgentId::from_str("agent-a").expect("valid agent id"),
        AgentId::from_str("agent-b").expect("valid agent id"),
    ]
}

/// A world with one founder, the code index above, and the origin's clock.
pub struct Demo {
    origin: Origin,
    index: CodeIndex,
    founder: MemberId,
    now: Timestamp,
}

impl Demo {
    pub fn open() -> Self {
        let founder = MemberId::from_str("owner").expect("valid member id");
        let now = Timestamp::from_millis(1_800_000_000_000);
        let world = WorldRef {
            id: WorldId::from_str("demo").expect("valid world id"),
            repo: "demo".to_owned(),
        };
        let (origin, _opened) = Origin::open(world, founder.clone(), now);
        Self {
            origin,
            index: code_index(),
            founder,
            now,
        }
    }

    pub fn world(&self) -> &World {
        self.origin.world()
    }

    pub fn index(&self) -> &CodeIndex {
        &self.index
    }

    /// Runs one command through the origin and returns the events it accepted. Panics when the
    /// origin refuses it, since a test command must be one the origin accepts.
    pub fn apply(&mut self, cmd: Command) -> Vec<Stamped> {
        self.try_apply(cmd)
            .unwrap_or_else(|rejection| panic!("the origin refused a demo command: {rejection}"))
    }

    /// Runs one command through the origin. A refusal comes back with the origin's reason, for
    /// commands built from model replies, which may be refused.
    pub fn try_apply(&mut self, cmd: Command) -> Result<Vec<Stamped>, Rejection> {
        let ctx = Ctx {
            now: self.now,
            index: &self.index,
        };
        self.origin.execute(&ctx, cmd)
    }

    fn human(&mut self, cmd: HumanCmd) -> Vec<Stamped> {
        let member = self.founder.clone();
        self.apply(Command::Human { member, cmd })
    }

    /// Posts an order on the `ui` machine, pinned to one test witness, and returns its id.
    pub fn post_order(&mut self, sentence: &str) -> OrderId {
        let events = self.human(HumanCmd::PostOrder {
            machine: machine(UI),
            sentence: sentence.to_owned(),
            witnesses: vec![WitnessSpec {
                check: Check::Test {
                    command: WITNESS_COMMAND.to_owned(),
                },
                reason: WITNESS_REASON.to_owned(),
            }],
        });
        events
            .iter()
            .find_map(|stamped| match &stamped.event {
                Event::OrderPosted { order, .. } => Some(*order),
                _ => None,
            })
            .expect("posting an order emits OrderPosted")
    }

    /// The tasks of `order`, in id order.
    pub fn tasks_of(&self, order: OrderId) -> Vec<TaskId> {
        self.world().tasks_of(order).map(|task| task.id).collect()
    }

    fn register(&mut self, agent: &AgentId) {
        self.human(HumanCmd::RegisterAgent {
            agent: agent.clone(),
        });
    }

    /// Posts `agent`'s readback of `task` and returns its id.
    fn read_back(&mut self, agent: &AgentId, task: TaskId, summary: &str) -> ReadbackId {
        let events = self.apply(Command::Agent {
            agent: agent.clone(),
            cmd: AgentCmd::PostReadback {
                task,
                summary: summary.to_owned(),
                touches: vec![Locus::Machine {
                    machine: machine(UI),
                }],
            },
        });
        events
            .iter()
            .find_map(|stamped| match &stamped.event {
                Event::ReadbackPosted { readback } => Some(readback.id),
                _ => None,
            })
            .expect("posting a readback emits ReadbackPosted")
    }

    fn claim(&mut self, agent: &AgentId, task: TaskId) -> Fence {
        self.apply(Command::Agent {
            agent: agent.clone(),
            cmd: AgentCmd::Claim { task },
        });
        match &self.world().task(task).expect("the task exists").state {
            TaskState::Claimed { claim, .. } => claim.fence(task),
            other => panic!("the claim left {task} as {other:?}"),
        }
    }

    fn declare(&mut self, agent: &AgentId, fence: Fence, symbols: Vec<SymbolRef>) {
        self.apply(Command::Agent {
            agent: agent.clone(),
            cmd: AgentCmd::Declare {
                fence,
                footprint: DeclarationInput {
                    symbols,
                    ..DeclarationInput::default()
                },
            },
        });
    }

    fn clash(&self) -> Option<ClashId> {
        self.world().clashes().next().map(|clash| clash.id)
    }
}

/// Registers the agent pair and posts one readback on each of two tasks. Returns the readback
/// ids.
pub fn read_back(demo: &mut Demo, tasks: [TaskId; 2]) -> BTreeSet<ReadbackId> {
    let agents = agent_pair();
    let mut ids = BTreeSet::new();
    for ((agent, task), summary) in agents.iter().zip(tasks).zip(SUMMARIES) {
        demo.register(agent);
        ids.insert(demo.read_back(agent, task, summary));
    }
    ids
}

/// Claims both tasks under their readbacks, then declares the same symbol on each. The two
/// declarations overlap, which raises a clash. The readbacks must already carry matching verdicts.
pub fn claim_and_declare(demo: &mut Demo, tasks: [TaskId; 2]) -> ClashId {
    let agents = agent_pair();
    for (agent, task) in agents.iter().zip(tasks) {
        let fence = demo.claim(agent, task);
        demo.declare(agent, fence, vec![greet_symbol()]);
    }
    demo.clash()
        .expect("two declarations of one symbol raise a clash")
}
