// Node tests for the first-run checklist's page logic. Run with:
//   node --test crates/acan-render-web/tests/js/firstrun.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  DONE,
  FIRSTRUN_KEY,
  INIT_SHIPPED,
  TODO,
  UNAVAILABLE,
  UNKNOWN,
  contractOf,
  contractState,
  progressText,
  setupState,
  shouldShow,
  steps,
  workerCommand,
  workersState,
} from "../../www/firstrun.js";

const ORIGIN = "http://127.0.0.1:7311";
const CONTRACT = { present: true, version: "acan-app/2" };

// A view as the wasm side folds it, cut down to the fields the checklist reads.
function view({ agents = 0, orders = 0, lanes = [] } = {}) {
  return {
    agents: Array.from({ length: agents }, (_, i) => ({ id: `agent-${i + 1}`, task: null, activity: "idle", machine: null, body: null })),
    orders: Array.from({ length: orders }, (_, i) => ({ id: i + 1, machine: "auth", sentence: "s", phase: "building", tasks: 1, at: null })),
    lanes,
  };
}

const KEYED = { status: 200, body: { configured: true, source: "setup", can_save: true, providers: [], roles: [] } };
const NO_KEY = { status: 200, body: { configured: false, source: null, can_save: true, providers: [], roles: [] } };
const ROUTED = { status: 200, body: { configured: false, source: "routing_file", can_save: false, providers: [], roles: [] } };

function byId(list) {
  return Object.fromEntries(list.map((step) => [step.id, step]));
}

test("the steps come in the order a first run takes them", () => {
  assert.deepEqual(steps().map((step) => step.id), ["signin", "key", "contract", "worker", "order", "seal", "watch"]);
  assert.equal(FIRSTRUN_KEY, "acan.firstrun");
});

test("a page with no token starts at sign-in, and links to the token field", () => {
  const list = steps({ origin: ORIGIN });
  const first = byId(list).signin;
  assert.equal(byId(list).contract.action, null);
  assert.equal(first.state, TODO);
  assert.equal(first.current, true);
  assert.deepEqual(first.action, { kind: "focus", target: "token", label: "Sign in" });
  assert.equal(byId(list).key.state, UNKNOWN);
  assert.equal(list.filter((step) => step.current).length, 1);
});

test("the key step follows GET /api/setup, and a server without it does not hold the tester up", () => {
  assert.equal(setupState(KEYED), DONE);
  assert.equal(setupState(NO_KEY), TODO);
  assert.equal(setupState({ status: 404, body: null }), UNAVAILABLE);
  assert.equal(setupState({ status: 500, body: null }), UNKNOWN);
  assert.equal(setupState({ status: 0, body: null }), UNKNOWN);
  assert.equal(setupState(null), UNKNOWN);
  const missing = byId(steps({ token: true, setup: { status: 404, body: null } })).key;
  assert.equal(missing.state, UNAVAILABLE);
  assert.equal(missing.current, false);
  assert.equal(missing.action, null);
  assert.match(missing.detail, /\.env/);
});

test("with no key the step opens the key setup, and a routing file points at its .env", () => {
  const open = byId(steps({ token: true, setup: NO_KEY })).key;
  assert.equal(open.current, true);
  assert.deepEqual(open.action, { kind: "setup", label: "Set a key" });
  const routed = byId(steps({ token: true, setup: ROUTED })).key;
  assert.equal(routed.action, null);
  assert.match(routed.detail, /llm\.toml/);
});

test("the contract step reads the plant's contract field, and a plant without it is unknown", () => {
  assert.equal(contractOf({ machines: [] }), null);
  assert.equal(contractOf({ contract: true }), null);
  assert.deepEqual(contractOf({ contract: { present: true, version: "acan-app/2" } }), { present: true, version: "acan-app/2" });
  assert.deepEqual(contractOf({ contract: { present: false, version: null } }), { present: false, version: null });
  assert.equal(contractState({ machines: [] }), UNKNOWN);
  assert.equal(contractState({ contract: { present: true, version: "acan-app/2" } }), DONE);
  assert.equal(contractState({ contract: { present: false, version: null } }), TODO);
  const present = byId(steps({ token: true, setup: KEYED, contract: { present: true, version: "acan-app/2" } })).contract;
  assert.equal(present.state, DONE);
  assert.equal(present.detail, "Your repository has an app contract (acan-app/2).");
  const absent = byId(steps({ token: true, setup: KEYED, contract: { present: false, version: null } })).contract;
  assert.equal(absent.state, TODO);
  assert.equal(absent.current, true);
});

test("the contract step says to run acan init, marked as coming while the command does not ship", () => {
  assert.equal(INIT_SHIPPED, false);
  const asked = byId(steps({ token: true, setup: KEYED })).contract;
  assert.equal(asked.state, UNKNOWN);
  assert.equal(asked.current, false);
  assert.match(asked.detail, /^Run `acan init` in your repo \(coming in this alpha\), so the gate/);
  assert.deepEqual(asked.action, { kind: "contract", label: "Check again" });
  const silent = byId(steps({ token: true, setup: KEYED, contract: null })).contract;
  assert.match(silent.detail, /cannot report it yet/);
});

function workers(...states) {
  return { status: 200, body: { workers: states.map((state, i) => ({ id: `local-${i + 1}`, state, last: null })) } };
}

test("GET /api/workers decides the worker step when the server has it", () => {
  assert.deepEqual(workersState(workers("running", "no_key")), { kind: "running", running: 1 });
  assert.deepEqual(workersState(workers("no_key", "no_key")), { kind: "no_key", running: 0 });
  assert.deepEqual(workersState(workers()), { kind: "none", running: 0 });
  assert.deepEqual(workersState({ status: 404, body: null }), { kind: "outside", running: 0 });
  assert.deepEqual(workersState({ status: 403, body: null }), { kind: "outside", running: 0 });
  assert.deepEqual(workersState(null), { kind: "outside", running: 0 });
  const base = { token: true, setup: KEYED, contract: CONTRACT, view: view(), origin: ORIGIN };
  const running = byId(steps({ ...base, workers: workers("running") })).worker;
  assert.equal(running.state, DONE);
  assert.equal(running.detail, "1 local worker running.");
  const waiting = byId(steps({ ...base, workers: workers("no_key", "no_key") })).worker;
  assert.equal(waiting.state, TODO);
  assert.equal(waiting.detail, "Workers are waiting for a model key.");
  assert.deepEqual(waiting.action, { kind: "setup", label: "Set a key" });
  const none = byId(steps({ ...base, workers: workers() })).worker;
  assert.equal(none.detail, "Start `acan serve` with `--workers 1`, so it runs a worker for you.");
  assert.equal(none.command, undefined);
  assert.equal(none.action, null);
  // With the route, the outside worker's token path is not offered, even with no agent in the view.
  for (const step of [running, waiting, none]) assert.doesNotMatch(step.detail, /tokens\.json/);
});

test("a server without GET /api/workers falls back to the view's agents and the outside command", () => {
  const waiting = byId(steps({ token: true, setup: KEYED, contract: CONTRACT, workers: { status: 404, body: null }, view: view(), origin: ORIGIN })).worker;
  assert.equal(waiting.state, TODO);
  assert.equal(waiting.current, true);
  assert.match(waiting.detail, /^Start a worker with an agent token from \.acan\/tokens\.json/);
  assert.equal(waiting.command, "acan-worker --server http://127.0.0.1:7311 --token <agent token from .acan/tokens.json> --repo <path to your repo>");
  // The acan-worker these servers ship has no environment variable for the token.
  assert.doesNotMatch(waiting.detail + waiting.command, /ACAN_AGENT_TOKEN/);
  assert.equal(waiting.command, workerCommand(ORIGIN));
  assert.deepEqual(waiting.action, { kind: "copy", label: "Copy command" });
  const connected = byId(steps({ token: true, view: view({ agents: 2 }) })).worker;
  assert.equal(connected.state, DONE);
  assert.equal(connected.detail, "2 workers connected.");
});

test("the order and seal steps follow the view's orders and lanes", () => {
  const before = byId(steps({ token: true, setup: KEYED, contract: CONTRACT, view: view({ agents: 1 }) }));
  assert.equal(before.order.current, true);
  assert.deepEqual(before.order.action, { kind: "focus", target: "compose", label: "Write an order" });
  const drafted = byId(steps({ token: true, setup: KEYED, contract: CONTRACT, view: view({ agents: 1, orders: 1, lanes: [{ state: "unconfirmed" }] }) }));
  assert.equal(drafted.order.state, DONE);
  assert.equal(drafted.seal.current, true);
  assert.deepEqual(drafted.seal.action, { kind: "focus", target: "slip", label: "Open the slip" });
  const sealed = byId(steps({ token: true, setup: KEYED, contract: CONTRACT, view: view({ agents: 1, orders: 1, lanes: [{ state: "unconfirmed" }, { state: "confirmed" }] }) }));
  assert.equal(sealed.seal.state, DONE);
  assert.equal(sealed.watch.current, true);
  assert.deepEqual(sealed.watch.action, { kind: "dismiss", label: "Done" });
});

test("a step this page cannot check does not hold the tester at it", () => {
  const list = byId(steps({ token: true, setup: KEYED, view: view({ agents: 1, orders: 1 }) }));
  assert.equal(list.contract.state, UNKNOWN);
  assert.equal(list.contract.current, false);
  assert.equal(list.seal.current, true);
});

test("the progress line counts done steps, and a step the server does not offer", () => {
  assert.equal(progressText(steps()), "0 of 7 done");
  const list = steps({ token: true, setup: { status: 404, body: null }, contract: CONTRACT, view: view({ agents: 1, orders: 1, lanes: [{ state: "confirmed" }] }) });
  assert.equal(progressText(list), "6 of 7 done");
  assert.equal(progressText(steps({ token: true, setup: KEYED, contract: CONTRACT, view: view({ agents: 1, orders: 1, lanes: [{ state: "confirmed" }] }), watched: true })), "7 of 7 done");
});

test("the checklist never shows on a replay, nor once dismissed", () => {
  assert.equal(shouldShow({}), true);
  assert.equal(shouldShow({ replay: true }), false);
  assert.equal(shouldShow({ dismissed: true }), false);
});
