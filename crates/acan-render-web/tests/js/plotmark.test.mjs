// Node tests for the plot mark a draft scope rests as. Run with:
//   node --test crates/acan-render-web/tests/js/plotmark.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const marks = readFileSync(new URL("../../www/marks.js", import.meta.url), "utf8");

const constant = (name) => {
  const m = marks.match(new RegExp(`const ${name} = ("#[0-9a-f]{6}"|\\d+(?:\\.\\d+)?);`));
  assert.ok(m, `marks.js declares ${name}`);
  return JSON.parse(m[1]);
};

const rgb = (hex) => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
const luminance = (hex) => {
  const [r, g, b] = rgb(hex).map((c) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
};
const contrast = (a, b) => {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};
const hue = (hex) => {
  const [r, g, b] = rgb(hex).map((c) => c / 255);
  const max = Math.max(r, g, b);
  const d = max - Math.min(r, g, b);
  const h = max === r ? ((g - b) / d) % 6 : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
  return (h * 60 + 360) % 360;
};

// A draft plot takes no body tint, only its outline, so its ring sits on the machine's own white.
const DRAFT_MACHINE = "#ffffff";

test("the draft ring is wider and larger than the old 2 px ring on a 10 px mark", () => {
  assert.ok(constant("DRAFT_RING_WIDTH_PX") > 2);
  assert.ok(constant("DRAFT_RING_PX") > 10);
  assert.ok(constant("DRAFT_RING_PX") >= constant("PLOT_MARK_PX"), "no smaller than the confirmed dot");
});

test("the draft ring holds 3:1 against its untinted machine, more than the old amber did", () => {
  const machine = DRAFT_MACHINE;
  const ring = constant("DRAFT_AMBER");
  const old = constant("AMBER");
  assert.ok(contrast(ring, machine) >= 3, `${ring} on ${machine}: ${contrast(ring, machine).toFixed(2)}`);
  assert.ok(contrast(ring, machine) > contrast(old, machine));
});

test("the draft ring stays in the scope's amber", () => {
  assert.ok(Math.abs(hue(constant("DRAFT_AMBER")) - hue(constant("AMBER"))) < 8);
});
