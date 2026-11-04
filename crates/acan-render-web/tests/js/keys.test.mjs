// Node tests for the map's key table: the G chord, Esc's order, and the single keys. Run with:
//   node --test crates/acan-render-web/tests/js/keys.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { ESCAPE_ORDER, keyAction } from "../../www/keys.js";
import { GO_MS } from "../../www/lanes.js";

const LETTERS = new Map([["i", () => {}], ["t", () => {}], ["h", () => {}]]);
const NONE = { command: false, compose: false, viewer: false, question: false, clash: false };

function press(key, extra = {}) {
  return { key, code: "", repeat: false, shift: false, ...extra };
}

function ctx(extra = {}) {
  return { armed: null, now: 1000, letters: LETTERS, held: false, riding: false, open: NONE, ...extra };
}

test("G then a digit rides that lane, and the digit does not settle", () => {
  const armed = keyAction(press("g"), ctx());
  assert.deepEqual(armed.action, { kind: "arm-go" });
  assert.equal(armed.prevent, true);
  const ride = keyAction(press("2"), ctx({ armed: armed.armed, now: 1200 }));
  assert.deepEqual(ride.action, { kind: "ride-lane", index: 1 });
  assert.equal(ride.armed, null);
  assert.deepEqual(keyAction(press("9"), ctx({ armed: 1000, now: 1100 })).action, { kind: "ride-lane", index: 8 });
});

test("G then I goes to the inbox and G then T to the scrubber, not the plate's T", () => {
  assert.deepEqual(keyAction(press("I", { shift: true }), ctx({ armed: 1000 })).action, { kind: "go", letter: "i" });
  assert.deepEqual(keyAction(press("t"), ctx({ armed: 1000 })).action, { kind: "go", letter: "t" });
  assert.deepEqual(keyAction(press("t"), ctx()).action, { kind: "mode" });
});

test("a bare digit settles 1 to 3, and a late digit after G settles too", () => {
  for (const n of [1, 2, 3]) assert.deepEqual(keyAction(press(String(n)), ctx()).action, { kind: "digit", n });
  const late = keyAction(press("1"), ctx({ armed: 1000, now: 1000 + GO_MS + 1 }));
  assert.deepEqual(late.action, { kind: "digit", n: 1 });
  assert.equal(late.armed, null);
  assert.equal(keyAction(press("4"), ctx()).action, null);
  assert.equal(keyAction(press("1", { repeat: true }), ctx()).action, null, "a held digit settles once");
});

test("G then a letter no panel answers ends the chord and the letter does its own work", () => {
  const step = keyAction(press("q"), ctx({ armed: 1000 }));
  assert.deepEqual(step.action, { kind: "turn", dir: -1 });
  assert.equal(step.armed, null);
});

test("A answers, O orders and / opens the command bar, each once per press", () => {
  assert.deepEqual(keyAction(press("a"), ctx()).action, { kind: "toggle-question" });
  assert.deepEqual(keyAction(press("A", { shift: true }), ctx()).action, { kind: "toggle-question" });
  const order = keyAction(press("o"), ctx());
  assert.deepEqual(order.action, { kind: "order-here" });
  assert.equal(order.prevent, true, "the O is not typed into the compose field");
  const bar = keyAction(press("/"), ctx());
  assert.deepEqual(bar.action, { kind: "open-command" });
  assert.equal(bar.prevent, true);
  const held = keyAction(press("/", { repeat: true }), ctx());
  assert.equal(held.action, null);
  assert.equal(held.prevent, true, "a held / is not typed either");
  assert.equal(keyAction(press("a", { repeat: true }), ctx()).action, null);
});

test("Esc closes the topmost open panel: command bar, compose, viewer, question, clash card", () => {
  assert.deepEqual(ESCAPE_ORDER, ["command", "compose", "settings", "viewer", "question", "clash"]);
  const all = { command: true, compose: true, settings: true, viewer: true, question: true, clash: true };
  const closed = [];
  const open = { ...all };
  for (;;) {
    const step = keyAction(press("Escape"), ctx({ open, held: true, riding: true }));
    if (step.action.kind !== "close") {
      assert.deepEqual(step.action, { kind: "go-live" }, "with every panel closed, a rewound map goes live");
      break;
    }
    closed.push(step.action.panel);
    open[step.action.panel] = false;
  }
  assert.deepEqual(closed, ESCAPE_ORDER);
});

test("Esc goes live before it ends a ride, ends a ride on a live map, and else does nothing", () => {
  assert.deepEqual(keyAction(press("Escape"), ctx({ held: true, riding: true })).action, { kind: "go-live" });
  assert.deepEqual(keyAction(press("Escape"), ctx({ riding: true })).action, { kind: "leave-ride" });
  assert.equal(keyAction(press("Escape"), ctx()).action, null);
});

test("Esc after G still closes, and ends the chord", () => {
  const step = keyAction(press("Escape"), ctx({ armed: 1000, open: { ...NONE, compose: true } }));
  assert.deepEqual(step.action, { kind: "close", panel: "compose" });
  assert.equal(step.armed, null);
});

test("the camera keys keep their meanings", () => {
  assert.deepEqual(keyAction(press("ArrowLeft"), ctx()).action, { kind: "pan", dx: 80, dy: 0 });
  assert.deepEqual(keyAction(press("5"), ctx()).action, { kind: "projection" });
  assert.deepEqual(keyAction(press("x", { code: "Numpad5" }), ctx()).action, { kind: "projection" });
  assert.deepEqual(keyAction(press("+"), ctx()).action, { kind: "zoom", dir: 1 });
  assert.deepEqual(keyAction(press("-", { repeat: true }), ctx()).action, { kind: "zoom", dir: -1 });
  assert.deepEqual(keyAction(press("0"), ctx()).action, { kind: "fit" });
  assert.deepEqual(keyAction(press("Tab", { shift: true }), ctx()).action, { kind: "ride-step", dir: -1 });
  assert.deepEqual(keyAction(press("c"), ctx()).action, { kind: "toggle-card" });
  assert.deepEqual(keyAction(press("]"), ctx()).action, { kind: "step", delta: 1 });
  assert.equal(keyAction(press("Shift"), ctx()).action, null);
});

test("a held G does not re-arm the chord", () => {
  const step = keyAction(press("g", { repeat: true }), ctx({ armed: 1000, now: 1100 }));
  assert.equal(step.armed, 1000);
  assert.equal(step.action, null);
});

test("V turns the map to top-down once per press, and a held V does not turn it again", () => {
  assert.deepEqual(keyAction(press("v"), ctx()).action, { kind: "top-down" });
  assert.deepEqual(keyAction(press("V", { shift: true }), ctx()).action, { kind: "top-down" });
  assert.equal(keyAction(press("v", { repeat: true }), ctx()).action, null);
  const armed = keyAction(press("g"), ctx());
  assert.deepEqual(keyAction(press("v"), ctx({ armed: armed.armed, now: 1100 })).action, { kind: "top-down" });
});

test("the comma key toggles settings, and Esc closes an open settings drawer first", () => {
  assert.deepEqual(keyAction(press(","), ctx()).action, { kind: "settings" });
  const step = keyAction(press("Escape"), ctx({ open: { ...NONE, settings: true, clash: true } }));
  assert.deepEqual(step.action, { kind: "close", panel: "settings" });
});
