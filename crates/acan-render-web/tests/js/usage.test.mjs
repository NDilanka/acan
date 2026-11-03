// Node tests for the foreman plate's history rows: the model, the step word and the hover title of
// each row, the verdict a tour's step reads, and the history cap. Run with:
//   node --test crates/acan-render-web/tests/js/usage.test.mjs
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

import { HISTORY_ROWS, foremanLine, roleLabel } from "../../www/usage.js";

const rows = JSON.parse(
  await readFile(new URL("../../www/fixtures/foreman-rows.json", import.meta.url), "utf8"),
);

test("a_detail_row_shows_model_step_and_detail", () => {
  const scope = rows.find((row) => row.role === "scope");
  assert.deepEqual(foremanLine(scope), {
    model: "muse-spark-1.3",
    step: "scope",
    title: "scope draft for o-1 rev 1",
  });
});

test("a_scope_row_reads_scope", () => {
  assert.equal(roleLabel("scope"), "scope");
  const scope = rows.find((row) => row.role === "scope");
  assert.equal(foremanLine(scope).step, "scope");
});

test("a_row_without_detail_shows_the_step_alone", () => {
  const explain = rows.find((row) => row.role === "explain");
  assert.deepEqual(foremanLine(explain), {
    model: "muse-spark-1.3",
    step: "explain",
    title: null,
  });
});

test("an_unknown_step_renders_its_word", () => {
  const future = rows.find((row) => row.role === "future_step");
  assert.deepEqual(foremanLine(future), {
    model: "muse-spark-1.3",
    step: "future_step",
    title: null,
  });
});

test("a_tour_row_reads_directed_or_deterministic", () => {
  const admitted = rows.find((row) => row.detail?.endsWith(": admitted"));
  const refused = rows.find((row) => row.detail?.includes(": refused: "));
  assert.deepEqual(foremanLine(admitted), {
    model: "muse-spark-1.3",
    step: "tour · directed",
    title: "tour for receipt 1a2b3c4d: admitted",
  });
  assert.deepEqual(foremanLine(refused), {
    model: "muse-spark-1.3",
    step: "tour · deterministic",
    title: "tour for receipt 9f8e7d6c: refused: out_of_scope",
  });
});

test("a_refused_tour_reads_deterministic", () => {
  const refused = { role: "director", model: "muse-spark-1.3", detail: "tour for receipt 1a2b3c4d: refused: out_of_scope" };
  const failed = { role: "director", model: "muse-spark-1.3", detail: "tour for receipt 1a2b3c4d: error" };
  assert.equal(foremanLine(refused).step, "tour · deterministic");
  assert.equal(foremanLine(failed).step, "tour · deterministic");
});

test("a_row_with_no_detail_has_no_title", () => {
  const tour = { role: "director", model: "muse-spark-1.3", detail: null };
  assert.deepEqual(foremanLine(tour), {
    model: "muse-spark-1.3",
    step: "tour",
    title: null,
  });
});

test("the_director_role_reads_as_tour", () => {
  assert.equal(roleLabel("director"), "tour");
  assert.equal(roleLabel("future_step"), "future_step");
});

test("history_stays_capped_at_three", () => {
  assert.equal(HISTORY_ROWS, 3);
  assert.equal(rows.slice(0, HISTORY_ROWS).length, 3);
});
