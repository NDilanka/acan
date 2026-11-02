// Node tests for the needs-you inbox: what waits on the human, the order it is listed in, the
// vision doubts, no duplicates after a re-ingest, the title, the announcement and the
// notification rule. Run with:
//   node --test crates/acan-render-web/tests/js/inbox.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  announcement,
  freshItems,
  inboxItems,
  shouldNotify,
  titleWith,
  visionDoubts,
} from "../../www/inbox.js";

// A view with one of each thing that waits: a clash, a question, a stopped lane, a lane waiting
// for its seal, and the quiet things that do not wait: a settled clash, a sealed lane, and a lane
// of a cancelled order.
function view() {
  return {
    orders: [
      { id: 1, machine: "src/api", phase: "building", sentence: "add a retry header" },
      { id: 2, machine: "src/web", phase: "gating", sentence: "show the banner" },
      { id: 3, machine: "src/db", phase: "building", sentence: "index the table" },
      { id: 4, machine: "src/cli", phase: "cancelled", sentence: "drop the flag" },
      { id: 5, machine: "src/ops", phase: "building", sentence: "rotate logs" },
    ],
    tasks: [
      { id: 1, order: 1, machine: "src/api", state: "building" },
      { id: 2, order: 2, machine: "src/web", state: "gating" },
      { id: 3, order: 2, machine: "src/web", state: "dropped" },
      { id: 4, order: 4, machine: "src/cli", state: "dropped" },
    ],
    questions: [{ id: 7, order: 1, text: "keep the old header?", options: ["yes", "no"] }],
    clashes: [
      { id: 2, state: "open", label: "c-2 · breaks w-3", a: 1, b: 2 },
      { id: 1, state: "settled", label: "c-1", a: 1, b: 2 },
      { id: 3, state: "proposed", label: "c-3", a: 2, b: 1 },
    ],
    lanes: [
      { order: 2, rev: 3, state: "unconfirmed", stopped: false },
      { order: 3, rev: 1, state: "confirmed", stopped: true },
      { order: 4, rev: 1, state: "unconfirmed", stopped: false },
      { order: 5, rev: 2, state: "confirmed", stopped: false },
    ],
  };
}

function vision(seq, task, run, inconsistent, unclear) {
  return { seq, at: 1000 + seq, actor: { kind: "system" }, event: { type: "vision.checked", task, gate_run: run, status: "done", counts: { consistent: 2, inconsistent, unclear, not_checked: 0 }, human_questions: inconsistent + unclear } };
}

test("each kind of waiting thing is listed once, in the order blockers first", () => {
  const items = inboxItems(view());
  assert.deepEqual(items.map((item) => item.key), ["clash:2", "clash:3", "question:7", "stopped:3", "seal:2:3"]);
  const byKey = Object.fromEntries(items.map((item) => [item.key, item]));
  assert.equal(byKey["clash:2"].machine, "src/api", "a clash flies to its lower task's machine");
  assert.equal(byKey["clash:2"].text, "c-2 · breaks w-3: two tasks overlap, so choose how to settle it");
  assert.equal(byKey["clash:3"].text, "c-3: a settle was proposed and waits for yours");
  assert.equal(byKey["question:7"].text, "q-7 asks: keep the old header?");
  assert.equal(byKey["question:7"].machine, "src/api");
  assert.equal(byKey["stopped:3"].text, "o-3 stopped because its repair budget ran out");
  assert.equal(byKey["seal:2:3"].text, "o-2 scope rev 3 waits for your seal");
  assert.equal(byKey["seal:2:3"].machine, "src/web");
});

test("a settled clash, a sealed lane and a cancelled order's lane do not wait", () => {
  const keys = inboxItems(view()).map((item) => item.key);
  assert.equal(keys.includes("clash:1"), false);
  assert.equal(keys.some((key) => key.startsWith("seal:5")), false);
  assert.equal(keys.some((key) => key.includes(":4")), false);
});

test("a new scope revision is a new thing to seal", () => {
  const next = view();
  next.lanes[0].rev = 4;
  const before = new Set(inboxItems(view()).map((item) => item.key));
  assert.deepEqual(freshItems(before, inboxItems(next)).map((item) => item.key), ["seal:2:4"]);
});

test("residue items in a plant view wait for a ruling, one line per order", () => {
  const plantViews = new Map([
    ["o-1", { recorded: 1, play: { items: [
      { item_id: "a", machine_id: "src/api/retry", state: "residue" },
      { item_id: "b", machine_id: "src/api", state: "residue" },
      { item_id: "c", machine_id: "src/api", state: "fixed" },
    ] } }],
    ["o-2", { recorded: 1, play: null, items: [{ item_id: "d", machine_id: "src/web", state: "fixed" }] }],
    ["o-4", { recorded: 1, play: null, items: [{ item_id: "e", machine_id: "src/cli", state: "residue" }] }],
  ]);
  const residue = inboxItems(view(), plantViews).filter((item) => item.kind === "residue");
  assert.deepEqual(residue.map((item) => [item.key, item.text, item.machine]), [
    ["residue:o-1", "o-1 has 2 residue items to rule on", "src/api/retry"],
  ]);
});

test("a vision check that found something inconsistent or unclear waits; a later clean run clears it", () => {
  const events = [vision(1, 1, 10, 1, 0), vision(2, 2, 11, 0, 2), vision(3, 2, 12, 0, 0), vision(4, 1, 13, 1, 2)];
  const doubts = visionDoubts(events);
  assert.deepEqual([...doubts.keys()], [1]);
  const items = inboxItems(view(), new Map(), doubts).filter((item) => item.kind === "vision");
  assert.deepEqual(items.map((item) => [item.key, item.text, item.machine]), [
    ["vision:1:13", "t-1 vision check found 1 inconsistent, 2 unclear; it is advisory, so take a look", "src/api"],
  ]);
});

test("a vision doubt on a dropped task or a done order does not wait", () => {
  const doubts = visionDoubts([vision(1, 3, 10, 1, 0), vision(2, 4, 11, 1, 0)]);
  assert.deepEqual(inboxItems(view(), new Map(), doubts).filter((item) => item.kind === "vision"), []);
});

test("folding the same log again yields the same keys, so nothing is new and nothing is twice", () => {
  const events = [vision(1, 1, 10, 1, 0)];
  const first = inboxItems(view(), new Map(), visionDoubts(events));
  const again = inboxItems(view(), new Map(), visionDoubts([...events, ...events]));
  assert.deepEqual(again, first);
  assert.equal(new Set(again.map((item) => item.key)).size, again.length);
  assert.deepEqual(freshItems(new Set(first.map((item) => item.key)), again), []);
});

test("the title carries the count only when something waits", () => {
  assert.equal(titleWith("acan", 3), "(3) acan");
  assert.equal(titleWith("acan", 0), "acan");
});

test("the announcement names the first new item and the total, in one plain line", () => {
  const items = inboxItems(view());
  assert.equal(announcement([], 0), "Nothing waits on you.");
  assert.equal(announcement([], 1), "1 thing waits on you.");
  assert.equal(announcement(items.slice(2, 3), 5), "New: q-7 asks: keep the old header? 5 things wait on you.");
  assert.equal(announcement(items.slice(0, 2), 5), `New: ${items[0].text}. And 1 more. 5 things wait on you.`);
});

test("a notification goes out only while hidden, opted in, allowed and live", () => {
  const yes = { hidden: true, optedIn: true, permission: "granted", live: true };
  assert.equal(shouldNotify(yes), true);
  assert.equal(shouldNotify({ ...yes, hidden: false }), false);
  assert.equal(shouldNotify({ ...yes, optedIn: false }), false);
  assert.equal(shouldNotify({ ...yes, permission: "default" }), false);
  assert.equal(shouldNotify({ ...yes, live: false }), false, "a replay or a rewound map notifies nobody");
});
