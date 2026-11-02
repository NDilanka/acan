// Node tests for the feed, the hold and the timeline scrubber's logic, with a 2000-event log for
// the cost of one rebuild on the page's side. Run with:
//   node --test crates/acan-render-web/tests/js/scrub.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { orderHistory, ribbonRows } from "../../www/history.js";
import { inboxItems, visionDoubts } from "../../www/inbox.js";
import { lastActivity } from "../../www/lanes.js";
import {
  SETTLE_MS,
  clampSeq,
  createFeed,
  createHold,
  createSettle,
  eventsAt,
  eventsBefore,
  holdLabel,
  scrubRange,
  timeWords,
} from "../../www/scrub.js";

const T0 = Date.UTC(2026, 10, 3, 14, 0, 0);

function stamped(seq, event, actor = { kind: "system" }) {
  return { seq, at: T0 + seq * 1000, actor, event };
}

function small(count) {
  return Array.from({ length: count }, (_, seq) => stamped(seq, { type: seq === 0 ? "world_opened" : "tick" }));
}

// A run of `count` events: orders posted and split, four agents leasing, renewing, building and
// landing, receipts and vision checks, in the shapes the origin writes.
function bigLog(count) {
  const events = [stamped(0, { type: "world_opened" }, { kind: "member", id: "owner" })];
  const agents = ["agent-a", "agent-b", "agent-c", "agent-d"];
  for (const id of agents) events.push(stamped(events.length, { type: "agent_registered", agent: { id, owner: "owner" } }));
  let order = 0;
  let task = 0;
  let run = 0;
  while (events.length < count) {
    order += 1;
    events.push(stamped(events.length, { type: "order_posted", order, machine: `src/m${order % 12}`, sentence: `order ${order}`, witnesses: [], posted_by: "owner" }));
    const tasks = [task + 1, task + 2];
    task += 2;
    events.push(stamped(events.length, { type: "order_split", order, rev: 0, tasks: tasks.map((id) => [id, {}]) }));
    events.push(stamped(events.length, { type: "scope_confirmed", order, confirmation: { scope_rev: 1 } }));
    for (const id of tasks) {
      const holder = agents[id % agents.length];
      const actor = { kind: "agent", id: holder };
      events.push(stamped(events.length, { type: "lease_granted", task: id, lease: { holder, generation: 1 }, readback: id }, actor));
      for (let k = 0; k < 6; k += 1) events.push(stamped(events.length, { type: "lease_renewed", task: id, generation: 1 }, actor));
      run += 1;
      events.push(stamped(events.length, { type: "build_reported", task: id, generation: 1, head: "abc", run, rev: 0, witnesses: [] }, actor));
      events.push(stamped(events.length, { type: "receipt_recorded", run, record: { verdict: { verdict: "passed" } } }));
      events.push(stamped(events.length, { type: "vision.checked", task: id, gate_run: run, status: "done", counts: { consistent: 3, inconsistent: id % 5 === 0 ? 1 : 0, unclear: 0, not_checked: 0 }, human_questions: 0 }));
      events.push(stamped(events.length, { type: "landed", task: id, commit: `c0ffee${id}` }));
    }
  }
  return events.slice(0, count);
}

test("the feed keeps each event once, and a reload replaces it", () => {
  const feed = createFeed();
  feed.load(small(3));
  assert.equal(feed.next, 3);
  assert.equal(feed.add(stamped(2, { type: "tick" })), false, "a resent event is passed over");
  assert.equal(feed.add(stamped(3, { type: "tick" })), true);
  assert.deepEqual(feed.events.map((event) => event.seq), [0, 1, 2, 3]);
  const version = feed.version;
  feed.load(small(4));
  assert.deepEqual(feed.events.map((event) => event.seq), [0, 1, 2, 3]);
  assert.equal(feed.version, version + 1);
  assert.equal(feed.lastAt(), T0 + 3000);
  assert.equal(createFeed().lastAt(), null);
});

test("the hold keeps every event that arrives while the map is in the past, once", () => {
  const hold = createHold();
  hold.start(small(5));
  assert.equal(hold.on, true);
  assert.equal(hold.push(stamped(5, { type: "tick" })), true);
  assert.equal(hold.push(stamped(5, { type: "tick" })), false, "a resent event is passed over");
  assert.equal(hold.push(stamped(4, { type: "tick" })), false);
  assert.equal(hold.push(stamped(6, { type: "tick" })), true);
  assert.equal(hold.waiting, 2);
  assert.equal(hold.next(), 7);
  const live = hold.end();
  assert.deepEqual(live.map((event) => event.seq), [0, 1, 2, 3, 4, 5, 6], "no event is lost");
  assert.equal(hold.on, false);
  assert.equal(hold.waiting, 0);
});

test("a log reloaded while held replaces the live log and counts what it added", () => {
  const hold = createHold();
  hold.start(small(5));
  hold.push(stamped(5, { type: "tick" }));
  hold.replace(small(9));
  assert.equal(hold.waiting, 4);
  assert.deepEqual(hold.end().map((event) => event.seq), [0, 1, 2, 3, 4, 5, 6, 7, 8]);
});

test("the range starts after the world opened and ends at the newest event", () => {
  assert.deepEqual(scrubRange(small(10)), { min: 1, max: 9 });
  assert.equal(scrubRange(small(1)), null, "a log of one event has nothing to rewind");
  assert.equal(scrubRange([]), null);
  const range = { min: 1, max: 9 };
  assert.equal(clampSeq(0, range), 1);
  assert.equal(clampSeq(42, range), 9);
  assert.equal(clampSeq(4.6, range), 5);
});

test("the map at seq N folds every event up to and including N, as ?upto=N+1 does", () => {
  const events = small(10);
  assert.deepEqual(eventsAt(events, 4).map((event) => event.seq), [0, 1, 2, 3, 4]);
  assert.deepEqual(eventsAt(events, 4), eventsBefore(events, 5));
});

test("the label says HISTORY and the seq, and the time reads in seconds", () => {
  assert.equal(holdLabel(412), "HISTORY · seq 412");
  assert.equal(timeWords(T0 + 61_000, "UTC"), "14:01:01");
});

// Timers the test moves by hand.
function fakeTimers() {
  let now = 0;
  let next = 1;
  const pending = new Map();
  return {
    setTimeout(fn, ms) {
      pending.set(next, { fn, at: now + ms });
      return next++;
    },
    clearTimeout(id) {
      pending.delete(id);
    },
    advance(ms) {
      now += ms;
      for (const [id, timer] of [...pending]) {
        if (timer.at <= now) {
          pending.delete(id);
          timer.fn();
        }
      }
    },
  };
}

test("a drag rebuilds once it pauses for SETTLE_MS, with the last position", () => {
  const timers = fakeTimers();
  const built = [];
  const settle = createSettle((seq) => built.push(seq), SETTLE_MS, timers);
  settle.poke(10);
  timers.advance(100);
  settle.poke(11);
  timers.advance(100);
  settle.poke(12);
  assert.deepEqual(built, [], "no rebuild while the drag moves");
  timers.advance(SETTLE_MS);
  assert.deepEqual(built, [12]);
});

test("a release rebuilds at once and cancels the wait, so it rebuilds once", () => {
  const timers = fakeTimers();
  const built = [];
  const settle = createSettle((seq) => built.push(seq), SETTLE_MS, timers);
  settle.poke(30);
  settle.flush(31);
  timers.advance(SETTLE_MS * 2);
  assert.deepEqual(built, [31]);
  settle.poke(32);
  settle.cancel();
  timers.advance(SETTLE_MS * 2);
  assert.deepEqual(built, [31], "Esc cancels a pending rebuild");
});

test("rewinding a 2000-event log and folding the panels again takes well under the settle pause", (t) => {
  const log = bigLog(2000);
  assert.equal(log.length, 2000);
  const feed = createFeed();
  const times = [];
  let rows = null;
  for (let seq = 100; seq < 2000; seq += 95) {
    const started = performance.now();
    // What one rebuild does on the page's side: cut the log, hand it to load_log as JSON, keep the
    // cut in the feed, and fold the lanes, inbox and ribbon from it.
    const cut = eventsAt(log, seq);
    JSON.stringify(cut);
    feed.load(cut);
    lastActivity(feed.events);
    const doubts = visionDoubts(feed.events);
    rows = ribbonRows(orderHistory(feed.events), { orders: [] }, "UTC");
    inboxItems({ orders: [], tasks: [], questions: [], clashes: [], lanes: [] }, new Map(), doubts);
    times.push(performance.now() - started);
  }
  times.sort((a, b) => a - b);
  const median = times[Math.floor(times.length / 2)];
  const worst = times.at(-1);
  t.diagnostic(`2000-event log: ${times.length} rebuilds, median ${median.toFixed(2)} ms, worst ${worst.toFixed(2)} ms (JS side, without the wasm fold)`);
  assert.ok(rows.length > 50, "the fixture posts many orders");
  assert.ok(median < 50, `a rebuild's JS side took ${median.toFixed(1)} ms at the median`);
});
