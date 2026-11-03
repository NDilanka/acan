// Node tests for the drones' page rules. Run with:
//   node --test crates/acan-render-web/tests/js/drones.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  AWAY_MS, FLAGS_PER_COLUMN, agentFlagShown, foldColumn, seenOnReturn, standsBeside,
} from "../../www/place.js";

const NOW = 1_700_000_000_000;

test("a reader back after five minutes or more is told when they last saw the page", () => {
  const seen = NOW - AWAY_MS;
  assert.equal(seenOnReturn(String(seen), NOW, true), seen);
  assert.equal(seenOnReturn(String(NOW - 3 * AWAY_MS), NOW, true), NOW - 3 * AWAY_MS);
});

test("a short absence shows nothing", () => {
  assert.equal(seenOnReturn(String(NOW - AWAY_MS + 1), NOW, true), 0);
  assert.equal(seenOnReturn(String(NOW), NOW, true), 0);
});

test("a frozen or replayed page never shows what landed while away", () => {
  assert.equal(seenOnReturn(String(NOW - 10 * AWAY_MS), NOW, false), 0);
});

test("nothing kept, or nothing usable, shows nothing", () => {
  assert.equal(seenOnReturn(null, NOW, true), 0);
  assert.equal(seenOnReturn(undefined, NOW, true), 0);
  assert.equal(seenOnReturn("", NOW, true), 0);
  assert.equal(seenOnReturn("soon", NOW, true), 0);
  assert.equal(seenOnReturn("-5", NOW, true), 0);
  assert.equal(seenOnReturn(String(NOW + 1), NOW, true), 0, "a time from the future");
});

test("an away dot starts on its plinth corner, beside its anchor, as a landed dot does", () => {
  assert.equal(standsBeside("away"), true);
  assert.equal(standsBeside("landed"), true);
  assert.equal(standsBeside("clash"), true);
  for (const kind of ["agent", "order", "question", "plot", "stage"]) {
    assert.equal(standsBeside(kind), false, kind);
  }
});

test("an idle drone's flag shows only on hover or with Alt", () => {
  for (const word of ["working", "waiting", "stale"]) assert.equal(agentFlagShown(word, false, false), true, word);
  assert.equal(agentFlagShown("idle", false, false), false);
  assert.equal(agentFlagShown("idle", true, false), true, "Alt");
  assert.equal(agentFlagShown("idle", false, true), true, "hover");
});

test("a column shows two flags, most urgent first, and folds the rest into +N", () => {
  assert.equal(FLAGS_PER_COLUMN, 2);
  const items = [
    { id: "a", state: "working" },
    { id: "b", state: "idle" },
    { id: "c", state: "waiting" },
    { id: "d", state: "working" },
    { id: "e", state: "stale" },
  ];
  const { shown, folded } = foldColumn(items);
  assert.deepEqual(shown.map((item) => item.id), ["c", "e"]);
  assert.equal(folded, 3);
  const calm = foldColumn([{ id: "a", state: "working" }, { id: "d", state: "working" }]);
  assert.deepEqual(calm.shown.map((item) => item.id), ["a", "d"], "one rank keeps its order");
  assert.equal(calm.folded, 0);
  assert.deepEqual(foldColumn([]), { shown: [], folded: 0 });
});

test("the hovered drone's flag comes to the front of its column, even from the fold", () => {
  const items = [
    { id: "a", state: "waiting" },
    { id: "b", state: "working" },
    { id: "c", state: "idle", hovered: true },
  ];
  const { shown, folded } = foldColumn(items);
  assert.deepEqual(shown.map((item) => item.id), ["c", "a"]);
  assert.equal(folded, 1);
});
