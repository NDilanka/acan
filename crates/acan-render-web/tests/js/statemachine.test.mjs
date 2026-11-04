// Node tests for the state machines in the map: parsing the plant's `state_machines`, the stations
// and belts on the ground, the dials, which transition a touch names, and the light the belts show.
// Run with:
//   node --test crates/acan-render-web/tests/js/statemachine.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  DIAL_MAX,
  FADE_MS,
  HELD_MS,
  allStateMachines,
  beltsOf,
  curvePoints,
  dialTicks,
  lastTouchedState,
  lightAt,
  lightNow,
  loopPoints,
  noteLogEntry,
  noteTouch,
  readStateMachines,
  resetLight,
  sceneOf,
  setStateMachines,
  stackPlan,
  stationLayout,
  stubPoints,
  tileSize,
  touchOf,
  transitionIndex,
} from "../../www/statemachine.js";

const FIXTURE = new URL("../../www/fixtures/state-machines-v1.json", import.meta.url);
const DEMO = new URL("../../www/fixtures/demo-plant.json", import.meta.url);
// The recorded run the replay page plays, at the repository root.
const RECORDING = new URL("../../../../replay/events.json", import.meta.url);

const FOOTPRINT = { min: [10, 20], size: [12, 8] };

function machine(id, states, transitions, confidence = "high") {
  return { id, name: id, file: "src/a.ts", line: 1, module: "a", confidence, states: states.map((name) => ({ name, line: 1 })), transitions };
}

function transition(from, to, file = "src/a.ts", line = 5, end = 5) {
  return { from, to, file, line, end_line: end, via: null };
}

test("a plant payload keeps the machines the spec lists, and nothing else", () => {
  const good = { id: "x", module: "a", states: [{ name: "s" }, { name: "t" }], transitions: [transition("s", "t")] };
  const text = JSON.stringify({
    state_machines: [
      good,
      { ...good, id: "one-state", states: [{ name: "s" }] },
      { ...good, id: "no-transition", transitions: [] },
      { ...good, id: "no-module", module: undefined },
    ],
  });
  assert.deepEqual(readStateMachines(text).map((m) => m.id), ["x"]);
});

test("a payload without state machines, or one that is not JSON, has none", () => {
  assert.deepEqual(readStateMachines("{}"), []);
  assert.deepEqual(readStateMachines('{"state_machines":"nope"}'), []);
  assert.deepEqual(readStateMachines("not json"), []);
});

test("only a transition touch is read as one, and an event that merely names it is not", () => {
  const touch = { type: "transition_touched", machine: "a#X", file: "src/a.ts", line: 5, end_line: 5 };
  assert.deepEqual(touchOf(JSON.stringify(touch)), touch);
  assert.equal(touchOf(JSON.stringify({ type: "landed", note: '"transition_touched"' })), null);
  assert.equal(touchOf('{"seq":3,"event":{"type":"declared"}}'), null, "an ordinary event is not parsed as a touch");
  assert.equal(touchOf('{"type":"transition_touched",'), null, "a broken touch is not a touch");
});

test("up to six states sit in a row, in the order of the states, centred on the building", () => {
  const states = ["a", "b", "c", "d"].map((name) => ({ name }));
  const stations = stationLayout(states, FOOTPRINT);
  assert.deepEqual(stations.map((s) => s.name), ["a", "b", "c", "d"]);
  assert.ok(stations.every((s) => s.y === stations[0].y), "one row");
  assert.ok(stations.every((s, i) => i === 0 || s.x > stations[i - 1].x), "left to right");
  const mid = (stations[0].x + stations[3].x) / 2;
  assert.ok(Math.abs(mid - (FOOTPRINT.min[0] + FOOTPRINT.size[0] / 2)) < 1e-9, "centred on the building");
  assert.ok(stations[0].y > FOOTPRINT.min[1] + FOOTPRINT.size[1], "the row stands in front of the building");
});

test("more than six states sit on a ring round the building, clear of it", () => {
  const states = Array.from({ length: 8 }, (_, i) => ({ name: `s${i}` }));
  const stations = stationLayout(states, FOOTPRINT);
  assert.equal(stations.length, 8);
  const keys = new Set(stations.map((s) => `${s.x.toFixed(6)},${s.y.toFixed(6)}`));
  assert.equal(keys.size, 8, "every state has its own spot");
  const [x, y] = FOOTPRINT.min;
  const [w, d] = FOOTPRINT.size;
  for (const s of stations) {
    const inside = s.x > x && s.x < x + w && s.y > y && s.y < y + d;
    assert.equal(inside, false, `${s.name} stands clear of the building`);
  }
});

test("a belt runs between its two stations' centres and bows to one side", () => {
  const a = { x: 0, y: 0 };
  const b = { x: 10, y: 0 };
  const points = curvePoints(a, b);
  assert.equal(points.length, 11);
  assert.deepEqual(points[0], a);
  assert.deepEqual(points[points.length - 1], b);
  const reverse = curvePoints(b, a);
  const sum = { x: points[5].x + reverse[5].x, y: points[5].y + reverse[5].y };
  assert.ok(Math.abs(sum.x - 10) < 1e-9 && Math.abs(sum.y) < 1e-9, "the reverse belt bows the other way");
  assert.ok(Math.abs(points[5].y) > 0.5, "the belt is curved, not straight");
});

test("a loop starts and ends at its station, and a stub enters its station from outside", () => {
  const station = { x: 4, y: 6 };
  const loop = loopPoints(station, 2);
  assert.ok(Math.abs(loop[0].x - 4) < 1e-9 && Math.abs(loop[0].y - 6) < 1e-9);
  assert.ok(Math.abs(loop.at(-1).x - 4) < 1e-9 && Math.abs(loop.at(-1).y - 6) < 1e-9);
  const centre = { x: 0, y: 0 };
  const stub = stubPoints(station, centre, 2);
  assert.deepEqual(stub.at(-1), station, "the stub ends on the station");
  assert.ok(Math.hypot(stub[0].x, stub[0].y) > Math.hypot(station.x, station.y), "the stub starts outside the station");
});

test("belts follow the transitions, a stub stands for a transition from *, and an unknown state is left out", () => {
  const m = machine("a#Order", ["open", "paid", "closed"], [
    transition("open", "paid"),
    transition("*", "closed", "src/a.ts", 9, 9),
    transition("paid", "ghost"),
    transition("paid", "paid", "src/a.ts", 12, 12),
  ]);
  const { stations, belts, left } = beltsOf(m, FOOTPRINT);
  assert.equal(stations.length, 3);
  assert.deepEqual(belts.map((b) => b.index), [0, 1, 3]);
  assert.equal(belts[1].stub, true);
  assert.equal(belts[0].stub, false);
  assert.equal(left, 1, "the transition to a state the machine lacks is counted and left out");
});

test("a low-confidence machine marks its belts low", () => {
  const m = machine("a#Phase", ["open", "tripped"], [transition("open", "tripped")], "low");
  const { belts } = beltsOf(m, FOOTPRINT);
  assert.equal(belts[0].low, true);
  assert.ok(tileSize(FOOTPRINT) >= 1, "a station tile has a size");
});

test("the dial has one tick per state over a 270-degree arc, with one lit tick", () => {
  const ticks = dialTicks(4, 2);
  assert.equal(ticks.length, 4);
  assert.equal(ticks[0].angle, -135);
  assert.equal(ticks[3].angle, 135);
  assert.deepEqual(ticks.map((t) => t.lit), [false, false, true, false]);
  assert.ok(dialTicks(4, -1).every((t) => !t.lit), "no tick lit when none was touched");
});

test("a building's roof shows at most three dials, then a count of the rest", () => {
  const list = [1, 2, 3, 4, 5].map((i) => ({ id: `m${i}` }));
  assert.deepEqual(stackPlan(list.slice(0, 2)), { shown: list.slice(0, 2), more: 0 });
  const plan = stackPlan(list);
  assert.equal(plan.shown.length, DIAL_MAX);
  assert.equal(plan.more, 2);
});

test("a touch names the transition in the same file on the same lines, else the same from and to", () => {
  const m = machine("a#X", ["p", "q"], [transition("p", "q", "src/a.ts", 5, 7), transition("q", "p", "src/b.ts", 9, 9)]);
  assert.equal(transitionIndex(m, { file: "src/b.ts", line: 9, end_line: 9, from: "q", to: "p" }), 1);
  assert.equal(transitionIndex(m, { file: "src/x.ts", line: 1, end_line: 1, from: "p", to: "q" }), 0);
  assert.equal(transitionIndex(m, { file: "src/x.ts", line: 1, end_line: 1, from: "q", to: "q" }), -1);
});

test("a claim holds its belt, and a landing fades it out", () => {
  setStateMachines([machine("a#X", ["p", "q"], [transition("p", "q")])]);
  const touch = { type: "transition_touched", machine: "a#X", from: "p", to: "q", file: "src/a.ts", line: 5, end_line: 5, by: "claim", stage: "declared", task: 3, commit: null };
  assert.equal(noteTouch(touch, 1000), true);
  assert.deepEqual(lightAt(1000).get("a#X#0"), { kind: "held", task: 3 });
  assert.deepEqual(lightAt(1000 + HELD_MS - 1).get("a#X#0")?.kind, "held");
  assert.equal(lightAt(1000 + HELD_MS).has("a#X#0"), false, "a claim with no landing stops holding");

  noteTouch({ ...touch, by: "commit", stage: "landed", commit: "abc" }, 2000);
  const half = lightAt(2000 + FADE_MS / 2).get("a#X#0");
  assert.equal(half.kind, "fade");
  assert.ok(Math.abs(half.alpha - 0.5) < 1e-9, "the belt fades over FADE_MS");
  assert.equal(lightAt(2000 + FADE_MS).has("a#X#0"), false, "and then it is paper again");
});

test("a touch of a machine the page does not hold is not lit", () => {
  setStateMachines([machine("a#X", ["p", "q"], [transition("p", "q")])]);
  assert.equal(noteTouch({ machine: "b#Y", file: "src/a.ts", line: 5, end_line: 5, from: "p", to: "q", task: 1, stage: "declared" }, 0), false);
  resetLight();
  assert.equal(lightAt(0).size, 0);
});

test("the replay derives the same light from the claims and landings in its log", () => {
  setStateMachines([machine("a#X", ["p", "q"], [transition("p", "q", "src/a.ts")])]);
  noteLogEntry({ at: 100, event: { type: "declared", decl: { task: 7, footprint: { files: ["src/other.ts"] } } } });
  assert.equal(lightAt(100).size, 0, "a claim on other files lights nothing");
  noteLogEntry({ at: 200, event: { type: "declared", decl: { task: 7, footprint: { files: ["src/a.ts"] } } } });
  assert.deepEqual(lightAt(200).get("a#X#0"), { kind: "held", task: 7 });
  noteLogEntry({ at: 300, event: { type: "landed", task: 7, commit: "c" } });
  assert.equal(lightAt(300).get("a#X#0").kind, "fade");
  noteLogEntry({ at: 301, event: { type: "index_updated" } });
  assert.equal(lightAt(301).get("a#X#0").kind, "fade", "other events leave the light as it is");
});

test("the recorded run lights the fixture's ratelimit belts while task 0 holds them", () => {
  const plant = JSON.parse(readFileSync(FIXTURE, "utf8"));
  setStateMachines(readStateMachines(JSON.stringify(plant)));
  const entries = JSON.parse(readFileSync(RECORDING, "utf8"));
  const firstClaim = entries.find((e) => e.event?.type === "declared");
  const landing = entries.find((e) => e.event?.type === "landed");
  assert.ok(firstClaim && landing, "the recording has a claim and a landing");
  for (const entry of entries) {
    if (entry.seq > firstClaim.seq) break;
    noteLogEntry(entry);
  }
  const after = lightAt(firstClaim.at);
  const lit = [...after.keys()].filter((key) => key.startsWith("src/ratelimit.ts#"));
  assert.equal(lit.length, 3, "all three of the ratelimit belts are held by the claim");
  assert.ok([...after.values()].every((light) => light.kind === "held"));
  assert.equal([...after.keys()].some((key) => key.startsWith("src/store.ts#")), false, "the claim does not touch the store");
  resetLight();
  for (const entry of entries) {
    if (entry.seq > landing.seq) break;
    noteLogEntry(entry);
  }
  assert.equal(lightAt(landing.at).get("src/ratelimit.ts#WindowPhase#0")?.kind, "fade", "the landing fades the belts");
});

test("a dial's lit tick is the state the latest touch lands in, and none before a touch", () => {
  setStateMachines([machine("a#X", ["p", "q", "r"], [transition("p", "q"), transition("q", "r", "src/a.ts", 9, 9)])]);
  const [m] = allStateMachines();
  assert.equal(lastTouchedState(m), -1);
  assert.equal(noteTouch({ machine: "a#X", file: "src/a.ts", line: 9, end_line: 9, from: "q", to: "r", task: 1, stage: "built" }, 1000), true);
  assert.equal(lastTouchedState(m), 2);
});

test("a scene puts a dial on the roof, and stations and belts on the ground only when near", () => {
  setStateMachines([machine("a#X", ["p", "q", "r"], [transition("p", "q")])]);
  const buildings = [{ name: "a", min: [0, 0], size: [10, 10] }];
  const common = {
    buildings,
    machines: allStateMachines(),
    project: (x, y) => [x * 10, y * 10],
    riseAt: () => 5,
    topOf: () => 4,
    now: 0,
  };
  const far = sceneOf({ ...common, near: false });
  assert.equal(far.dials.length, 1);
  assert.deepEqual([far.dials[0].x, far.dials[0].y], [50, 30], "the hub is the roof's centre, four units up at 5 px a unit");
  assert.equal(far.stations.length, 0);
  assert.equal(far.belts.length, 0);
  const near = sceneOf({ ...common, near: true });
  assert.equal(near.stations.length, 3, "one station per state");
  assert.ok(near.stations.every((s) => s.y > 100), "the row stands in front of the building, on the ground");
  assert.equal(near.belts.length, 1);
  assert.equal(near.fading, false);
});

test("a second machine of a building stands further out, and a claim lights only its own belt", () => {
  setStateMachines([
    machine("a#X", ["p", "q"], [transition("p", "q")]),
    machine("a#Y", ["p", "q"], [transition("p", "q")]),
  ]);
  noteTouch({ machine: "a#Y", file: "src/a.ts", line: 5, end_line: 5, from: "p", to: "q", task: 3, stage: "declared" }, 1000);
  const scene = sceneOf({
    buildings: [{ name: "a", min: [0, 0], size: [10, 10] }],
    machines: allStateMachines(),
    project: (x, y) => [x * 10, y * 10],
    riseAt: () => 5,
    topOf: () => 4,
    near: true,
    now: 1001,
  });
  assert.equal(scene.dials.length, 2, "each machine gets a dial on the roof");
  assert.ok(scene.stations[2].y > scene.stations[0].y, "the second machine's row stands further out");
  assert.deepEqual(scene.belts.map((b) => b.lit?.kind ?? null), [null, "held"]);
  assert.equal(scene.belts[1].lit.task, 3);
});

test("the clock is the replay's recorded time once a replay has folded an entry, else the wall clock", () => {
  resetLight();
  assert.ok(Math.abs(lightNow() - Date.now()) < 1000, "a live page reads the wall clock");
  noteLogEntry({ at: 1700000000000, event: { type: "index_updated" } });
  assert.equal(lightNow(), 1700000000000, "a replay reads the recorded time");
  resetLight();
  assert.ok(Math.abs(lightNow() - Date.now()) < 1000, "a restarted replay reads the wall clock until its first entry");
});

test("the fixture is a plant-shaped payload for two buildings of the demo plant", () => {
  const text = readFileSync(FIXTURE, "utf8");
  const machines = readStateMachines(text);
  assert.equal(machines.length, 2);
  setStateMachines(machines);
  assert.equal(allStateMachines().length, 2);
  const demo = JSON.parse(readFileSync(DEMO, "utf8"));
  const names = new Set(demo.machines.map((m) => m.name));
  for (const m of machines) assert.ok(names.has(m.module), `${m.module} is a building of the demo plant`);
  assert.ok(machines.some((m) => m.confidence === "low"), "one machine is low confidence");
  assert.ok(machines.some((m) => m.transitions.some((t) => t.from === "*")), "one transition is from *");
  for (const m of machines) {
    const known = new Set(m.states.map((s) => s.name));
    for (const t of m.transitions) {
      assert.ok(known.has(t.to), `${m.id}: ${t.to} is a state`);
      assert.ok(t.from === "*" || known.has(t.from), `${m.id}: ${t.from} is a state`);
    }
  }
});
