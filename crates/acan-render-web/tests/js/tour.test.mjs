// Node tests for the receipt tour's page logic. Run with:
//   node --test crates/acan-render-web/tests/js/tour.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  DIRECTED,
  DWELL_MS,
  MOVE_MS,
  PLAIN,
  STEP_MS,
  advance,
  chipText,
  clampIndex,
  countLines,
  countSummary,
  itemTone,
  shortHash,
  shortSha,
  stepAt,
} from "../../www/tour.js";

// A payload as the wasm side returns it, cut down to the fields the page reads.
function play(overrides = {}) {
  return {
    mode: "directed",
    model: "muse-spark",
    steps: [
      { item_id: "d-login-error-body", machine_id: "auth", zoom: "close", stage: [], caption: "one" },
      { item_id: "d-links-sort", machine_id: "links", zoom: "machine", stage: [], caption: "two" },
      { item_id: "i-retry-after", machine_id: "ratelimit", zoom: "machine", stage: [], caption: "three" },
    ],
    counts: {
      by_class: { preserved: 1, obligation_fulfilled: 0, out_of_scope: 2, violates_ruling: 1 },
      probes_run: 3,
      probes_total: 8,
      routes_covered: 4,
      routes_total: 9,
      masks_applied: 2,
      behavior: { obligations_total: 7, passed: 5, failed: 1, unknown: 1 },
    },
    items: [{}, {}, {}, {}],
    ...overrides,
  };
}

test("the timings are the move, then a dwell, and the move matches the wasm side", () => {
  assert.equal(MOVE_MS, 600);
  assert.equal(STEP_MS, MOVE_MS + DWELL_MS);
  assert.ok(DWELL_MS > 0);
});

test("the chip reads plain in plain mode, whatever the play holds", () => {
  assert.equal(chipText(PLAIN, play()), "plain");
  assert.equal(chipText(PLAIN, null), "plain");
});

test("the chip names the director when its tour plays, and says deterministic when the order is the origin's", () => {
  assert.equal(chipText(DIRECTED, play()), "directed · muse-spark");
  assert.equal(
    chipText(DIRECTED, play({ mode: "deterministic", model: null })),
    "deterministic",
  );
});

test("directed mode with no play to show falls back to the plain chip", () => {
  assert.equal(chipText(DIRECTED, null), "plain");
});

test("an index is kept inside the walk, and an empty walk stays at zero", () => {
  assert.equal(clampIndex(-1, 3), 0);
  assert.equal(clampIndex(1, 3), 1);
  assert.equal(clampIndex(9, 3), 2);
  assert.equal(clampIndex(4, 0), 0);
});

test("a step is found by index, clamped to the walk, and none without a walk", () => {
  const walk = play();
  assert.equal(stepAt(walk, 1).item_id, "d-links-sort");
  assert.equal(stepAt(walk, 7).item_id, "i-retry-after");
  assert.equal(stepAt(null, 0), null);
  assert.equal(stepAt(play({ steps: [] }), 0), null);
});

test("playback moves one step per tick, dwells on the last step, then stops", () => {
  let run = { index: 0, playing: true };
  run = advance(run, 3);
  assert.deepEqual(run, { index: 1, playing: true });
  run = advance(run, 3);
  assert.deepEqual(run, { index: 2, playing: true });
  run = advance(run, 3);
  assert.deepEqual(run, { index: 2, playing: false });
  assert.deepEqual(advance(run, 3), { index: 2, playing: false });
});

test("a walk that is not playing does not move on a tick", () => {
  const run = { index: 1, playing: false };
  assert.deepEqual(advance(run, 3), run);
});

test("an empty walk stops at once", () => {
  assert.deepEqual(advance({ index: 0, playing: true }, 0), { index: 0, playing: false });
});

test("a settled item in a passing class is green", () => {
  assert.equal(itemTone({ class: "preserved", state: "open" }), "pass");
  assert.equal(itemTone({ class: "obligation_fulfilled", state: "fixed" }), "pass");
});

test("a class that does not pass is never green", () => {
  assert.equal(itemTone({ class: "out_of_scope", state: "open" }), "fail");
  assert.equal(itemTone({ class: "obligation_failed", state: "open" }), "fail");
  assert.equal(itemTone({ class: "violates_ruling", state: "open" }), "fail");
});

test("a residue is amber, and a bounced or stopped item is never green", () => {
  assert.equal(itemTone({ class: "out_of_scope", state: "residue" }), "residue");
  assert.equal(itemTone({ class: "preserved", state: "bounced" }), "fail");
  assert.equal(itemTone({ class: "preserved", state: "stopped" }), "fail");
});

test("a class the page cannot judge is grey, even in a settled state", () => {
  for (const cls of ["unstable_base", "incomparable", "error"]) {
    assert.equal(itemTone({ class: cls, state: "fixed" }), "unknown", cls);
    assert.equal(itemTone({ class: cls, state: "open" }), "unknown", cls);
  }
});

test("a hash is shown without its algorithm and cut to twelve characters", () => {
  const hash = `sha256:${"ab".repeat(32)}`;
  assert.equal(shortHash(hash), "abababababab");
  assert.equal(shortHash("abcdef"), "abcdef");
});

test("a commit is shown in seven characters", () => {
  assert.equal(shortSha("0123456789abcdef"), "0123456");
});

test("the count lines carry the payload's own figures, each with its denominator", () => {
  const lines = countLines(play());
  assert.equal(lines[0], "4 items: 1 preserved · 2 out of scope · 1 violates ruling");
  assert.equal(lines[1], "probes 3 of 8 run");
  assert.equal(lines[2], "routes 4 of 9 covered");
  assert.equal(lines[3], "obligations 5 of 7 passed · 1 failed · 1 unknown");
  assert.equal(lines[4], "2 masks applied");
});

test("a class with no items is left out of the count line", () => {
  const [items] = countLines(play());
  assert.ok(!items.includes("obligation fulfilled"), items);
});

test("the closed count line gives the item total and the obligations passed, with the denominator", () => {
  assert.equal(countSummary(play()), "4 items · obligations 5 of 7 passed");
});
