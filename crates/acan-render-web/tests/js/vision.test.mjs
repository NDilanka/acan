// Node tests for the vision pins' page logic. Run with:
//   node --test crates/acan-render-web/tests/js/vision.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  ADVISORY,
  checkView,
  countsLine,
  hasPin,
  imagePaths,
  pinLabel,
  secondLook,
  spelled,
  summarize,
  verdictCounts,
  verdictInfo,
  visionTasks,
  witnessOff,
} from "../../www/vision.js";

function check(verdict, overrides = {}) {
  return {
    index: 0,
    route: "/dashboard",
    comparison: "baseline_vs_candidate",
    must_keep: [{ id: "k-1", text: "The usage card stays." }],
    baseline_image_sha256: "sha256:a",
    candidate_image_sha256: "sha256:b",
    verdict,
    reason: verdict === "not_checked" ? null : "a reason",
    not_checked_reason: verdict === "not_checked" ? "vision is off: it needs a Meta key" : null,
    ...overrides,
  };
}

function record(checks, overrides = {}) {
  return { version: "vision_witness/1", task: "t-2", gate_run: "g-3", status: "done", model: "muse-spark-1.3", provider: "meta", failed_reason: null, checks, ...overrides };
}

test("each verdict the server sends has its own words, and none of them is a pass", () => {
  for (const verdict of ["consistent", "inconsistent", "unclear", "not_checked", "pending", "failed"]) {
    const info = verdictInfo(verdict);
    assert.equal(info.verdict, verdict);
    assert.doesNotMatch(info.word, /pass|fail(ed)? the gate|ok/i);
  }
  assert.equal(verdictInfo("not_checked").word, "not checked");
  assert.equal(verdictInfo("consistent").tone, "consistent");
  assert.equal(verdictInfo("inconsistent").tone, "doubt");
  assert.equal(verdictInfo("unclear").tone, "doubt");
  assert.equal(verdictInfo("not_checked").tone, "none");
  assert.equal(verdictInfo("pending").tone, "none");
  assert.equal(verdictInfo("failed").tone, "none");
});

test("a verdict this page does not know makes no claim", () => {
  const info = verdictInfo("passed");
  assert.equal(info.verdict, "unknown");
  assert.equal(info.tone, "none");
});

test("the advisory line says the gate never reads the verdict", () => {
  assert.match(ADVISORY, /^Advisory/);
  assert.match(ADVISORY, /gate never reads/);
});

test("a page no model judged keeps the record from reading as consistent", () => {
  const summary = summarize(record([check("consistent"), check("consistent", { index: 1 }), check("not_checked", { index: 2 })]));
  assert.equal(summary.verdict, "not_checked");
  assert.equal(summary.word, "not checked");
  assert.equal(summary.tone, "none");
  assert.equal(summary.counts, "3 pages: 2 consistent · 1 not checked");
});

test("only a record whose every page is consistent reads as consistent", () => {
  const summary = summarize(record([check("consistent"), check("consistent", { index: 1 })]));
  assert.equal(summary.verdict, "consistent");
  assert.equal(summary.counts, "2 pages: 2 consistent");
});

test("a doubt wins over clean pages, and inconsistent over unclear", () => {
  assert.equal(summarize(record([check("consistent"), check("unclear")])).verdict, "unclear");
  assert.equal(summarize(record([check("unclear"), check("inconsistent"), check("consistent")])).verdict, "inconsistent");
});

test("a done record with no pages reads as not checked, never consistent", () => {
  const summary = summarize(record([]));
  assert.equal(summary.verdict, "not_checked");
  assert.equal(summary.counts, "no pages were checked");
});

test("a pending record and a failed one say so, with the reason it failed", () => {
  assert.equal(summarize(record([], { status: "pending" })).verdict, "pending");
  const failed = summarize(record([], { status: "failed", failed_reason: "the server stopped before the vision check finished" }));
  assert.equal(failed.verdict, "failed");
  assert.equal(failed.line, "The vision check could not run: the server stopped before the vision check finished.");
});

test("an unknown verdict on a check counts as not checked", () => {
  assert.deepEqual(verdictCounts(record([check("passed")])), { consistent: 0, inconsistent: 0, unclear: 0, not_checked: 1 });
  assert.equal(countsLine({ consistent: 1, inconsistent: 0, unclear: 0, not_checked: 0 }, 1), "1 page: 1 consistent");
});

test("a server with no vision witness shows no pin, but a check that stopped does", () => {
  assert.equal(hasPin(record([], { status: "failed", failed_reason: "this server runs no vision witness" })), false);
  assert.equal(hasPin(record([], { status: "failed", failed_reason: "the server stopped before the vision check finished" })), true);
  assert.equal(hasPin(record([], { status: "pending" })), true);
  assert.equal(hasPin(record([check("consistent")])), true);
  assert.equal(hasPin(null), false);
});

test("only a failed record with the no-witness reason and no checks reads as vision off", () => {
  const off = record([], { status: "failed", failed_reason: "this server runs no vision witness" });
  assert.equal(witnessOff(off), true);
  assert.equal(witnessOff({ ...off, checks: [check("not_checked")] }), false);
  assert.equal(witnessOff({ ...off, status: "done" }), false);
  assert.equal(witnessOff(record([], { status: "failed", failed_reason: "the server stopped before the vision check finished" })), false);
  assert.equal(witnessOff(null), false);
});

test("a check that was not judged gives the server's reason, not a model's", () => {
  const view = checkView(check("not_checked", { comparison: "candidate_only" }));
  assert.equal(view.word, "not checked");
  assert.equal(view.reason, "vision is off: it needs a Meta key");
  assert.equal(view.compared, "after only");
  assert.deepEqual(view.mustKeep, ["The usage card stays."]);
});

test("the second look sits beside the first and says whether it agrees", () => {
  assert.equal(secondLook(check("inconsistent")), null);
  const agrees = secondLook(check("inconsistent", { second_look: { model: "muse-wonder", verdict: "inconsistent", agrees: true, reason: "gone" } }));
  assert.equal(agrees.line, "Agrees with the first look: inconsistent.");
  assert.equal(agrees.model, "muse-wonder");
  const differs = secondLook(check("unclear", { second_look: { model: "muse-wonder", verdict: "consistent", agrees: false, reason: "kept" } }));
  assert.equal(differs.line, "Disagrees with the first look: it sees consistent.");
  const none = secondLook(check("unclear", { second_look: { model: null, verdict: "not_checked", agrees: null, not_checked_reason: "wonder is off: it needs a Meta key" } }));
  assert.equal(none.line, "Could not take a second look: wonder is off: it needs a Meta key.");
  assert.equal(none.tone, "none");
});

test("the image routes follow the server's, and a side with no hash has no image", () => {
  assert.deepEqual(imagePaths("t-2", check("consistent", { index: 3 })), {
    before: "/api/tasks/t-2/vision/3/baseline.png",
    after: "/api/tasks/t-2/vision/3.png",
  });
  assert.deepEqual(imagePaths("t-2", check("not_checked", { baseline_image_sha256: null, candidate_image_sha256: null })), { before: null, after: null });
});

test("the page asks for the tasks on a machine whose order has a receipt, with the route spelling", () => {
  const view = {
    tasks: [
      { id: 1, order: 1, machine: "auth" },
      { id: 2, order: 2, machine: "billing" },
      { id: 3, order: 2, machine: null },
      { id: 4, order: 3, machine: "web" },
    ],
    receipts: [{ order: "o-2", recorded: 1 }, { order: "o-3", recorded: 2 }],
  };
  assert.deepEqual(visionTasks(view), [
    { task: "t-2", machine: "billing", order: "o-2", key: 1 },
    { task: "t-4", machine: "web", order: "o-3", key: 2 },
  ]);
  assert.deepEqual(visionTasks(null), []);
  assert.equal(spelled("t", "t-9"), "t-9");
});

test("the pin's name says the verdict is advisory", () => {
  assert.equal(
    pinLabel("t-2", "billing", summarize(record([check("unclear")]))),
    "Vision check of t-2 on billing: unclear, advisory. Open the before and after pages.",
  );
});

test("the fixture record reads as inconsistent, with one page not checked and a second look", () => {
  const fixture = JSON.parse(readFileSync(new URL("../../www/fixtures/vision-witness.json", import.meta.url), "utf8"));
  const summary = summarize(fixture.record);
  assert.equal(summary.verdict, "inconsistent");
  assert.equal(summary.counts, "3 pages: 1 consistent · 1 inconsistent · 1 not checked");
  assert.notEqual(secondLook(fixture.record.checks[1]), null);
});
