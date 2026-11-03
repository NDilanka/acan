// Node tests for the drones' page rules. Run with:
//   node --test crates/acan-render-web/tests/js/drones.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { AWAY_MS, seenOnReturn, standsBeside } from "../../www/place.js";

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
