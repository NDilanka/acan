// Node tests for the first-run checklist's page logic. Run with:
//   node --test crates/acan-render-web/tests/js/firstrun.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  DONE,
  FIRSTRUN_KEY,
  INIT_SHIPPED,
  TODO,
  UNAVAILABLE,
  UNKNOWN,
  CONTRACT_ABSENT,
  allDone,
  contractOf,
  contractState,
  isWindows,
  progressShort,
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

// Runs a test body with navigator stubbed to nav, so the worker command follows the stub's platform
// and not the host's. The real navigator is put back after the body, however it ends.
const POSIX = { platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; Linux x86_64)" };
const WINDOWS = { platform: "Win32", userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)" };
function onPlatform(nav, body) {
  return () => {
    const had = Object.getOwnPropertyDescriptor(globalThis, "navigator");
    Object.defineProperty(globalThis, "navigator", { value: nav, configurable: true, writable: true });
    try {
      body();
    } finally {
      if (had) Object.defineProperty(globalThis, "navigator", had);
      else delete globalThis.navigator;
    }
  };
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

test("the contract step reads the plant's contract field, and a plant without it does not offer the step", () => {
  assert.equal(contractOf({ machines: [] }), CONTRACT_ABSENT);
  assert.equal(contractOf({ contract: true }), null);
  assert.equal(contractOf({ contract: null }), null, "a malformed flag is a failed read, not absent");
  assert.deepEqual(contractOf({ contract: { present: true, version: "acan-app/2" } }), { present: true, version: "acan-app/2" });
  assert.deepEqual(contractOf({ contract: { present: false, version: null } }), { present: false, version: null });
  assert.equal(contractState({ machines: [] }), UNAVAILABLE);
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
  assert.equal(silent.state, UNKNOWN, "a failed read keeps the step unknown");
  assert.match(silent.detail, /could not read it/);
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

test("a server without GET /api/workers falls back to the view's agents and the outside command", onPlatform(POSIX, () => {
  const waiting = byId(steps({ token: true, setup: KEYED, contract: CONTRACT, workers: { status: 404, body: null }, view: view(), origin: ORIGIN })).worker;
  assert.equal(waiting.state, TODO);
  assert.equal(waiting.current, true);
  assert.match(waiting.detail, /^Start a worker with an agent token from \.acan\/tokens\.json/);
  assert.equal(waiting.command, "ACAN_AGENT_TOKEN=<agent token from .acan/tokens.json> acan-worker --server http://127.0.0.1:7311 --repo <path to your repo>");
  // The token goes in the environment, not a flag, so it stays out of the process list.
  assert.match(waiting.command, /^ACAN_AGENT_TOKEN=/);
  assert.doesNotMatch(waiting.command, /--token\b/);
  assert.equal(waiting.command, workerCommand(ORIGIN));
  assert.deepEqual(waiting.action, { kind: "copy", label: "Copy command" });
  const connected = byId(steps({ token: true, view: view({ agents: 2 }) })).worker;
  assert.equal(connected.state, DONE);
  assert.equal(connected.detail, "2 workers connected.");
}));

test("the worker command passes the token in the environment, in the one form its platform's shell reads", () => {
  const token = "<agent token from .acan/tokens.json>";
  const flags = `--server ${ORIGIN} --repo <path to your repo>`;
  assert.equal(workerCommand(ORIGIN, false), `ACAN_AGENT_TOKEN=${token} acan-worker ${flags}`);
  assert.equal(workerCommand(ORIGIN, true), `$env:ACAN_AGENT_TOKEN="${token}"; acan-worker ${flags}`);
  for (const windows of [false, true]) {
    const command = workerCommand(ORIGIN, windows);
    assert.equal(command.split("acan-worker").length, 2, "one command, not two");
    assert.doesNotMatch(command, /--token\b/);
  }
});

test("a Windows browser is told by its client hints, its platform or its user agent", () => {
  assert.equal(isWindows({ userAgentData: { platform: "Windows" } }), true);
  assert.equal(isWindows({ platform: "Win32" }), true);
  assert.equal(isWindows({ userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)" }), true);
  assert.equal(isWindows({ userAgentData: { platform: "macOS" }, platform: "MacIntel" }), false);
  assert.equal(isWindows({ userAgent: "Mozilla/5.0 (X11; Linux x86_64)" }), false);
  assert.equal(isWindows({ platform: "Linux x86_64", userAgent: "Mozilla/5.0 (Windows NT 10.0)" }), false, "the platform wins over the user agent");
  assert.equal(isWindows(null), false);
});

test("the checklist's worker step shows the PowerShell form in a Windows browser", onPlatform(WINDOWS, () => {
  const worker = byId(steps({ token: true, setup: KEYED, contract: CONTRACT, workers: { status: 404, body: null }, view: view(), origin: ORIGIN })).worker;
  assert.equal(worker.command, workerCommand(ORIGIN, true));
  assert.match(worker.command, /^\$env:ACAN_AGENT_TOKEN=/);
}));

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

test("the folded checklist reads Setup N/7", () => {
  assert.equal(progressShort(steps()), "0/7");
  const list = steps({ token: true, setup: KEYED, contract: CONTRACT, view: view({ agents: 1, orders: 1, lanes: [{ state: "confirmed" }] }), watched: true });
  assert.equal(progressShort(list), "7/7");
});

test("the checklist hides by itself once all seven steps are done, with no dismiss", () => {
  const done = { token: true, setup: KEYED, contract: CONTRACT, view: view({ agents: 1, orders: 1, lanes: [{ state: "confirmed" }] }) };
  const live = steps({ ...done, live: true });
  assert.equal(byId(live).watch.state, DONE, "a sealed lane on the live map is the watch surface");
  assert.equal(allDone(live), true);
  assert.equal(allDone(steps({ ...done, setup: { status: 404, body: null }, live: true })), true, "a step not offered counts as done");
  const panel = readFileSync(new URL("../../www/firstrun-panel.js", import.meta.url), "utf8");
  assert.match(panel, /if \(allDone\(list\)\) \{\s*remember\(\);[\s\S]*?panel\.hidden = true;\s*return;/, "hidden for good, so a reload keeps it gone");
});

test("the checklist still shows while any step is open", () => {
  const done = { token: true, setup: KEYED, contract: CONTRACT, view: view({ agents: 1, orders: 1, lanes: [{ state: "confirmed" }] }) };
  const held = steps({ ...done, live: false });
  assert.equal(byId(held).watch.state, TODO, "a rewound or replayed map is not watching");
  assert.equal(allDone(held), false);
  assert.equal(allDone(steps({ ...done, view: view({ agents: 1, orders: 1, lanes: [{ state: "unconfirmed" }] }), live: true })), false, "no lane sealed");
  assert.equal(allDone(steps({ ...done, contract: null, live: true })), false, "a contract the server cannot report");
  assert.equal(allDone(steps({ ...done, token: false, live: true })), false, "signed out");
});

test("a server that sends no contract flag does not hold the checklist open", () => {
  const rest = { token: true, setup: KEYED, view: view({ agents: 1, orders: 1, lanes: [{ state: "confirmed" }] }), live: true };
  const absent = steps({ ...rest, contract: contractOf({ machines: [] }) });
  assert.equal(byId(absent).contract.state, UNAVAILABLE);
  assert.equal(byId(absent).contract.action, null);
  assert.equal(progressShort(absent), "7/7");
  assert.equal(allDone(absent), true, "the line can hide");
  const missing = steps({ ...rest, contract: contractOf({ machines: [], contract: { present: false, version: null } }) });
  assert.equal(byId(missing).contract.state, TODO);
  assert.equal(allDone(missing), false);
  const present = steps({ ...rest, contract: contractOf({ contract: { present: true, version: "acan-app/1" } }) });
  assert.equal(byId(present).contract.state, DONE);
  assert.equal(byId(present).contract.detail, "Your repository has an app contract (acan-app/1).");
  assert.equal(allDone(present), true);
  const failed = steps({ ...rest, contract: null });
  assert.equal(byId(failed).contract.state, UNKNOWN);
  assert.equal(allDone(failed), false, "a failed read keeps the line open");
  const panel = readFileSync(new URL("../../www/firstrun-panel.js", import.meta.url), "utf8");
  assert.match(panel, /contract = res\.ok \? contractOf\(await res\.json\(\)\) : null;/, "the panel reads GET /api/plant's body; a refused or failed read is null");
});
