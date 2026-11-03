// The project-tagged overlays of a canvas of several projects: the split by project, the parked
// marks, the click on a parked plant and the flag columns. Run with:
//   node --test crates/acan-render-web/tests/js/overlays.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  flagGroups,
  flagLeader,
  parkedClick,
  parkedEntries,
  parkedKind,
  pinNear,
  plateSpots,
  splitByProject,
} from "../../www/overlays.js";

test("a flat layout splits into each project's rows, without the project on screen", () => {
  const flat = new Float32Array([0, 1, 10, 20, 1, 2, 30, 40, 2, 3, 50, 60, 1, 4, 70, 80]);
  const split = splitByProject(flat, 4, 0);
  assert.deepEqual([...split.keys()], [1, 2]);
  assert.deepEqual(split.get(1), [[2, 30, 40], [4, 70, 80]]);
  assert.deepEqual(split.get(2), [[3, 50, 60]]);
  assert.equal(splitByProject(flat, 4).size, 3);
});

test("a short tail row is not read", () => {
  assert.deepEqual([...splitByProject([1, 5, 6, 1, 7], 3).entries()], [[1, [[5, 6]]]]);
});

test("the same agent name on two plants stays two rows, one per project", () => {
  const drones = [0, 100, 100, 8, 1, 400, 100, 8];
  const split = splitByProject(drones, 4);
  assert.deepEqual(split.get(0), [[100, 100, 8]]);
  assert.deepEqual(split.get(1), [[400, 100, 8]]);
});

test("the parked entries leave out the project on screen", () => {
  const entries = [{ id: "demo", project: 0 }, { id: "shop", project: 1 }, { id: "notes", project: 2 }];
  assert.deepEqual(parkedEntries(entries, "shop").map((e) => e.id), ["demo", "notes"]);
});

test("a pin near the click on a parked plant is found, nearest first", () => {
  const quads = [0, 0, 100, 100, 1, 3, 300, 300, 1, 4, 305, 300];
  assert.deepEqual(pinNear(quads, 304, 300, 0, 18), { project: 1, clash: 4 });
  assert.equal(pinNear(quads, 100, 100, 0, 18), null, "the project on screen picks its own pins");
  assert.equal(pinNear(quads, 500, 500, 0, 18), null);
});

test("a click on a parked pin wins over a drone, and the project on screen keeps its clicks", () => {
  const order = ["demo", "shop"];
  const drone = { project: 1, id: "shop", agent: "agent-a" };
  assert.deepEqual(parkedClick({ project: 1, clash: 2 }, drone, order, "demo"), { kind: "pin", id: "shop", clash: 2 });
  assert.deepEqual(parkedClick(null, drone, order, "demo"), { kind: "drone", id: "shop", agent: "agent-a" });
  assert.equal(parkedClick(null, drone, order, "shop"), null);
  assert.equal(parkedClick({ project: 0, clash: 1 }, null, order, "demo"), null);
  assert.equal(parkedClick(null, null, order, "demo"), null);
});

test("flag anchors split into agent columns by machine and lone flags", () => {
  const { columns, alone } = flagGroups([[0, 2, 10, 20], [1, 2, 12, 20], [2, -1, 50, 60], [3, 5, 80, 90]]);
  assert.deepEqual([...columns.keys()], [2, 5]);
  assert.deepEqual(columns.get(2).map((item) => item.index), [0, 1]);
  assert.deepEqual(alone, [{ index: 2, x: 50, y: 60 }]);
});

test("a parked plant shows agents, landed and away dots and questions, not hover-only kinds", () => {
  for (const kind of ["agent", "landed", "away", "question"]) assert.equal(parkedKind(kind), true, kind);
  for (const kind of ["clash", "order", "plot", "stage"]) assert.equal(parkedKind(kind), false, kind);
});

test("a lone flag at its wanted spot needs no leader; a moved or stacked one draws one to its anchor", () => {
  const anchor = { x: 100, y: 200 };
  assert.equal(flagLeader({ left: 94, top: 170, w: 60, h: 18 }, 94, 170, anchor, true, 6), null);
  assert.deepEqual(flagLeader({ left: 30, top: 170, w: 60, h: 18 }, 94, 170, anchor, true, 6), { x1: 36, y1: 188, x2: 100, y2: 200 });
  assert.deepEqual(flagLeader({ left: 70, top: 150, w: 60, h: 18 }, 70, 150, anchor, false, 6), { x1: 100, y1: 168, x2: 100, y2: 200 });
});

test("plates of close plants stack downwards, the first keeps its spot", () => {
  const spots = plateSpots([
    { id: "demo", x: 100, y: 50, w: 60, h: 20 },
    { id: "shop", x: 120, y: 55, w: 60, h: 20 },
    { id: "notes", x: 110, y: 52, w: 60, h: 20 },
  ], [], 3);
  assert.deepEqual(spots.get("demo"), { left: 70, top: 50 });
  assert.equal(spots.get("shop").top, 73);
  assert.equal(spots.get("notes").top, 96);
  const boxes = [...spots].map(([id, { left, top }]) => ({ left, right: left + 60, top, bottom: top + 20 }));
  for (let i = 0; i < boxes.length; i += 1) {
    for (let j = i + 1; j < boxes.length; j += 1) {
      const [a, b] = [boxes[i], boxes[j]];
      assert.ok(!(a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom), `${i} and ${j} overlap`);
    }
  }
});

test("a plate moves below a machine name it would cover, and a plate far away keeps its spot", () => {
  const names = [{ left: 90, right: 130, top: 45, bottom: 60 }];
  const spots = plateSpots([{ id: "notes", x: 110, y: 50, w: 60, h: 20 }, { id: "far", x: 600, y: 50, w: 60, h: 20 }], names, 3);
  assert.equal(spots.get("notes").top, 63);
  assert.deepEqual(spots.get("far"), { left: 570, top: 50 });
});
