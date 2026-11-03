// Node tests for the replay page's receipts: a replay makes no plant-view call, so each receipt
// is marked as not in the replay, with no error. Run with:
//   node --test crates/acan-render-web/tests/js/replay.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { REPLAY_PLATE_LINE, markReplayReceipts } from "../../www/plant.js";

function stateWith(receipts) {
  return { view: { receipts }, plantViews: new Map() };
}

test("a replay marks each receipt as not in the replay, with no error to show", () => {
  const state = stateWith([{ order: "o-0", recorded: 2 }]);
  markReplayReceipts(state);
  const entry = state.plantViews.get("o-0");
  assert.equal(entry.replay, true);
  assert.equal(entry.recorded, 2);
  assert.equal(entry.error, null);
  assert.equal(entry.playError, null);
  assert.equal(entry.play, null);
  assert.deepEqual(entry.items, []);
  assert.match(REPLAY_PLATE_LINE, /not in this replay/);
});

test("a receipt whose count moves gets a new mark, and an unchanged one keeps its own", () => {
  const state = stateWith([{ order: "o-0", recorded: 2 }]);
  markReplayReceipts(state);
  const first = state.plantViews.get("o-0");
  markReplayReceipts(state);
  assert.equal(state.plantViews.get("o-0"), first, "an unchanged receipt is not marked again");
  state.view.receipts = [{ order: "o-0", recorded: 3 }];
  markReplayReceipts(state);
  assert.equal(state.plantViews.get("o-0").recorded, 3);
});

test("a replay with no receipts marks nothing", () => {
  const state = stateWith([]);
  markReplayReceipts(state);
  assert.equal(state.plantViews.size, 0);
});
