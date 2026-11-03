// Node tests for the frame loop's rule. Run with:
//   node --test crates/acan-render-web/tests/js/frames.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { nextFrame } from "../../www/frames.js";

test("a still app asks for no frame", () => {
  assert.equal(nextFrame({ moving: false, hidden: false, gapMs: 0 }), "stop");
  assert.equal(nextFrame({ moving: false, hidden: false, gapMs: 50 }), "stop");
});

test("a hidden tab asks for no frame, even while drones bob", () => {
  assert.equal(nextFrame({ moving: true, hidden: true, gapMs: 0 }), "stop");
  assert.equal(nextFrame({ moving: true, hidden: true, gapMs: 50 }), "stop");
});

test("a moving app draws the next display frame, or waits out the calm gap", () => {
  assert.equal(nextFrame({ moving: true, hidden: false, gapMs: 0 }), "now");
  assert.equal(nextFrame({ moving: true, hidden: false, gapMs: 50 }), 50);
});

test("the page feeds the rule the tab's visibility and starts again when the tab shows", () => {
  const main = readFileSync(new URL("../../www/main.js", import.meta.url), "utf8");
  assert.match(main, /nextFrame\(\{ moving: turning, hidden: document\.hidden, gapMs: state\.app\.frame_gap_ms\(\) \}\)/);
  assert.match(main, /addEventListener\("visibilitychange", \(\) => \{\s*if \(!document\.hidden\) requestRender\(\);/);
});
