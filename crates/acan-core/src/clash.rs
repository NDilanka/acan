//! Declarations, pure clash detection, and settlement.
//!
//! [`detect`] is a pure function of declarations and a [`CodeIndex`]. The origin calls it
//! inside `execute` and records its blocking results as `ClashFound` and `ClashDissolved`
//! events, so replicas never re-run it and never need the index.
//!
//! ```text
//! (none) --ClashFound--> Open
//! Open --ClashExplained--> Open (explanation attached; settling never waits for it)
//! Open --SettleClash with Propose right--> Proposed
//! Open|Proposed --SettleClash with Settle right--> Settled
//! Open|Proposed --redetection no longer finds the key, or a party has no declaration--> Dissolved
//! Settled(Serialize|SplitAtContract) --the same, unless its first task is queued to land--> Dissolved
//! ```
//!
//! Building waits on the order's declare [`Barrier`] as well as on clashes, so siblings that
//! overlap are detected while both still only hold declarations.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use serde::{Deserialize, Serialize};

use crate::event::{Event, Rejection};
use crate::ids::{
    AgentId, ClashId, CodeRev, FilePath, Generation, MachineKey, MemberId, TaskId, Timestamp,
    WitnessId,
};
use crate::index::{CodeIndex, Locus};
use crate::lease::{self, Fence, LEASE_TTL_MS};
use crate::member::SettleRight;
use crate::order::{OrderState, Stage, TaskState};
use crate::world::{Ctx, World};

/// File names that are produced by a tool and must have one producer. Matched on the last path segment.
pub(crate) const GENERATED_FILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
];

/// An order's declare barrier. While it holds, no task of the order is cleared to build, so
/// siblings with overlapping footprints meet as a clash before either one writes.
///
/// ```text
/// Unarmed --the order's first Declared, stamped at t--> Armed{deadline = t + DECLARE_BARRIER_MS}
/// Armed --BuildCleared for any task of the order--> Lifted
/// Armed --now >= deadline (Tick or any command)--> Lifted     [BarrierExpired]
/// ```
///
/// An armed barrier holds while any task of the order is open or claimed without a declaration;
/// [`crate::World::barrier`] reports what it waits for. Once every sibling has declared or is
/// queued, landed or dropped, it stops holding, and the first task cleared lifts it. Tasks that
/// all wait on clashes keep it armed until the deadline. It never re-arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "barrier", rename_all = "snake_case")]
pub enum Barrier {
    /// No task of the order has declared yet.
    Unarmed,
    /// Armed by the order's first declaration.
    Armed {
        /// When the barrier lifts even if a sibling never declares.
        deadline: Timestamp,
    },
    /// A task of the order was cleared to build, or the deadline passed.
    Lifted,
}

/// What an armed [`Barrier`] waits for while it holds. Every declared task of the order is
/// waiting for these siblings to declare.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarrierHold {
    /// When the barrier lifts even if they never declare.
    pub deadline: Timestamp,
    /// The order's tasks with no declaration: open, or claimed and still declaring.
    pub waiting_for: Vec<TaskId>,
}

/// A symbol named by file and name. The origin resolves it to byte ranges through the index.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SymbolRef {
    /// The file.
    pub file: FilePath,
    /// The symbol name.
    pub name: String,
}

/// What an agent says it will change, before it writes code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclarationInput {
    /// Files the agent may rewrite anywhere. Coverage is unknown, so these overlap with anything in the file.
    pub files: Vec<FilePath>,
    /// Symbols whose bodies the agent will change.
    pub symbols: Vec<SymbolRef>,
    /// Exported symbols whose signature or contract the agent will change.
    pub interfaces: Vec<SymbolRef>,
    /// Files the agent will only append to: a route table, a registry, a barrel file.
    pub inserts: Vec<FilePath>,
}

impl DeclarationInput {
    /// Whether the footprint names nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.symbols.is_empty()
            && self.interfaces.is_empty()
            && self.inserts.is_empty()
    }
}

/// An accepted declaration, bound to the claim generation and index revision it was made under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Declaration {
    /// The declaring task.
    pub task: TaskId,
    /// The claim generation it was declared under.
    pub generation: Generation,
    /// The index revision current when it was accepted.
    pub index: CodeRev,
    /// What the agent will change.
    pub footprint: DeclarationInput,
}

/// How two declarations overlap, after the agent-collision taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapKind {
    /// Both change the same symbol's byte range. Blocking.
    SharedImplementation,
    /// One changes an exported symbol in machine M; the other's footprint is in a machine that imports M. Blocking.
    Contract,
    /// Both touch a generated file such as a lockfile. Blocking: one producer.
    GeneratedOutput,
    /// Both append to the same insertion file. Blocking, cheap to settle.
    SharedInsertion,
    /// A whole-file write (or an unresolved symbol) meets anything in that file. Blocking.
    WholeFile,
    /// Same file, non-overlapping symbols. Advisory: never recorded as a clash.
    DisjointRegions,
}

impl OverlapKind {
    /// Whether this kind blocks building until settled.
    #[must_use]
    pub fn blocking(self) -> bool {
        !matches!(self, Self::DisjointRegions)
    }
}

/// The identity of an overlap: an ordered task pair and a place.
///
/// The lower task id is always `a`, so detection is deterministic and re-detection of the
/// same overlap maps to the same clash.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ClashKey {
    a: TaskId,
    b: TaskId,
    at: Locus,
}

impl ClashKey {
    /// Builds a key for two distinct tasks, ordering them lower first.
    ///
    /// # Panics
    /// Panics if `x == y`; a task never clashes with itself.
    #[must_use]
    pub fn new(x: TaskId, y: TaskId, at: Locus) -> Self {
        assert_ne!(x, y, "a clash needs two different tasks, got {x} twice");
        let (a, b) = if x < y { (x, y) } else { (y, x) };
        Self { a, b, at }
    }

    /// The lower task id.
    #[must_use]
    pub fn a(&self) -> TaskId {
        self.a
    }

    /// The higher task id.
    #[must_use]
    pub fn b(&self) -> TaskId {
        self.b
    }

    /// Where the overlap is.
    #[must_use]
    pub fn at(&self) -> &Locus {
        &self.at
    }

    /// Whether `task` is one of the two parties.
    #[must_use]
    pub fn involves(&self, task: TaskId) -> bool {
        self.a == task || self.b == task
    }

    /// The party that is not `task`.
    fn other(&self, task: TaskId) -> TaskId {
        if task == self.a { self.b } else { self.a }
    }
}

/// One overlap found by [`detect`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Overlap {
    /// The tasks and place.
    pub key: ClashKey,
    /// How they overlap.
    pub kind: OverlapKind,
}

/// Finds every overlap between declarations of different tasks.
///
/// Pure: no I/O, no model, no clock. The output is sorted by key, then kind, and does not
/// depend on the order of `decls`. Rules for each pair of declarations from different tasks:
///
/// - A symbol that does not resolve in `index` counts as a whole-file write of its file.
/// - Same symbol range (ranges intersect) in one file: `SharedImplementation` at that symbol.
/// - Both list the file in `inserts`: `SharedInsertion` at the file.
/// - Both touch a file in the generated list: `GeneratedOutput` at the file.
/// - One side writes the whole file and the other touches it at all: `WholeFile` at the file.
/// - Same file, only disjoint symbols: `DisjointRegions` at the file.
/// - A lists an exported interface symbol in machine M, and any of B's files or symbols is
///   in a machine N with a belt `N -> M`: `Contract` at that belt.
///
/// At most one overlap is reported per key; when several rules hit the same key, the first
/// kind in [`OverlapKind`] order wins.
#[must_use]
pub fn detect(decls: &[&Declaration], index: &CodeIndex) -> Vec<Overlap> {
    hits(decls, index)
        .into_iter()
        .map(|(key, hit)| Overlap {
            key,
            kind: hit.kind,
        })
        .collect()
}

/// The task that changes the interface in a `Contract` overlap, and the interface itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContractRef {
    /// The task changing the exported interface.
    pub owner: TaskId,
    /// The interface symbol. The contract is named after it.
    pub interface: SymbolRef,
}

/// The winning overlap at one key. Ordered by kind first, so the minimum is the kind that wins.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Hit {
    pub(crate) kind: OverlapKind,
    /// Set exactly when `kind` is `Contract`.
    pub(crate) contract: Option<ContractRef>,
}

impl Hit {
    fn plain(kind: OverlapKind) -> Self {
        Self {
            kind,
            contract: None,
        }
    }
}

/// [`detect`] with the contract owner kept for each `Contract` hit.
pub(crate) fn hits(decls: &[&Declaration], index: &CodeIndex) -> BTreeMap<ClashKey, Hit> {
    let resolved: Vec<Resolved<'_>> = decls.iter().map(|d| Resolved::new(d, index)).collect();
    let mut found = BTreeMap::new();
    for (i, x) in resolved.iter().enumerate() {
        for y in &resolved[i + 1..] {
            if x.task == y.task {
                continue;
            }
            let (a, b) = if x.task < y.task { (x, y) } else { (y, x) };
            file_overlaps(a, b, &mut found);
            contract_overlaps(a, b, index, &mut found);
            contract_overlaps(b, a, index, &mut found);
        }
    }
    found
}

/// One declaration resolved against the index.
struct Resolved<'a> {
    task: TaskId,
    /// Files written anywhere: listed files, plus files of symbols that did not resolve.
    whole: BTreeSet<&'a FilePath>,
    /// Resolved symbol and interface ranges per file, with the declared name.
    ranges: BTreeMap<&'a FilePath, Vec<(&'a str, Range<u32>)>>,
    inserts: BTreeSet<&'a FilePath>,
    /// Interfaces that resolve to an exported symbol, with the machine that owns them.
    exported: Vec<(&'a MachineKey, &'a SymbolRef)>,
}

impl<'a> Resolved<'a> {
    fn new(decl: &'a Declaration, index: &'a CodeIndex) -> Self {
        let footprint = &decl.footprint;
        let mut resolved = Self {
            task: decl.task,
            whole: footprint.files.iter().collect(),
            ranges: BTreeMap::new(),
            inserts: footprint.inserts.iter().collect(),
            exported: Vec::new(),
        };
        for sym in footprint.symbols.iter().chain(&footprint.interfaces) {
            let mut matched = index.symbols(&sym.file, &sym.name).peekable();
            if matched.peek().is_none() {
                resolved.whole.insert(&sym.file);
            }
            let ranges = resolved.ranges.entry(&sym.file).or_default();
            ranges.extend(matched.map(|s| (sym.name.as_str(), s.bytes.clone())));
        }
        for sym in &footprint.interfaces {
            let exported = index.symbols(&sym.file, &sym.name).any(|s| s.exported);
            if exported && let Some(machine) = index.machine_of(&sym.file) {
                resolved.exported.push((machine, sym));
            }
        }
        resolved
    }

    fn touched(&self) -> BTreeSet<&'a FilePath> {
        self.whole
            .iter()
            .chain(self.ranges.keys())
            .chain(&self.inserts)
            .copied()
            .collect()
    }
}

fn add(found: &mut BTreeMap<ClashKey, Hit>, key: ClashKey, hit: Hit) {
    match found.get_mut(&key) {
        Some(existing) if *existing <= hit => {}
        Some(existing) => *existing = hit,
        None => {
            found.insert(key, hit);
        }
    }
}

/// Overlaps inside files both declarations touch. `a` is the lower task.
fn file_overlaps(a: &Resolved<'_>, b: &Resolved<'_>, found: &mut BTreeMap<ClashKey, Hit>) {
    for file in a.touched().intersection(&b.touched()) {
        let key = |at: Locus| ClashKey::new(a.task, b.task, at);
        let at_file = || Locus::File {
            file: (*file).clone(),
        };
        let mut blocking = Vec::new();
        if a.inserts.contains(file) && b.inserts.contains(file) {
            blocking.push(OverlapKind::SharedInsertion);
        }
        if is_generated(file) {
            blocking.push(OverlapKind::GeneratedOutput);
        }
        if a.whole.contains(file) || b.whole.contains(file) {
            blocking.push(OverlapKind::WholeFile);
        }
        let (a_ranges, b_ranges) = (a.ranges.get(file), b.ranges.get(file));
        let shared: BTreeSet<&str> = a_ranges
            .into_iter()
            .flatten()
            .filter(|(_, x)| {
                b_ranges
                    .into_iter()
                    .flatten()
                    .any(|(_, y)| x.start < y.end && y.start < x.end)
            })
            .map(|(name, _)| *name)
            .collect();
        for name in &shared {
            let at = Locus::Symbol {
                file: (*file).clone(),
                name: (*name).to_owned(),
            };
            add(
                found,
                key(at),
                Hit::plain(OverlapKind::SharedImplementation),
            );
        }
        for kind in &blocking {
            add(found, key(at_file()), Hit::plain(*kind));
        }
        if blocking.is_empty() && shared.is_empty() {
            add(
                found,
                key(at_file()),
                Hit::plain(OverlapKind::DisjointRegions),
            );
        }
    }
}

/// `Contract` overlaps where `owner` changes an exported interface that `other`'s machines import.
fn contract_overlaps(
    owner: &Resolved<'_>,
    other: &Resolved<'_>,
    index: &CodeIndex,
    found: &mut BTreeMap<ClashKey, Hit>,
) {
    if owner.exported.is_empty() {
        return;
    }
    let importers: BTreeSet<&MachineKey> = other
        .touched()
        .into_iter()
        .filter_map(|file| index.machine_of(file))
        .collect();
    for (machine, interface) in &owner.exported {
        for belt in index
            .belts
            .iter()
            .filter(|b| &b.to == *machine && importers.contains(&b.from))
        {
            let at = Locus::Belt {
                from: belt.from.clone(),
                to: belt.to.clone(),
            };
            let hit = Hit {
                kind: OverlapKind::Contract,
                contract: Some(ContractRef {
                    owner: owner.task,
                    interface: (*interface).clone(),
                }),
            };
            add(found, ClashKey::new(owner.task, other.task, at), hit);
        }
    }
}

fn is_generated(file: &FilePath) -> bool {
    let name = file.as_str().rsplit('/').next().unwrap_or_default();
    GENERATED_FILES.contains(&name)
}

/// A recorded blocking overlap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clash {
    /// The clash id.
    pub id: ClashId,
    /// The tasks and place.
    pub key: ClashKey,
    /// How they overlap.
    pub kind: OverlapKind,
    /// The index revision detection ran against.
    pub found_at: CodeRev,
    /// Who owns the interface, for a `Contract` clash. `None` for every other kind.
    pub contract: Option<ContractRef>,
    /// The foreman's explanation, once the explain job has run.
    pub explanation: Option<Explanation>,
    /// Where the clash is in its lifecycle.
    pub state: ClashState,
}

/// A clash's lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ClashState {
    /// Blocking; no choice yet.
    Open,
    /// Blocking; a member without the right to settle proposed a choice.
    Proposed {
        /// The proposed choice.
        choice: Settlement,
        /// Who proposed it.
        by: MemberId,
    },
    /// Settled. Its effect is applied.
    Settled {
        /// The chosen settlement.
        choice: Settlement,
        /// Who settled it.
        by: MemberId,
    },
    /// A later declaration or rescan removed the overlap.
    Dissolved,
}

/// The foreman's account of a clash, written by a model. It never changes the legal menu.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Explanation {
    /// What goes wrong if both proceed.
    pub consequence: String,
    /// The witness that would break, if any.
    pub breaks: Option<WitnessId>,
    /// The suggested choice. Ignored unless it is on [`Clash::menu`].
    pub suggested: Option<Settlement>,
}

/// A way to settle a clash.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "choice", rename_all = "snake_case")]
pub enum Settlement {
    /// `task` redeclares without the overlapping place. Its task returns to `Declaring`.
    Narrow {
        /// The task that narrows.
        task: TaskId,
    },
    /// `then` may not build until `first` has landed.
    Serialize {
        /// The task that goes first.
        first: TaskId,
        /// The task that waits.
        then: TaskId,
    },
    /// `owner` lands the named contract first; the other task builds against it.
    SplitAtContract {
        /// The task that owns the contract.
        owner: TaskId,
        /// The contract, named after the interface symbol.
        contract: String,
    },
}

impl Settlement {
    /// The task that gives way under this choice. `None` if the choice does not fit `key`.
    #[must_use]
    pub fn yielder(&self, key: &ClashKey) -> Option<TaskId> {
        match *self {
            Self::Narrow { task } => key.involves(task).then_some(task),
            Self::Serialize { first, then } => {
                (first != then && key.involves(first) && key.involves(then)).then_some(then)
            }
            Self::SplitAtContract { owner, .. } => key.involves(owner).then(|| key.other(owner)),
        }
    }

    /// The task that must land before `task` may build under this settlement.
    fn predecessor_of(&self, task: TaskId, key: &ClashKey) -> Option<TaskId> {
        self.lands_first()
            .filter(|&first| first != task && key.involves(task))
    }

    /// The task that lands first under this settlement. `None` for `Narrow`, which orders nothing.
    fn lands_first(&self) -> Option<TaskId> {
        match *self {
            Self::Narrow { .. } => None,
            Self::Serialize { first, .. } => Some(first),
            Self::SplitAtContract { owner, .. } => Some(owner),
        }
    }
}

impl Clash {
    /// The legal choices for this clash, two or three, derived from its kind and key.
    ///
    /// `SharedImplementation`, `GeneratedOutput`, `SharedInsertion`, `WholeFile`: narrow `a`,
    /// narrow `b`, serialize `a` then `b`. `Contract`: split at the contract owned by the
    /// task that changes the interface, or serialize that task first.
    #[must_use]
    pub fn menu(&self) -> Vec<Settlement> {
        let (a, b) = (self.key.a, self.key.b);
        match (&self.kind, &self.contract) {
            (OverlapKind::Contract, Some(contract)) => vec![
                Settlement::SplitAtContract {
                    owner: contract.owner,
                    contract: contract.interface.name.clone(),
                },
                Settlement::Serialize {
                    first: contract.owner,
                    then: self.key.other(contract.owner),
                },
            ],
            _ => vec![
                Settlement::Narrow { task: a },
                Settlement::Narrow { task: b },
                Settlement::Serialize { first: a, then: b },
            ],
        }
    }

    /// Whether this clash still blocks its tasks from building.
    #[must_use]
    pub fn blocking(&self) -> bool {
        matches!(self.state, ClashState::Open | ClashState::Proposed { .. })
    }

    /// Whether this clash should dissolve once a party loses its declaration or the overlap
    /// is no longer found: it is blocking, or it is a settled landing order whose first task
    /// has not yet delivered. A delivered first task has no declaration left, but its waiter
    /// must still hold until it lands.
    fn dissolvable(&self, world: &World) -> bool {
        match &self.state {
            ClashState::Open | ClashState::Proposed { .. } => true,
            ClashState::Settled { choice, .. } => choice.lands_first().is_some_and(|first| {
                !world
                    .task(first)
                    .is_some_and(|t| matches!(t.state, TaskState::Queued { .. }))
            }),
            ClashState::Dissolved => false,
        }
    }

    /// Whether redetecting this clash's key should leave it alone. A narrow settlement does
    /// not stand: finding the overlap again means the narrowing task redeclared it.
    fn stands(&self) -> bool {
        match &self.state {
            ClashState::Open | ClashState::Proposed { .. } => true,
            ClashState::Settled { choice, .. } => !matches!(choice, Settlement::Narrow { .. }),
            ClashState::Dissolved => false,
        }
    }
}

/// `Declare`: checks the fence, stores the declaration, runs [`detect`] over every live
/// declaration, and emits `ClashFound` for new keys and `ClashDissolved` for vanished ones.
/// Allowed in `Declaring`, `Declared` and `Building`; the task moves to `Declared`.
/// Redeclaring the footprint already held is a no-op.
pub(crate) fn decide_declare(
    world: &World,
    ctx: &Ctx<'_>,
    agent: &AgentId,
    fence: Fence,
    footprint: DeclarationInput,
) -> Result<Vec<Event>, Rejection> {
    let stage = lease::check_fence(world, ctx, agent, fence)?;
    if let Stage::Gating { .. } = stage {
        return Err(Rejection::wrong_state("task", stage.name()));
    }
    if footprint.is_empty() {
        return Err(Rejection::invalid(
            "a declaration must name what it changes",
        ));
    }
    if stage
        .declaration()
        .is_some_and(|d| d.footprint == footprint)
    {
        return Ok(Vec::new());
    }
    let decl = Declaration {
        task: fence.task,
        generation: fence.generation,
        index: ctx.index.rev.clone(),
        footprint,
    };
    let redetected = {
        let mut decls: Vec<&Declaration> = world
            .live_declarations()
            .filter(|d| d.task != fence.task)
            .collect();
        decls.push(&decl);
        redetect(world, ctx, &decls)?
    };
    let mut events = vec![Event::Declared { decl }];
    events.extend(redetected);
    Ok(events)
}

/// `IndexUpdated`: re-runs detection over live declarations against the new index.
/// A revision already recorded is a no-op, since equal revisions mean equal contents.
pub(crate) fn decide_index_updated(world: &World, ctx: &Ctx<'_>) -> Result<Vec<Event>, Rejection> {
    if world.index_rev() == Some(&ctx.index.rev) {
        return Ok(Vec::new());
    }
    let decls: Vec<&Declaration> = world.live_declarations().collect();
    let mut events = vec![Event::IndexUpdated {
        rev: ctx.index.rev.clone(),
    }];
    events.extend(redetect(world, ctx, &decls)?);
    Ok(events)
}

/// `ClashFound` for each blocking key without a standing clash, and `ClashDissolved` for
/// each dissolvable clash whose key is no longer found.
///
/// A new clash abandons any gating run of its parties. Their leases did not run down while
/// gating, so each is renewed from `ctx.now` rather than expiring at once.
fn redetect(world: &World, ctx: &Ctx<'_>, decls: &[&Declaration]) -> Result<Vec<Event>, Rejection> {
    let found = hits(decls, ctx.index);
    let blocking: BTreeMap<&ClashKey, &Hit> = found
        .iter()
        .filter(|(_, hit)| hit.kind.blocking())
        .collect();
    let new: Vec<(&ClashKey, &Hit)> = blocking
        .iter()
        .filter(|(key, _)| {
            !world
                .clash_keys
                .get(**key)
                .and_then(|id| world.clash(*id))
                .is_some_and(Clash::stands)
        })
        .map(|(key, hit)| (*key, *hit))
        .collect();
    let ids = world.next_clash_ids(new.len())?;
    let mut events: Vec<Event> = ids
        .into_iter()
        .zip(&new)
        .map(|(clash, (key, hit))| Event::ClashFound {
            clash,
            key: (*key).clone(),
            kind: hit.kind,
            found_at: ctx.index.rev.clone(),
            contract: hit.contract.clone(),
        })
        .collect();
    let abandoned: BTreeSet<(TaskId, Generation)> = new
        .iter()
        .flat_map(|(key, _)| [key.a, key.b])
        .filter_map(|task| match &world.task(task)?.state {
            TaskState::Claimed {
                claim,
                stage: Stage::Gating { .. },
            } => Some((task, claim.lease.generation)),
            _ => None,
        })
        .collect();
    events.extend(
        abandoned
            .into_iter()
            .map(|(task, generation)| Event::LeaseRenewed {
                task,
                generation,
                expires_at: ctx.now.plus_millis(LEASE_TTL_MS),
            }),
    );
    events.extend(
        world
            .clashes()
            .filter(|c| c.dissolvable(world) && !blocking.contains_key(&c.key))
            .map(|c| Event::ClashDissolved { clash: c.id }),
    );
    Ok(events)
}

/// `ClashDissolved` for every dissolvable clash a party of which no longer holds a
/// declaration: its lease ended or was delivered, its order was cancelled, or it narrowed.
/// The origin runs this after every accepted command, so neither a blocking clash nor a
/// landing order outlives the claims that made it.
pub(crate) fn dissolve_orphans(world: &World) -> Vec<Event> {
    let declared = |task: TaskId| {
        world
            .task(task)
            .is_some_and(|t| matches!(&t.state, TaskState::Claimed { stage, .. } if stage.declaration().is_some()))
    };
    world
        .clashes()
        .filter(|c| c.dissolvable(world) && !(declared(c.key.a) && declared(c.key.b)))
        .map(|c| Event::ClashDissolved { clash: c.id })
        .collect()
}

/// `SettleClash`: the choice must be on the menu. Emits `ClashSettled` or `ClashProposed`
/// according to [`World::settle_right`].
pub(crate) fn decide_settle(
    world: &World,
    member: &MemberId,
    clash: ClashId,
    choice: Settlement,
) -> Result<Vec<Event>, Rejection> {
    let c = world
        .clash(clash)
        .ok_or_else(|| Rejection::unknown("clash", clash))?;
    match &c.state {
        ClashState::Open | ClashState::Proposed { .. } => {}
        ClashState::Settled { .. } => return Err(Rejection::wrong_state("clash", "settled")),
        ClashState::Dissolved => return Err(Rejection::wrong_state("clash", "dissolved")),
    }
    let right = world
        .settle_right(member, c, &choice)
        .ok_or_else(|| Rejection::invalid("the choice is not on the clash's menu"))?;
    let by = member.clone();
    let event = match right {
        SettleRight::Settle => Event::ClashSettled { clash, choice, by },
        SettleRight::Propose => {
            let already = matches!(&c.state, ClashState::Proposed { choice: p, by: b } if *p == choice && b == member);
            if already {
                return Ok(Vec::new());
            }
            Event::ClashProposed { clash, choice, by }
        }
    };
    Ok(vec![event])
}

/// `ClashExplained`: attaches the model's explanation to an open or proposed clash.
/// A suggestion off the menu is dropped, so the UI never offers an illegal choice.
pub(crate) fn decide_explained(
    world: &World,
    clash: ClashId,
    mut explanation: Explanation,
) -> Result<Vec<Event>, Rejection> {
    let c = world
        .clash(clash)
        .ok_or_else(|| Rejection::unknown("clash", clash))?;
    if !c.blocking() {
        return Err(Rejection::wrong_state("clash", "closed"));
    }
    if c.explanation.is_some() {
        return Err(Rejection::wrong_state("clash", "explained"));
    }
    let menu = c.menu();
    if explanation
        .suggested
        .as_ref()
        .is_some_and(|s| !menu.contains(s))
    {
        explanation.suggested = None;
    }
    Ok(vec![Event::ClashExplained { clash, explanation }])
}

/// `BarrierExpired` for every armed barrier of a split order whose deadline is at or before
/// `ctx.now`. Runs before every command, as lease expiry does.
pub(crate) fn expire_barriers(world: &World, ctx: &Ctx<'_>) -> Vec<Event> {
    world
        .orders()
        .filter(|o| {
            o.state == OrderState::Split
                && matches!(o.barrier, Barrier::Armed { deadline } if ctx.now >= deadline)
        })
        .map(|o| Event::BarrierExpired { order: o.id })
        .collect()
}

/// `BuildCleared` for every `Declared` task whose order's barrier does not hold, with no
/// blocking clash and no `Serialize` predecessor that is still to land, in a lane that is not
/// stopped: a stopped lane clears no build. The origin runs this after every accepted command.
pub(crate) fn build_clear(world: &World) -> Vec<Event> {
    let waiting_on = |first: TaskId| {
        world
            .task(first)
            .is_some_and(|t| !matches!(t.state, TaskState::Landed { .. } | TaskState::Dropped))
    };
    let blocked = |task: TaskId| {
        world.clashes_for(task).any(|c| {
            c.blocking()
                || matches!(&c.state, ClashState::Settled { choice, .. }
                    if choice.predecessor_of(task, &c.key).is_some_and(waiting_on))
        })
    };
    world
        .tasks
        .values()
        .filter(|t| {
            matches!(
                t.state,
                TaskState::Claimed {
                    stage: Stage::Declared { .. },
                    ..
                }
            ) && !blocked(t.id)
                && world.barrier(t.order).is_none()
                && !world.lane(t.order).is_some_and(|lane| lane.stopped)
        })
        .map(|t| Event::BuildCleared { task: t.id })
        .collect()
}

#[cfg(test)]
mod tests {
    use acan_intent::Scope;
    use serde_json::json;

    use super::*;
    use crate::ids::{OrderId, OrderRev, ReadbackId, WorldId};
    use crate::lane::ScopeLane;
    use crate::lease::{Claim, Lease};
    use crate::order::{Order, Task, TaskSpec};
    use crate::world::{Origin, WorldRef};

    /// One split order with one task that holds a claim and a declaration. The order's barrier
    /// is lifted and the task has no clash, so only the lane's stop flag can hold its build back.
    ///
    /// The stop batch ends every lease in the lane before the build pass runs, so no command
    /// sequence leaves a declared task on a stopped lane. The world is built directly instead.
    fn declared_world() -> (World, TaskId, OrderId) {
        let founder: MemberId = "ada".parse().expect("a member id");
        let world_ref = WorldRef {
            id: "world-1".parse::<WorldId>().expect("a world id"),
            repo: "acan".to_owned(),
        };
        let (origin, _) = Origin::open(world_ref, founder.clone(), Timestamp::from_millis(0));
        let mut world = origin.world().clone();

        let order = OrderId::new(1);
        let task = TaskId::new(1);
        let agent: AgentId = "ada-agent".parse().expect("an agent id");
        let scope: Scope = serde_json::from_value(json!({
            "version": "acan-scope/1",
            "order": "o-1",
            "rev": 1,
            "base_commit": "1111111111111111111111111111111111111111",
            "body": {
                "may": [{
                    "id": "i-limit",
                    "selector": {
                        "surface": "http", "method": "POST", "route_template": "/api/login",
                        "step_context": {"kind": "step", "step_id": "s6"},
                        "principal": {"kind": "anonymous"}, "channel": "status"
                    },
                    "change": "add",
                    "acceptance": [{
                        "obligation_id": "obl-limit",
                        "predicate": {"predicate": "present", "at": {"ref": "field", "run": "candidate", "step_id": "s6", "path": "status"}},
                        "text": "the 429 status shows"
                    }],
                    "provenance": "user",
                    "example": {"kind": "target", "step_id": "s6", "path": "status", "after": {"state": "present", "value": 429}},
                    "machine_ids": ["api"]
                }],
                "must_keep": [],
                "open": []
            }
        }))
        .expect("the scope decodes");
        let scope_hash = scope.scope_hash().expect("the scope hashes");

        world.orders.insert(
            order,
            Order {
                id: order,
                machine: "api".parse().expect("a machine key"),
                sentence: "limit failed logins".to_owned(),
                rev: OrderRev::new(0),
                witnesses: Vec::new(),
                posted_by: founder,
                state: OrderState::Split,
                barrier: Barrier::Lifted,
            },
        );
        world
            .lanes
            .insert(order, ScopeLane::opened(order, scope, scope_hash));
        world.tasks.insert(
            task,
            Task {
                id: task,
                order,
                spec: TaskSpec {
                    title: "login rate limit".to_owned(),
                    brief: "answer 429 after repeated failures".to_owned(),
                    scope: Vec::new(),
                },
                state: TaskState::Claimed {
                    claim: Claim {
                        lease: Lease {
                            holder: agent,
                            generation: Generation::new(1),
                            expires_at: Timestamp::from_millis(60_000),
                        },
                        readback: ReadbackId::new(1),
                    },
                    stage: Stage::Declared {
                        decl: Declaration {
                            task,
                            generation: Generation::new(1),
                            index: "rev-1".parse().expect("a code rev"),
                            footprint: DeclarationInput {
                                files: vec!["src/auth/login.rs".parse().expect("a repo path")],
                                ..DeclarationInput::default()
                            },
                        },
                    },
                },
                last_generation: Some(Generation::new(1)),
            },
        );
        (world, task, order)
    }

    /// A stopped lane clears no build for a declared task. The same world without the stop
    /// clears it, so the lane's stop flag is what holds the build back.
    #[test]
    fn a_stopped_lane_clears_no_build_for_a_declared_task() {
        let (mut world, task, order) = declared_world();
        let cleared = |world: &World| {
            build_clear(world)
                .iter()
                .any(|event| matches!(event, Event::BuildCleared { task: t } if *t == task))
        };
        assert!(
            cleared(&world),
            "the unstopped lane clears the declared task"
        );

        world.lanes.get_mut(&order).expect("the lane").stopped = true;
        assert!(
            !cleared(&world),
            "a stopped lane clears no build for the declared task"
        );
    }
}
