// Node tests for the replay page's origin reads. A replay has no origin, so it makes no plant-view
// call and no Seal route call: each receipt says the view is not in the replay, and each lane's
// Seal says the Seal needs the live origin. Run with:
//   node --test crates/acan-render-web/tests/js/replay.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { syncOrigin } from "../../www/origin.js";
import { REPLAY_PLATE_LINE, markReplayReceipts } from "../../www/plant.js";

// The recording's first events, with the recorded time `at` in milliseconds. The first receipt
// folds about 12.6 s into the playback and the second about 18.8 s, so 20 s holds two receipts.
const TIMELINE = [
  { at: 0, kind: "world" },
  { at: 3900, kind: "scope_drafted", order: "o-0" },
  { at: 12600, kind: "receipt_recorded", order: "o-0" },
  { at: 18800, kind: "receipt_recorded", order: "o-0" },
  { at: 30600, kind: "receipt_recorded", order: "o-0" },
  { at: 35600, kind: "scope_drafted", order: "o-0" },
];

function stateWith(receipts) {
  return { view: { receipts }, plantViews: new Map() };
}

// A fake wasm app: the plant view reads as an empty tour, and the route reads as a match.
const FAKE_APP = {
  plant_view_summary: () => "{}",
  tour_load: () => JSON.stringify({ mode: "plain" }),
  seal_check: () => JSON.stringify({ verdict: "match" }),
};

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

// Plays TIMELINE through `untilMs` as replay.js does: each due event folds into the view, then the
// paint makes its origin reads. Returns every URL the page asked for, and the page state.
async function playback(replay, untilMs) {
  const calls = [];
  const fetchText = async (url) => {
    calls.push(url);
    return "{}";
  };
  const state = {
    view: { receipts: [], lanes: [{ order: 0, rev: 1 }] },
    plantViews: new Map(),
    sealViews: new Map(),
    app: FAKE_APP,
  };
  const counts = new Map();
  for (const event of TIMELINE) {
    if (event.at > untilMs) break;
    if (event.kind === "receipt_recorded") counts.set(event.order, (counts.get(event.order) ?? 0) + 1);
    state.view.receipts = [...counts].map(([order, recorded]) => ({ order, recorded }));
    syncOrigin(state, fetchText, { plate: () => {}, slip: () => {} }, replay);
  }
  await settle();
  return { calls, state };
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

test("a replay makes no origin call through 20 s of playback", async () => {
  const { calls, state } = await playback(true, 20_000);
  assert.equal(state.plantViews.get("o-0")?.recorded, 2, "both receipts folded before 20 s");
  assert.deepEqual(calls, [], "no plant-view or confirmation route is asked");
});

test("a replay makes no origin call to the end of its events", async () => {
  const { calls, state } = await playback(true, Infinity);
  assert.equal(state.plantViews.get("o-0")?.recorded, 3, "every receipt folded");
  assert.deepEqual(calls, []);
});

test("the same playback on the live origin asks for each view and each route", async () => {
  const { calls } = await playback(false, 20_000);
  assert.ok(calls.includes("/api/orders/o-0/plant-view"), "a live receipt asks its plant view");
  assert.ok(calls.includes("/api/orders/o-0/scope/confirmation"), "a live lane asks its route");
});
