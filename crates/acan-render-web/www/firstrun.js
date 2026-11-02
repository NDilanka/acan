// The first-run checklist, with no DOM, so Node can test it. It walks a new tester from sign-in to
// watching the map: each step's state comes from the token, the GET /api/setup and GET /api/workers
// answers, the plant, and the view the page already folds. This file decides what each step says; firstrun-panel.js
// shows it and wires each step to the control that does it.

// localStorage key: set once the tester dismisses the checklist, so it does not come back.
export const FIRSTRUN_KEY = "acan.firstrun";

// The step states. `unknown` is a step this page cannot check yet, and `unavailable` one this
// server does not offer, so it does not hold the tester up.
export const DONE = "done";
export const TODO = "todo";
export const UNKNOWN = "unknown";
export const UNAVAILABLE = "unavailable";

// The model key step, from the GET /api/setup answer `{ status, body }`. A server without the
// route answers 404, and its keys come from its .env, so the step is not offered there. `null`
// is no answer yet: no token, or not asked.
export function setupState(answer) {
  if (!answer) return UNKNOWN;
  if (answer.status === 404) return UNAVAILABLE;
  if (answer.status !== 200) return UNKNOWN;
  return answer.body?.configured === true ? DONE : TODO;
}

// Whether `acan init` ships in the build testers run. Until it does, the contract step says the
// command is coming.
export const INIT_SHIPPED = false;

// The app contract (.acan/app.json) as GET /api/plant reports it: `contract: { present, version }`.
// A plant without that field, from a server that does not report it yet, gives null.
export function contractOf(plant) {
  const contract = plant?.contract;
  if (!contract || typeof contract !== "object" || typeof contract.present !== "boolean") return null;
  return { present: contract.present, version: typeof contract.version === "string" ? contract.version : null };
}

export function contractState(plant) {
  const contract = contractOf(plant);
  if (contract === null) return UNKNOWN;
  return contract.present ? DONE : TODO;
}

// What GET /api/workers says about the workers `acan serve --workers N` runs, from its answer
// `{ status, body }`: `running` with how many run, `no_key` when every one waits for a key, `none`
// when the server runs none, or `outside` when the server has no such route (or no answer), so
// workers connect from outside and the view's agents are the signal.
export function workersState(answer) {
  const list = answer?.status === 200 && Array.isArray(answer.body?.workers) ? answer.body.workers : null;
  if (list === null) return { kind: "outside", running: 0 };
  if (list.length === 0) return { kind: "none", running: 0 };
  const running = list.filter((worker) => worker?.state === "running").length;
  if (running > 0) return { kind: "running", running };
  return { kind: list.every((worker) => worker?.state === "no_key") ? "no_key" : "none", running: 0 };
}

// The command that starts one outside worker against this origin. Its agent token comes from
// ACAN_AGENT_TOKEN, so it never shows in the process list.
export function workerCommand(origin) {
  return `acan-worker --server ${origin} --repo <path to your repo>`;
}

function workerStep(workers, agents, origin) {
  const step = { id: "worker", title: "Connect a worker" };
  const state = workersState(workers);
  if (state.kind === "running") {
    return { ...step, state: DONE, detail: `${plural(state.running, "local worker")} running.`, action: null };
  }
  if (state.kind === "no_key") {
    return { ...step, state: TODO, detail: "Workers are waiting for a model key.", action: { kind: "setup", label: "Set a key" } };
  }
  if (state.kind === "none") {
    return { ...step, state: TODO, detail: "Start `acan serve` with `--workers 1`, so it runs a worker for you.", action: null };
  }
  if (agents > 0) return { ...step, state: DONE, detail: `${plural(agents, "worker")} connected.`, action: null };
  return {
    ...step,
    state: TODO,
    detail: "Set `ACAN_AGENT_TOKEN` to an agent token from .acan/tokens.json, then start a worker. The demo's `pwsh demo/run.ps1` starts its own server and workers instead.",
    command: workerCommand(origin),
    action: { kind: "copy", label: "Copy command" },
  };
}

function plural(count, word) {
  return `${count} ${word}${count === 1 ? "" : "s"}`;
}

function keyStep(token, setup) {
  const state = token ? setupState(setup) : UNKNOWN;
  const step = { id: "key", title: "Set a model key", state, action: null };
  if (state === DONE) return { ...step, detail: "Model calls go to your provider." };
  if (state === UNAVAILABLE) return { ...step, detail: "This acan has no key setup, so it reads keys from its .env file." };
  if (!token) return { ...step, detail: "Sign in first, and this step checks your key." };
  if (state === UNKNOWN) return { ...step, detail: "The key setup could not be read. Check that acan is running." };
  if (setup.body?.can_save === false) {
    return { ...step, detail: "This repository's .acan/llm.toml chooses the providers, so set their keys in its .env file." };
  }
  return { ...step, detail: "Add a provider key, so model jobs stop getting built-in replies.", action: { kind: "setup", label: "Set a key" } };
}

// `contract` is undefined before the page has asked, null when the server did not say, or
// `{ present, version }`.
function contractStep(token, contract) {
  const state = contract === undefined || contract === null ? UNKNOWN : (contract.present ? DONE : TODO);
  const step = { id: "contract", title: "Add the app contract", state };
  if (state === DONE) {
    const version = contract.version ? ` (${contract.version})` : "";
    return { ...step, detail: `Your repository has an app contract${version}.`, action: null };
  }
  const run = `Run \`acan init\` in your repo${INIT_SHIPPED ? "" : " (coming in this alpha)"}, so the gate has an app contract to check against.`;
  const detail = contract === null ? `${run} This acan does not report the contract yet, so check for .acan/app.json yourself.` : run;
  return { ...step, detail, action: token ? { kind: "contract", label: "Check again" } : null };
}

// Each step, in order, with its state, its words and the control that does it. `current` marks the
// first step still to do. A step this page cannot check is never current, so a server that cannot
// say yet does not hold the tester at it.
export function steps({ token = false, setup = null, contract, workers = null, view = null, watched = false, origin = "" } = {}) {
  const agents = view?.agents?.length ?? 0;
  const orders = view?.orders?.length ?? 0;
  const sealed = (view?.lanes ?? []).some((lane) => lane.state === "confirmed");
  const list = [
    token
      ? { id: "signin", title: "Sign in", state: DONE, detail: "Signed in with the owner token.", action: null }
      : { id: "signin", title: "Sign in", state: TODO, detail: "Paste the owner token that `acan serve` printed.", action: { kind: "focus", target: "token", label: "Sign in" } },
    keyStep(token, setup),
    contractStep(token, contract),
    workerStep(workers, agents, origin),
    orders > 0
      ? { id: "order", title: "Send a first order", state: DONE, detail: `${plural(orders, "order")} sent.`, action: null }
      : { id: "order", title: "Send a first order", state: TODO, detail: "Click a machine and write one sentence about what should change there.", action: { kind: "focus", target: "compose", label: "Write an order" } },
    sealed
      ? { id: "seal", title: "Seal a lane", state: DONE, detail: "A scope is sealed, so workers may build inside it.", action: null }
      : { id: "seal", title: "Seal a lane", state: TODO, detail: "Read the order's scope on the slip, then press Seal to let workers build.", action: { kind: "focus", target: "slip", label: "Open the slip" } },
    watched
      ? { id: "watch", title: "Watch the map", state: DONE, detail: "You are watching.", action: null }
      : { id: "watch", title: "Watch the map", state: TODO, detail: "Agents work on the map from here, and a pin shows when something needs you.", action: { kind: "dismiss", label: "Done" } },
  ];
  const first = list.findIndex((step) => step.state === TODO);
  return list.map((step, index) => ({ ...step, current: index === first }));
}

// The closed line: how many steps are done. A step this server does not offer counts as done.
export function progressText(list) {
  const done = list.filter((step) => step.state === DONE || step.state === UNAVAILABLE).length;
  return `${done} of ${list.length} done`;
}

// Whether the checklist shows: never on a recorded replay, which has no server, and never once
// dismissed.
export function shouldShow({ replay = false, dismissed = false } = {}) {
  return !replay && !dismissed;
}
