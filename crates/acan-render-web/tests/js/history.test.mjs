// Node tests for the land and history ribbon: what happened to each order, how each row ends,
// the Cancel command and its refusals. Run with:
//   node --test crates/acan-render-web/tests/js/history.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  cancelCommand,
  cancelRefusal,
  clockWords,
  orderHistory,
  ribbonRows,
  shortSha,
} from "../../www/history.js";

const T0 = Date.UTC(2026, 10, 3, 14, 2, 0);
const SYSTEM = { kind: "system" };
const OWNER = { kind: "member", id: "owner" };

let seq = 0;
function at(minutes, event, actor = SYSTEM) {
  return { seq: seq++, at: T0 + minutes * 60_000, actor, event };
}

// o-1 is sealed, gated through a receipt, and lands. o-2 is gated by a legacy gate run and fails
// to land. o-3 is cancelled. o-4 is posted and still splitting.
function log() {
  seq = 0;
  return [
    at(0, { type: "world_opened" }, OWNER),
    at(0, { type: "order_posted", order: 1, machine: "src/api", sentence: "add a retry header", witnesses: [], posted_by: "owner" }, OWNER),
    at(1, { type: "order_split", order: 1, rev: 0, tasks: [[1, {}], [2, {}]] }),
    at(2, { type: "scope_confirmed", order: 1, confirmation: { scope_rev: 2 } }, OWNER),
    at(3, { type: "order_posted", order: 2, machine: "src/web", sentence: "show the banner", witnesses: [], posted_by: "owner" }, OWNER),
    at(3, { type: "order_split", order: 2, rev: 0, tasks: [[3, {}]] }),
    at(5, { type: "build_reported", task: 1, generation: 1, head: "aaa", run: 1, rev: 0, witnesses: [] }),
    at(6, { type: "receipt_recorded", run: 1, record: { verdict: { verdict: "passed" } } }),
    at(7, { type: "landed", task: 1, commit: "1a2b3c4d5e6f7a8b" }),
    at(7, { type: "build_reported", task: 3, generation: 1, head: "bbb", run: 2, rev: 0, witnesses: [1] }),
    at(8, { type: "gate_closed", run: 2, verdict: { verdict: "passed" } }),
    at(9, { type: "land_failed", task: 3, reason: "the merge conflicted in src/web/banner.ts" }),
    at(9, { type: "order_posted", order: 3, machine: "src/db", sentence: "index the table", witnesses: [], posted_by: "owner" }, OWNER),
    at(10, { type: "order_cancelled", order: 3 }, OWNER),
    at(11, { type: "order_posted", order: 4, machine: "src/cli", sentence: "drop the flag", witnesses: [], posted_by: "owner" }, OWNER),
  ];
}

function view(phases = { 1: "landing", 2: "gating", 3: "cancelled", 4: "splitting" }) {
  return { orders: Object.entries(phases).map(([id, phase]) => ({ id: Number(id), phase })) };
}

test("the history ties tasks to orders by the split and gate runs to tasks by the build report", () => {
  const history = orderHistory(log());
  const one = history.get(1);
  assert.equal(one.machine, "src/api");
  assert.deepEqual(one.sealed, { at: T0 + 2 * 60_000, seq: 3, rev: 2 });
  assert.equal(one.gated.verdict, "passed");
  assert.deepEqual(one.landed.map((land) => land.commit), ["1a2b3c4d5e6f7a8b"]);
  assert.equal(history.get(2).gated.verdict, "passed");
  assert.equal(history.get(2).failed.reason, "the merge conflicted in src/web/banner.ts");
  assert.notEqual(history.get(3).cancelled, null);
});

test("rows are newest first and end landed <sha>, failed <reason>, cancelled, or the live phase", () => {
  const rows = ribbonRows(orderHistory(log()), view({ 1: "landed", 2: "gating", 3: "cancelled", 4: "splitting" }), "UTC");
  assert.deepEqual(rows.map((row) => row.label), ["o-4", "o-3", "o-2", "o-1"]);
  const [four, three, two, one] = rows;
  assert.deepEqual(one.steps.map((step) => step.text), ["posted 14:02", "sealed rev 2", "gated passed"]);
  assert.deepEqual(one.end, { kind: "landed", text: "landed 1a2b3c4", commit: "1a2b3c4d5e6f7a8b" });
  assert.deepEqual(two.steps.map((step) => step.kind), ["posted", "gated"]);
  assert.deepEqual(two.end, { kind: "failed", text: "failed: the merge conflicted in src/web/banner.ts" });
  assert.deepEqual(three.end, { kind: "cancelled", text: "cancelled" });
  assert.deepEqual(four.end, { kind: "live", text: "splitting" });
});

test("only an order that has neither landed nor been cancelled can be cancelled", () => {
  const rows = ribbonRows(orderHistory(log()), view({ 1: "landed", 2: "gating", 3: "cancelled", 4: "splitting" }), "UTC");
  assert.deepEqual(rows.map((row) => [row.label, row.cancellable]), [["o-4", true], ["o-3", false], ["o-2", true], ["o-1", false]]);
});

test("a task that lands after a failed land clears the failure", () => {
  const events = log();
  events.push({ seq: events.length, at: T0 + 20 * 60_000, actor: SYSTEM, event: { type: "landed", task: 3, commit: "ffeeddccbb" } });
  const two = ribbonRows(orderHistory(events), view({ 2: "landed" }), "UTC").find((row) => row.id === 2);
  assert.equal(two.end.text, "landed ffeeddc");
});

test("a second landed task shows as +1, and a new scope revision voids the seal", () => {
  const events = log();
  events.push({ seq: events.length, at: T0, actor: SYSTEM, event: { type: "landed", task: 2, commit: "9999999aaaa" } });
  events.push({ seq: events.length, at: T0, actor: OWNER, event: { type: "order_posted", order: 5, machine: "src/ops", sentence: "rotate logs", witnesses: [], posted_by: "owner" } });
  events.push({ seq: events.length, at: T0, actor: OWNER, event: { type: "scope_confirmed", order: 5, confirmation: { scope_rev: 1 } } });
  events.push({ seq: events.length, at: T0, actor: OWNER, event: { type: "scope_revised", order: 5, scope: {}, scope_hash: "h" } });
  const rows = ribbonRows(orderHistory(events), view({ 1: "landed", 5: "building" }), "UTC");
  assert.equal(rows.find((row) => row.id === 1).end.text, "landed 9999999 +1");
  assert.deepEqual(rows.find((row) => row.id === 5).steps.map((step) => step.kind), ["posted"]);
});

test("folding the same log twice gives the same rows", () => {
  const once = ribbonRows(orderHistory(log()), view(), "UTC");
  assert.deepEqual(ribbonRows(orderHistory(log()), view(), "UTC"), once);
});

test("the Cancel sends the origin's cancel_order command", () => {
  assert.deepEqual(cancelCommand(4), { cmd: "cancel_order", order: 4 });
});

test("each refusal is one plain sentence, never the server's JSON", () => {
  const cases = [
    [409, { reason: "forbidden", why: "only the poster or an owner cancels an order" }, "Only the order's poster or an owner can cancel it, so nothing was recorded."],
    [409, { reason: "wrong_state", what: "order", found: "landed" }, "The order has already landed, so it cannot be cancelled."],
    [409, { reason: "unknown", what: "order", id: "o-9" }, "The server does not know this order, so nothing was recorded."],
    [500, null, "The server failed to take the cancel, so try again."],
    [400, "not json", "The server refused the cancel, so nothing was recorded."],
  ];
  for (const [status, body, words] of cases) {
    const text = typeof body === "string" ? body : JSON.stringify(body);
    const sentence = cancelRefusal(status, text);
    assert.equal(sentence, words);
    assert.doesNotMatch(sentence, /[{}]/);
    assert.equal(sentence.split(". ").length, 1, "one sentence");
  }
});

test("a commit shows in seven characters and a time as hours and minutes", () => {
  assert.equal(shortSha("1a2b3c4d5e"), "1a2b3c4");
  assert.equal(clockWords(T0, "UTC"), "14:02");
});
