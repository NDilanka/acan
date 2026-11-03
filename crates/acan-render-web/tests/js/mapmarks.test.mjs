// Node tests for the words on the map: a clash, an order and a plot show their words only on
// hover. Run with:
//   node --test crates/acan-render-web/tests/js/mapmarks.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { isDraftPlot, markWords, plotLeads } from "../../www/place.js";

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

const order = { kind: "order", clash: null, on: "crates/acan-agent/src", text: "o-0 · src · clashing" };

test("an order's tag is hidden until its machine is hovered", () => {
  assert.equal(markWords(order, null, false), "hidden");
  assert.equal(markWords(order, { machine: "crates/acan-core/src" }, false), "hidden", "another machine");
  assert.equal(markWords(order, { clash: 0 }, false), "hidden", "a clash's diamond shows the clash, not the tag");
  assert.equal(markWords(order, { machine: "crates/acan-agent/src" }, false), "open");
  assert.equal(markWords(order, null, true), "open", "Alt");
});

const draft = { kind: "plot", clash: null, on: "links", text: "o-0 · links · keep · draft rev 1" };
const sealed = { kind: "plot", clash: null, on: "auth", text: "o-1 · auth · plot · confirmed rev 2" };

test("a draft plot rests as one small mark, its line on hover of the mark or the machine", () => {
  assert.equal(markWords(draft, null, false, true), "mark");
  assert.equal(markWords(draft, { machine: "links" }, false, true), "open");
  assert.equal(markWords(draft, { machine: "auth" }, false, true), "mark");
  assert.equal(markWords(draft, null, true, true), "open", "Alt");
  assert.equal(isDraftPlot(draft), true);
  assert.equal(isDraftPlot(sealed), false);
});

test("a machine with several plots shows one mark, a draft ring while any of them is a draft", () => {
  const second = { ...sealed, on: "links" };
  const leads = plotLeads([sealed, { kind: "agent", text: "agent-a" }, second, draft]);
  assert.deepEqual([...leads.keys()], [0, 2], "one lead per machine");
  assert.deepEqual(leads.get(0), { draft: false });
  assert.deepEqual(leads.get(2), { draft: true }, "the draft on links keeps the ring");
  assert.equal(markWords(draft, null, false, leads.has(3)), "hidden", "the second plot on links shows nothing");
  assert.equal(markWords(draft, { machine: "links" }, false, leads.has(3)), "open");
});

test("an agent's name pill and the other marks keep their words", () => {
  for (const kind of ["agent", "landed", "away", "question", "stage"]) {
    assert.equal(markWords({ kind, clash: null, on: null, text: "x" }, null, false), "open", kind);
  }
});
