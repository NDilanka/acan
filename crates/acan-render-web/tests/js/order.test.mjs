// Node tests for the order a human posts from the map: the command, the refusal sentences, what
// a machine holds, and the picking of a machine on screen. Run with:
//   node --test crates/acan-render-web/tests/js/order.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  EMPTY_LINE,
  centreMachine,
  composeSpot,
  insideQuad,
  machineAt,
  machineCentre,
  machineContext,
  orderCommand,
  orderRefusal,
  postedOrder,
  shownOrderOf,
} from "../../www/order.js";

// A flat projection: one world unit is ten CSS pixels, with no turn or tilt.
const flat = (x, y) => [x * 10, y * 10];

const MACHINES = [
  { name: "auth", min: [0, 0], size: [10, 10], abnormal: false },
  { name: "auth/session", min: [2, 2], size: [3, 3], abnormal: false },
  { name: "store", min: [20, 0], size: [10, 10], abnormal: false },
];

function view(extra = {}) {
  return { machines: MACHINES, orders: [], agents: [], tasks: [], questions: [], clashes: [], lanes: [], ...extra };
}

test("an order is one sentence on its machine, sent with no witnesses", () => {
  assert.deepEqual(orderCommand("auth", "  Keep the session   cookie\nsecure. "), {
    ok: true,
    command: { cmd: "post_order", machine: "auth", sentence: "Keep the session cookie secure.", witnesses: [] },
  });
});

test("an empty or blank sentence is refused on the page, and nothing is sent", () => {
  for (const text of ["", "   ", "\n\t", undefined]) {
    assert.deepEqual(orderCommand("auth", text), { ok: false, line: EMPTY_LINE });
  }
});

test("the posted order's id comes from the route's order_posted event", () => {
  const events = [
    { seq: 4, at: 1, actor: { kind: "member", id: "owner" }, event: { type: "order_posted", order: 7, machine: "auth" } },
  ];
  assert.equal(postedOrder(events), 7);
  assert.equal(postedOrder([]), null);
  assert.equal(postedOrder(null), null);
});

test("each refusal reads as one plain sentence, never as the server's JSON", () => {
  const cases = [
    [403, '{"error":"forbidden","why":"only members may do this over HTTP"}', /Only a member can post an order/],
    [400, "Failed to parse the request body as JSON", /could not read this order/],
    [422, "Failed to deserialize the JSON body: machine", /could not read this order/],
    [409, '{"reason":"forbidden","why":"not a member of this world"}', /not a member of this world/],
    [409, '{"reason":"invalid","why":"an order needs a sentence"}', /did not take it: an order needs a sentence\. Nothing was posted\./],
    [409, '{"reason":"wrong_state","what":"order","found":"cancelled"}', /The order is cancelled/],
    [409, "not json", /refused the order/],
    [500, '{"error":"internal"}', /failed to take the order\. Try again\./],
    [401, '{"error":"unauthorized"}', /Sign in again/],
  ];
  for (const [status, body, expected] of cases) {
    const line = orderRefusal(status, body);
    assert.match(line, expected, `${status} ${body}`);
    assert.doesNotMatch(line, /[{}]/, "no JSON leaks into the line");
    assert.equal(line.split(/(?<=[.!?]) /).length <= 2, true, "a short line");
  }
});

test("the machine's context names its orders in flight and its agents", () => {
  const busy = view({
    orders: [
      { id: 1, machine: "auth", sentence: "a", phase: "split", tasks: 2, at: null },
      { id: 2, machine: "auth", sentence: "b", phase: "landed", tasks: 1, at: null },
      { id: 3, machine: "store", sentence: "c", phase: "awaiting_review", tasks: 1, at: null },
    ],
    agents: [
      { id: "w1", task: 0, activity: "editing", machine: "auth", body: null },
      { id: "w2", task: 1, activity: "editing", machine: "auth", body: null },
    ],
  });
  assert.equal(machineContext(busy, "auth"), "Here now: o-1 split · 2 agents here.");
  assert.equal(machineContext(busy, "store"), "Here now: o-3 awaiting review.");
  assert.equal(machineContext(busy, "nowhere"), "Nothing is in flight here.");
});

test("the island names the order this page posted last while it is in flight", () => {
  const orders = [
    { id: 0, machine: "auth", sentence: "a", phase: "splitting", tasks: 0, at: null },
    { id: 3, machine: "store", sentence: "b", phase: "split", tasks: 1, at: null },
    { id: 4, machine: "store", sentence: "c", phase: "landed", tasks: 1, at: null },
  ];
  const held = view({ orders });
  assert.equal(shownOrderOf(held).id, 0, "with nothing posted, the first in flight");
  assert.equal(shownOrderOf(held, 3).id, 3, "the page's own order, though an older one is open");
  assert.equal(shownOrderOf(held, 4).id, 0, "a posted order that landed gives way");
  assert.equal(shownOrderOf(held, 9).id, 0, "an id the view lacks yet gives way");
  const done = view({ orders: [orders[2]] });
  assert.equal(shownOrderOf(done, null).id, 4, "with none in flight, the newest");
  assert.equal(shownOrderOf(view(), 3), undefined);
});
test("a click inside a footprint picks the machine, the smallest one first", () => {
  assert.equal(machineAt(MACHINES, flat, 80, 80), "auth");
  assert.equal(machineAt(MACHINES, flat, 30, 30), "auth/session", "the inner machine wins");
  assert.equal(machineAt(MACHINES, flat, 250, 50), "store");
  assert.equal(machineAt(MACHINES, flat, 150, 50), null, "open ground picks nothing");
});

test("a click near a shown name picks its machine when no footprint holds it", () => {
  const labels = [{ name: "store", x: 250, y: 120 }];
  assert.equal(machineAt(MACHINES, flat, 255, 130, labels), "store");
  assert.equal(machineAt(MACHINES, flat, 150, 130, labels), null, "too far from the name");
});

test("the even-odd test holds for a turned footprint", () => {
  const diamond = [[50, 0], [100, 50], [50, 100], [0, 50]];
  assert.equal(insideQuad(diamond, 50, 50), true);
  assert.equal(insideQuad(diamond, 5, 5), false);
});

test("order here means the machine nearest the middle of the canvas, on screen", () => {
  assert.equal(centreMachine(MACHINES, flat, 500, 100), "store");
  assert.equal(centreMachine(MACHINES, flat, 100, 100), "auth");
  const away = (x, y) => [x * 10 - 5000, y * 10];
  assert.equal(centreMachine(MACHINES, away, 500, 100), null, "nothing on screen");
  assert.deepEqual(machineCentre(MACHINES[0], flat), { x: 50, y: 50 });
});

test("the panel sits right of its machine, flips left at the edge, and stays inside", () => {
  assert.deepEqual(composeSpot({ x: 100, y: 300 }, 300, 120, 1200, 800), { left: 122, top: 240, side: "right" });
  assert.deepEqual(composeSpot({ x: 1100, y: 300 }, 300, 120, 1200, 800), { left: 778, top: 240, side: "left" });
  assert.deepEqual(composeSpot({ x: 100, y: 10 }, 300, 120, 1200, 800), { left: 122, top: 12, side: "right" });
  assert.deepEqual(composeSpot({ x: 100, y: 790 }, 300, 120, 1200, 800), { left: 122, top: 668, side: "right" });
});
