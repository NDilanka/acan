// Node tests for the words on the map: a clash, an order and a plot show their words only on
// hover. Run with:
//   node --test crates/acan-render-web/tests/js/mapmarks.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { markWords } from "../../www/place.js";

const clash = { kind: "clash", clash: 0, on: "src/api", text: "c-0 · overlap", detail: ["agent-a · t-0", "agent-b · t-1"] };

test("a clash shows nothing but its diamond until the diamond is hovered", () => {
  assert.equal(markWords(clash, null, false), "hidden");
  assert.equal(markWords(clash, { machine: "src/api" }, false), "hidden", "its machine's hover is not its own");
  assert.equal(markWords(clash, { clash: 1 }, false), "hidden", "another clash's hover");
});

test("hovering a clash's diamond or badges shows its detail", () => {
  assert.equal(markWords(clash, { clash: 0 }, false), "open");
  assert.equal(markWords(clash, null, true), "open", "Alt shows every word");
});

test("an agent's name pill and the other marks keep their words", () => {
  for (const kind of ["agent", "landed", "away", "question", "stage"]) {
    assert.equal(markWords({ kind, clash: null, on: null, text: "x" }, null, false), "open", kind);
  }
});
