// Node tests for the colour themes: their list, how applyTheme sets the page's theme, and how
// nextTheme steps through them. Run with:
//   node --test crates/acan-render-web/tests/js/themes.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { THEMES, applyTheme, nextTheme } from "../../www/themes.js";

test("the four themes come in order, each with its label", () => {
  assert.deepEqual(THEMES, [
    { id: "paper", label: "Light paper" },
    { id: "dark", label: "Dark" },
    { id: "blueprint", label: "Blueprint" },
    { id: "high_contrast", label: "High contrast" },
  ]);
});

test("applyTheme sets the page's theme and returns the id it applied", () => {
  const root = { dataset: {} };
  assert.equal(applyTheme(root, "dark"), "dark");
  assert.equal(root.dataset.theme, "dark");
  assert.equal(applyTheme(root, "high_contrast"), "high_contrast");
  assert.equal(root.dataset.theme, "high_contrast");
});

test("every theme id applies as itself", () => {
  for (const { id } of THEMES) {
    assert.equal(applyTheme({ dataset: {} }, id), id);
  }
});

test("an unknown theme falls back to light paper", () => {
  const root = { dataset: { theme: "dark" } };
  assert.equal(applyTheme(root, "sepia"), "paper");
  assert.equal(root.dataset.theme, "paper");
  assert.equal(applyTheme({ dataset: {} }, undefined), "paper");
});

test("nextTheme steps to the following theme", () => {
  assert.equal(nextTheme("paper"), "dark");
  assert.equal(nextTheme("dark"), "blueprint");
  assert.equal(nextTheme("blueprint"), "high_contrast");
});

test("nextTheme wraps at both ends", () => {
  assert.equal(nextTheme("high_contrast"), "paper");
  assert.equal(nextTheme("paper", -1), "high_contrast");
  assert.equal(nextTheme("dark", -1), "paper");
});

test("nextTheme moves by the step it is given, wrapping around", () => {
  assert.equal(nextTheme("paper", 2), "blueprint");
  assert.equal(nextTheme("paper", 5), "dark");
  assert.equal(nextTheme("paper", -5), "high_contrast");
});

test("an unknown id counts as the first theme", () => {
  assert.equal(nextTheme("sepia"), "dark");
  assert.equal(nextTheme(undefined, -1), "high_contrast");
});
