// Node tests for the panels' room on the page: every panel is one the map's names and flags
// avoid, the right rail holds the panels that used to share its corner, and a panel opened beside
// a machine stays inside the free band. Run with:
//   node --test crates/acan-render-web/tests/js/hud.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { composeSpot } from "../../www/order.js";
import { viewerPlacement } from "../../www/vision.js";

const www = (name) => readFileSync(new URL(`../../www/${name}`, import.meta.url), "utf8");

function hudSelectors() {
  const block = www("marks.js").match(/export const HUD_SELECTORS = \[([\s\S]*?)\]/);
  assert.ok(block, "marks.js declares HUD_SELECTORS as a list");
  return [...block[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]);
}

test("the names and flags avoid every new panel", () => {
  const selectors = hudSelectors();
  for (const wanted of ["#watch > .island", "#compose", "#command", "#question", "#vision", ".vpin", "#receipt"]) {
    assert.ok(selectors.includes(wanted), `HUD_SELECTORS has ${wanted}`);
  }
  const html = www("index.html");
  for (const selector of selectors.filter((s) => /^#[\w-]+$/.test(s))) {
    assert.match(html, new RegExp(`id="${selector.slice(1)}"`), `${selector} is on the page`);
  }
});

test("the first-run checklist, the clash card and the slip sit in the right rail, before the watch panels", () => {
  const rail = www("index.html").match(/<div id="watch">([\s\S]*?)<\/div>/)[1];
  const ids = [...rail.matchAll(/id="([\w-]+)"/g)].map((m) => m[1]);
  assert.deepEqual(ids, ["firstrun", "card", "slip", "inbox", "lanes", "history", "scrub"]);
  assert.equal((www("index.html").match(/id="card"/g) ?? []).length, 1, "the card is on the page once");
});

test("a panel beside a machine stays in the free band, left of the rail and right of the receipt", () => {
  // 1280 wide: the receipt ends at 342 and the rail starts at 968.
  const right = composeSpot({ x: 950, y: 300 }, 300, 120, 968, 720, 342);
  assert.ok(right.left + 300 <= 968 - 12 + 0.001, `left ${right.left}`);
  const left = composeSpot({ x: 100, y: 300 }, 300, 120, 968, 720, 342);
  assert.ok(left.left >= 342 + 12, `left ${left.left}`);
  assert.deepEqual(composeSpot({ x: 400, y: 300 }, 300, 120, 1280, 720), composeSpot({ x: 400, y: 300 }, 300, 120, 1280, 720, 0));
});

test("the vision viewer keeps to the free band too", () => {
  const spot = viewerPlacement({ pin: { left: 900, top: 300, w: 120, h: 20 }, size: [440, 400], viewport: [968, 720], from: 342 });
  assert.ok(spot.left >= 342 + 12 && spot.left + 440 <= 968 - 12, `left ${spot.left}`);
  const kept = viewerPlacement({ pin: null, size: [440, 400], viewport: [968, 720], last: { left: 0, top: 0 }, from: 342 });
  assert.equal(kept.left, 354);
});
