// Node tests for the agent lanes strip: who was last heard of when, the rows, the stale flag, the
// empty state and the G chord. Run with:
//   node --test crates/acan-render-web/tests/js/lanes.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  GO_MS,
  AWAY_MS,
  STALE_MS,
  agoWords,
  checkWords,
  emptyLines,
  goStep,
  highlightLane,
  listenForDroneHover,
  awayStamp,
  laneRows,
  lastActivity,
  quietState,
  waitingAgents,
  workerLines,
} from "../../www/lanes.js";
import { steps, workerCommand } from "../../www/firstrun.js";

const T0 = 1_760_000_000_000;

function stamped(seq, at, actor, event) {
  return { seq, at, actor, event };
}

const SYSTEM = { kind: "system" };
const OWNER = { kind: "member", id: "owner" };
const agent = (id) => ({ kind: "agent", id });

// Runs a test body with navigator stubbed to nav, so the worker command follows the stub's platform
// and not the host's. The real navigator is put back after the body, however it ends.
const POSIX = { platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; Linux x86_64)" };
function onPlatform(nav, body) {
  return () => {
    const had = Object.getOwnPropertyDescriptor(globalThis, "navigator");
    Object.defineProperty(globalThis, "navigator", { value: nav, configurable: true, writable: true });
    try {
      body();
    } finally {
      if (had) Object.defineProperty(globalThis, "navigator", had);
      else delete globalThis.navigator;
    }
  };
}

// Two agents register; agent-a takes t-1 and renews; agent-b takes t-2, and its task lands later
// under a system event. agent-c registers and never works.
function log() {
  return [
    stamped(0, T0, OWNER, { type: "world_opened" }),
    stamped(1, T0 + 1000, OWNER, { type: "agent_registered", agent: { id: "agent-a", owner: "owner" } }),
    stamped(2, T0 + 2000, OWNER, { type: "agent_registered", agent: { id: "agent-b", owner: "owner" } }),
    stamped(3, T0 + 3000, OWNER, { type: "agent_registered", agent: { id: "agent-c", owner: "owner" } }),
    stamped(4, T0 + 10_000, agent("agent-a"), { type: "lease_granted", task: 1, lease: { holder: "agent-a", generation: 1 }, readback: 1 }),
    stamped(5, T0 + 12_000, agent("agent-b"), { type: "lease_granted", task: 2, lease: { holder: "agent-b", generation: 1 }, readback: 2 }),
    stamped(6, T0 + 30_000, agent("agent-a"), { type: "lease_renewed", task: 1, generation: 1 }),
    stamped(7, T0 + 40_000, SYSTEM, { type: "build_cleared", task: 2 }),
  ];
}

function view() {
  return {
    agents: [
      { id: "agent-b", task: 2, activity: "building", machine: "src/web", body: [0, 0, 1, 1] },
      { id: "agent-a", task: 1, activity: "gating", machine: "src/api", body: [0, 0, 1, 1] },
      { id: "agent-c", task: null, activity: "idle", machine: null, body: null },
    ],
    tasks: [
      { id: 1, order: 1, title: "add the retry header", state: "gating", agent: "agent-a", machine: "src/api", check: { name: "w-3", state: "passed" } },
      { id: 2, order: 1, title: "show the retry banner", state: "building", agent: "agent-b", machine: "src/web", check: null },
    ],
  };
}

test("an agent is heard of as the actor, when registered, and through the task it leases", () => {
  const seen = lastActivity(log());
  assert.deepEqual(seen.get("agent-a"), { at: T0 + 30_000, seq: 6 });
  // The build clearance is a system event, but it names agent-b's task.
  assert.deepEqual(seen.get("agent-b"), { at: T0 + 40_000, seq: 7 });
  assert.deepEqual(seen.get("agent-c"), { at: T0 + 3000, seq: 3 });
});

test("a task's events stop counting for its agent once the lease ends", () => {
  const events = [
    ...log(),
    stamped(8, T0 + 50_000, SYSTEM, { type: "lease_ended", task: 1, generation: 1, why: "expired" }),
    stamped(9, T0 + 90_000, SYSTEM, { type: "landed", task: 1, commit: "abc" }),
  ];
  assert.deepEqual(lastActivity(events).get("agent-a"), { at: T0 + 50_000, seq: 8 });
});

test("rows sort by name, carry the task, machine, activity and check, and number 1 to 9", () => {
  const rows = laneRows(view(), lastActivity(log()), T0 + 60_000);
  assert.deepEqual(rows.map((row) => row.agent), ["agent-a", "agent-b", "agent-c"]);
  assert.deepEqual(rows.map((row) => row.key), [1, 2, 3]);
  const [a, b, c] = rows;
  assert.equal(a.task, "t-1");
  assert.equal(a.title, "add the retry header");
  assert.equal(a.machine, "src/api");
  assert.equal(a.activity, "gating");
  assert.equal(a.ago, "30s ago");
  assert.deepEqual(a.check, { name: "w-3", state: "passed" });
  assert.equal(b.check, null);
  assert.equal(c.task, null);
  assert.equal(c.rideable, false);
  assert.equal(rows.some((row) => row.stale), false);
});

test("an agent holding a task is stale after STALE_MS with no event; an idle one never is", () => {
  const now = T0 + 30_000 + STALE_MS;
  const rows = laneRows(view(), lastActivity(log()), now);
  const byAgent = Object.fromEntries(rows.map((row) => [row.agent, row]));
  assert.equal(byAgent["agent-a"].stale, true);
  assert.equal(byAgent["agent-a"].ago, "2m ago");
  assert.equal(byAgent["agent-b"].stale, false, "agent-b was heard of 10 s later");
  assert.equal(byAgent["agent-c"].stale, false, "an idle agent has nothing to report");
  assert.equal(laneRows(view(), lastActivity(log()), now - 1)[0].stale, false);
});

test("an agent with no event yet reads as not heard from, and is not stale", () => {
  const rows = laneRows(view(), new Map(), T0);
  assert.equal(rows[0].ago, "not heard from");
  assert.equal(rows[0].stale, false);
});

test("only the first nine rows get a key", () => {
  const agents = Array.from({ length: 11 }, (_, i) => ({ id: `agent-${String(i).padStart(2, "0")}`, task: null, activity: "idle", machine: null, body: null }));
  const rows = laneRows({ agents, tasks: [] }, new Map(), T0);
  assert.deepEqual(rows.map((row) => row.key), [1, 2, 3, 4, 5, 6, 7, 8, 9, null, null]);
});

test("quiet times read in the largest whole unit", () => {
  assert.equal(agoWords(0), "just now");
  assert.equal(agoWords(9_999), "just now");
  assert.equal(agoWords(45_000), "45s ago");
  assert.equal(agoWords(120_000), "2m ago");
  assert.equal(agoWords(3 * 3600_000 + 5), "3h ago");
  assert.equal(agoWords(50 * 3600_000), "2d ago");
  assert.equal(agoWords(-5), "just now");
});

test("the check reads as words for the row's label", () => {
  assert.equal(checkWords({ name: "w-3", state: "failed" }), "check w-3 failed");
  assert.equal(checkWords(null), "");
});

test("with no agent the strip names the worker command against this origin", onPlatform(POSIX, () => {
  const { line, command } = emptyLines("http://127.0.0.1:7311");
  assert.equal(line, "No worker connected.");
  assert.equal(command, "ACAN_AGENT_TOKEN=<agent token from .acan/tokens.json> acan-worker --server http://127.0.0.1:7311 --repo <path to your repo>");
  assert.equal(command, workerCommand("http://127.0.0.1:7311"), "the checklist offers the same command");
  assert.doesNotMatch(line + command, /\$|cost|token usage/i);
}));

test("G then a digit rides; the digit alone and a late digit are left to the page", () => {
  const armed = goStep(null, "g", 100);
  assert.deepEqual(armed, { armed: 100, taken: true, action: null });
  assert.deepEqual(goStep(armed.armed, "2", 200), { armed: null, taken: true, action: { kind: "agent", index: 1 } });
  assert.equal(goStep(null, "2", 200).taken, false, "a bare digit still settles");
  assert.equal(goStep(100, "2", 100 + GO_MS + 1).taken, false, "the chord times out");
  assert.equal(goStep(100, "0", 200).taken, false, "0 still fits the plant");
});

test("G then a letter another panel answers goes to it; any other letter is left to the page", () => {
  const letters = new Map([["i", () => {}]]);
  assert.deepEqual(goStep(100, "I", 200, letters), { armed: null, taken: true, action: { kind: "go", key: "i" } });
  assert.deepEqual(goStep(100, "q", 200, letters), { armed: null, taken: false, action: null });
});

function workersAnswer(...states) {
  return { status: 200, body: { workers: states.map((state, i) => ({ id: `local-${i + 1}`, state, last: null })) } };
}

test("with GET /api/workers the strip's status and empty state read the server's workers", () => {
  const origin = "http://127.0.0.1:7311";
  const running = workerLines(workersAnswer("running", "no_key"), 0, origin);
  assert.deepEqual(running, { kind: "running", status: "1 local worker running", line: "1 local worker running.", command: null, setKey: false });
  const keyless = workerLines(workersAnswer("no_key"), 1, origin);
  assert.equal(keyless.line, "Workers are waiting for a model key.");
  assert.equal(keyless.setKey, true, "the line links to Set a key");
  assert.equal(keyless.command, null, "a server that runs its workers needs no outside command");
  const none = workerLines(workersAnswer(), 0, origin);
  assert.equal(none.kind, "none");
  assert.match(none.line, /--workers 1/);
});

test("without GET /api/workers the strip infers from the view's agents, as before", onPlatform(POSIX, () => {
  const origin = "http://127.0.0.1:7311";
  for (const answer of [{ status: 404, body: null }, { status: 403, body: null }, null]) {
    const empty = workerLines(answer, 0, origin);
    assert.deepEqual(empty, { kind: "outside", status: "no worker connected", ...emptyLines(origin), setKey: false });
    assert.equal(workerLines(answer, 2, origin).status, "2 workers connected");
  }
}));

test("the strip and the first-run worker step never disagree on the same answer", onPlatform(POSIX, () => {
  const origin = "http://127.0.0.1:7311";
  const answers = [workersAnswer("running"), workersAnswer("no_key", "no_key"), workersAnswer(), { status: 404, body: null }, null];
  for (const answer of answers) {
    for (const agents of [0, 2]) {
      const view = { agents: Array.from({ length: agents }, (_, i) => ({ id: `a${i}` })), orders: [], lanes: [] };
      const step = steps({ token: true, workers: answer, view, origin }).find((s) => s.id === "worker");
      const lanes = workerLines(answer, agents, origin);
      const lanesDone = lanes.kind === "running" || lanes.kind === "connected";
      assert.equal(step.state === "done", lanesDone, `${JSON.stringify(answer)} with ${agents} agents`);
      assert.equal(step.action?.kind === "setup", lanes.setKey);
      if (step.command) assert.equal(lanes.command, step.command);
    }
  }
}));

// A stand-in for the strip: rows with a dataset and a class list, as the page builds them.
function fakeStrip(agents) {
  const rows = agents.map((agent, index) => {
    const classes = new Set();
    return {
      dataset: { lane: String(index), agent },
      classList: { toggle: (name, on) => (on ? classes.add(name) : classes.delete(name)), contains: (name) => classes.has(name) },
    };
  });
  return { rows, querySelectorAll: (selector) => (selector === "li[data-agent]" ? rows : []) };
}

test("highlightLane marks one agent's row, moves the mark, and clears it on null", () => {
  const strip = fakeStrip(["agent-1", "agent-2"]);
  assert.equal(highlightLane("agent-2", strip), true);
  assert.deepEqual(strip.rows.map((row) => row.classList.contains("hl")), [false, true]);
  assert.equal(strip.rows[1].dataset.highlighted, "true");
  assert.equal(highlightLane("agent-1", strip), true);
  assert.deepEqual(strip.rows.map((row) => row.classList.contains("hl")), [true, false]);
  assert.equal(strip.rows[1].dataset.highlighted, undefined);
  assert.equal(highlightLane("agent-9", strip), false, "an agent with no row marks nothing");
  assert.equal(highlightLane(null, strip), false);
  assert.deepEqual(strip.rows.map((row) => row.classList.contains("hl")), [false, false]);
});

test("a drone-hover event marks that agent's lane, and a null agent clears it", () => {
  const strip = fakeStrip(["agent-1", "agent-2"]);
  const map = new EventTarget();
  listenForDroneHover(map, strip);
  map.dispatchEvent(new CustomEvent("acan:drone-hover", { detail: { agent: "agent-1" } }));
  assert.equal(strip.rows[0].classList.contains("hl"), true);
  assert.equal(strip.rows[0].dataset.highlighted, "true");
  assert.equal(strip.rows[1].classList.contains("hl"), false);
  map.dispatchEvent(new CustomEvent("acan:drone-hover", { detail: { agent: null } }));
  assert.deepEqual(strip.rows.map((row) => row.classList.contains("hl")), [false, false]);
  assert.equal(strip.rows[0].dataset.highlighted, undefined);
});

test("stale and away use the map ring's numbers: stale from 2 to 10 minutes, away from 10", () => {
  // The same values as STALE_MS and AWAY_MS in the drone code (marks.rs).
  assert.equal(STALE_MS, 2 * 60 * 1000);
  assert.equal(AWAY_MS, 10 * 60 * 1000);
  const at = (quietMs) => quietState({ holds: true, waiting: false, quietMs });
  assert.equal(at(STALE_MS - 1), null);
  assert.equal(at(STALE_MS), "stale");
  assert.equal(at(AWAY_MS - 1), "stale");
  assert.equal(at(AWAY_MS), "away");
  assert.equal(quietState({ holds: false, waiting: false, quietMs: AWAY_MS }), null, "an idle agent is never quiet");
  assert.equal(quietState({ holds: true, waiting: true, quietMs: AWAY_MS }), null, "a waiting agent keeps its ring");
  assert.equal(quietState({ holds: true, waiting: false, quietMs: null }), null, "no live clock, nothing quiet");
});

test("an agent quiet 12 minutes reads away · 12m, not stale; a replayed page reads neither", () => {
  const now = T0 + 30_000 + 12 * 60_000;
  const rows = laneRows(view(), lastActivity(log()), now);
  const a = rows.find((row) => row.agent === "agent-a");
  assert.equal(a.quiet, "away");
  assert.equal(a.away, true);
  assert.equal(a.stale, false);
  assert.equal(a.stamp, "12m");
  const replayed = laneRows(view(), lastActivity(log()), now, { live: false });
  assert.equal(replayed.some((row) => row.quiet !== null), false);
  assert.equal(awayStamp(59 * 60_000), "59m");
  assert.equal(awayStamp(125 * 60_000), "2h");
});

test("an agent whose task is in an open clash, or whose order asks, waits and is never quiet", () => {
  const v = { ...view(), clashes: [{ id: 0, a: 1, b: 2, state: "open" }], questions: [] };
  assert.deepEqual([...waitingAgents(v)].sort(), ["agent-a", "agent-b"]);
  const asked = { ...view(), clashes: [], questions: [{ id: 0, order: 1 }] };
  assert.deepEqual([...waitingAgents(asked)].sort(), ["agent-a", "agent-b"]);
  const rows = laneRows(v, lastActivity(log()), T0 + 30_000 + AWAY_MS);
  assert.equal(rows.some((row) => row.quiet !== null), false);
});

test("stale is muted and away is ink in the strip; neither is red", () => {
  const css = readFileSync(new URL("../../www/index.html", import.meta.url), "utf8");
  assert.match(css, /#lanes \.when\.stale \{ color: var\(--muted\); \}/);
  assert.match(css, /#lanes \.when\.away \{ color: var\(--ink\); \}/);
  assert.doesNotMatch(css, /#lanes \.when\.\w+ \{ color: var\(--red\)/);
});
