// Node tests for the plant view's recheck: a view that shows the deterministic order is asked
// again every 10 s, until the director's tour arrives or 3 minutes have passed. Run with:
//   node --test crates/acan-render-web/tests/js/plant.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { RECHECK_EVERY_MS, RECHECK_FOR_MS, syncPlantViews } from "../../www/plant.js";

const DETERMINISTIC = JSON.stringify({ play: { mode: "deterministic" } });
const DIRECTED = JSON.stringify({ play: { mode: "directed" } });
const NO_TOUR = JSON.stringify({ play: {} });
const REFUSED = JSON.stringify({ refused: true });

// A page state with one order whose receipt count is `recorded`. The fake app reads a view's play
// from its text, and refuses a view marked `refused`, as the wasm side refuses an unadmitted tour.
function stateWith(recorded) {
  return {
    view: { receipts: [{ order: "o-1", recorded }] },
    plantViews: new Map(),
    app: {
      plant_view_summary: () => "{}",
      tour_load(text) {
        const view = JSON.parse(text);
        if (view.refused) throw new Error("the tour is refused");
        return JSON.stringify(view.play ?? {});
      },
    },
  };
}

// Answers each fetch with the next entry, and repeats the last one. An Error entry is thrown.
function scripted(answers) {
  let calls = 0;
  const fetchText = async () => {
    const answer = answers[Math.min(calls, answers.length - 1)];
    calls += 1;
    if (answer instanceof Error) throw answer;
    return answer;
  };
  fetchText.calls = () => calls;
  return fetchText;
}

// Timers that run only when the test fires them, earliest first, and a clock they move.
function fakeClock() {
  let now = 0;
  const pending = [];
  return {
    pending,
    now: () => now,
    timers: {
      schedule(fn, wait) {
        pending.push({ at: now + wait, wait, fn });
      },
      now: () => now,
    },
    async fire() {
      pending.sort((a, b) => a.at - b.at);
      const next = pending.shift();
      now = next.at;
      next.fn();
      await settle();
    },
  };
}

// Lets the fetches in flight finish.
const settle = () => new Promise((resolve) => setImmediate(resolve));

test("a deterministic view is asked again every 10 s, until the director's tour arrives", async (t) => {
  t.mock.method(console, "error", () => {});
  const state = stateWith(1);
  const fetchText = scripted([DETERMINISTIC, DETERMINISTIC, DIRECTED]);
  const clock = fakeClock();
  let repaints = 0;

  syncPlantViews(state, fetchText, () => (repaints += 1), clock.timers);
  await settle();
  assert.deepEqual(clock.pending.map((timer) => timer.wait), [RECHECK_EVERY_MS]);

  await clock.fire();
  assert.equal(clock.now(), RECHECK_EVERY_MS);
  assert.equal(clock.pending.length, 1, "a view that is still deterministic is checked again");

  await clock.fire();
  assert.equal(fetchText.calls(), 3);
  assert.equal(clock.pending.length, 0, "the director's tour stops the checks");
  assert.equal(state.plantViews.get("o-1").play.mode, "directed");
  assert.equal(
    repaints,
    2,
    "the first view and the director's tour repaint; a check that finds the same view does not",
  );
});

test("the checks stop 3 minutes after the first deterministic view", async (t) => {
  t.mock.method(console, "error", () => {});
  const state = stateWith(1);
  const fetchText = scripted([DETERMINISTIC]);
  const clock = fakeClock();

  syncPlantViews(state, fetchText, () => {}, clock.timers);
  await settle();
  while (clock.pending.length > 0) await clock.fire();

  assert.equal(clock.now(), RECHECK_FOR_MS);
  assert.equal(
    fetchText.calls(),
    RECHECK_FOR_MS / RECHECK_EVERY_MS + 1,
    "the first view, then a check every 10 s up to 3 minutes",
  );
});

test("a view with no tour, or a tour that does not play, is not asked again", async (t) => {
  t.mock.method(console, "error", () => {});
  for (const text of [NO_TOUR, REFUSED]) {
    const state = stateWith(1);
    const fetchText = scripted([text]);
    const clock = fakeClock();

    syncPlantViews(state, fetchText, () => {}, clock.timers);
    await settle();

    assert.equal(fetchText.calls(), 1, text);
    assert.equal(clock.pending.length, 0, text);
  }
});

test("a view that cannot be read is kept as an error, and is not asked again", async (t) => {
  t.mock.method(console, "error", () => {});
  const state = stateWith(1);
  const fetchText = scripted([new Error("the server did not answer")]);
  const clock = fakeClock();

  syncPlantViews(state, fetchText, () => {}, clock.timers);
  await settle();

  assert.equal(state.plantViews.get("o-1").error, "the server did not answer");
  assert.equal(clock.pending.length, 0);
});

test("a receipt count that moves starts its own window, and the old check asks nothing", async (t) => {
  t.mock.method(console, "error", () => {});
  const state = stateWith(1);
  const fetchText = scripted([DETERMINISTIC]);
  const clock = fakeClock();

  syncPlantViews(state, fetchText, () => {}, clock.timers);
  await settle();
  state.view.receipts[0].recorded = 2;
  syncPlantViews(state, fetchText, () => {}, clock.timers);
  await settle();
  assert.equal(fetchText.calls(), 2);
  assert.equal(clock.pending.length, 2, "one check for each count");

  await clock.fire();
  assert.equal(fetchText.calls(), 2, "the check for count 1 asks nothing, as the page shows count 2");

  await clock.fire();
  assert.equal(fetchText.calls(), 3, "the check for count 2 asks the view again");
  assert.equal(state.plantViews.get("o-1").recorded, 2);
});
