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

// What contractOf gives for a plant with no `contract` key at all: a server that does not report
// the contract, so the step is not offered there.
export const CONTRACT_ABSENT = "absent";

// The app contract (.acan/app.json) as GET /api/plant reports it: `contract: { present, version }`,
// sent on every plant answer by a server that reports it (version null when absent). A plant with
// no such key gives CONTRACT_ABSENT; a key with any other value gives null, as a failed read does.
export function contractOf(plant) {
  if (plant !== null && typeof plant === "object" && !("contract" in plant)) return CONTRACT_ABSENT;
  const contract = plant?.contract;
  if (!contract || typeof contract !== "object" || typeof contract.present !== "boolean") return null;
  return { present: contract.present, version: typeof contract.version === "string" ? contract.version : null };
}

export function contractState(plant) {
  const contract = contractOf(plant);
  if (contract === CONTRACT_ABSENT) return UNAVAILABLE;
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

// The command that starts one outside worker against this origin. Only a server without
// GET /api/workers shows it. The token goes in ACAN_AGENT_TOKEN, not a flag, so it stays out of
// the process list.
export function workerCommand(origin) {
  return `ACAN_AGENT_TOKEN=<agent token from .acan/tokens.json> acan-worker --server ${origin} --repo <path to your repo>`;
}

// The workers in one summary, read by the worker step here and by the lanes strip's status and
// empty state, so the two never disagree. `answer` is the GET /api/workers answer, `agents` the
// view's agent count. `kind` is `running`, `no_key`, `none`, `connected` (outside workers the
// view shows, on a server without the route) or `outside` (none of those yet). `status` is the
// short words for a strip's head, and `setKey` asks for the Set a key control.
export function workerSummary(answer, agents, origin) {
  const state = workersState(answer);
  if (state.kind === "running") {
    const words = `${plural(state.running, "local worker")} running`;
    return { kind: "running", done: true, status: words, detail: `${words}.`, setKey: false };
  }
  if (state.kind === "no_key") {
    return { kind: "no_key", done: false, status: "waiting for a model key", detail: "Workers are waiting for a model key.", setKey: true };
  }
  if (state.kind === "none") {
    return { kind: "none", done: false, status: "no local worker", detail: "Start `acan serve` with `--workers 1`, so it runs a worker for you.", setKey: false };
  }
  if (agents > 0) {
    const words = `${plural(agents, "worker")} connected`;
    return { kind: "connected", done: true, status: words, detail: `${words}.`, setKey: false };
  }
  return {
    kind: "outside",
    done: false,
    status: "no worker connected",
    detail: "Start a worker with an agent token from .acan/tokens.json. The demo's `pwsh demo/run.ps1` starts its own server and workers instead.",
    command: workerCommand(origin),
    setKey: false,
  };
}

function workerStep(workers, agents, origin) {
  const summary = workerSummary(workers, agents, origin);
  const step = { id: "worker", title: "Connect a worker", state: summary.done ? DONE : TODO, detail: summary.detail };
  if (summary.setKey) return { ...step, action: { kind: "setup", label: "Set a key" } };
  if (summary.command) return { ...step, command: summary.command, action: { kind: "copy", label: "Copy command" } };
  return { ...step, action: null };
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

// `contract` is undefined before the page has asked, null when the read failed, CONTRACT_ABSENT
// when the server does not report it, or `{ present, version }`.
function contractStep(token, contract) {
  if (contract === CONTRACT_ABSENT) {
    return { id: "contract", title: "Add the app contract", state: UNAVAILABLE, detail: "This acan does not report the app contract, so this step is not offered here.", action: null };
  }
  const state = contract === undefined || contract === null ? UNKNOWN : (contract.present ? DONE : TODO);
  const step = { id: "contract", title: "Add the app contract", state };
  if (state === DONE) {
    const version = contract.version ? ` (${contract.version})` : "";
    return { ...step, detail: `Your repository has an app contract${version}.`, action: null };
  }
  const run = `Run \`acan init\` in your repo${INIT_SHIPPED ? "" : " (coming in this alpha)"}, so the gate has an app contract to check against.`;
  const detail = contract === null ? `${run} The page could not read it, so check for .acan/app.json.` : run;
  return { ...step, detail, action: token ? { kind: "contract", label: "Check again" } : null };
}

// Each step, in order, with its state, its words and the control that does it. `current` marks the
// first step still to do. A step this page cannot check is never current, so a server that cannot
// say yet does not hold the tester at it. `live` is whether the page shows the live map: once a
// lane is sealed, the live map in front of the tester is the watch surface, so watching is done.
export function steps({ token = false, setup = null, contract, workers = null, view = null, watched = false, live = false, origin = "" } = {}) {
  const agents = view?.agents?.length ?? 0;
  const orders = view?.orders?.length ?? 0;
  const sealed = (view?.lanes ?? []).some((lane) => lane.state === "confirmed");
  watched = watched || (sealed && live);
  const list = [
    token
      ? { id: "signin", title: "Sign in", state: DONE, detail: "Signed in with the owner token.", action: null }
      : { id: "signin", title: "Sign in", state: TODO, detail: "Paste the owner token from the terminal, or from .acan/tokens.json.", action: { kind: "focus", target: "token", label: "Sign in" } },
    keyStep(token, setup),
    contractStep(token, contract),
    workerStep(workers, agents, origin),
    orders > 0
      ? { id: "order", title: "Send a first order", state: DONE, detail: `${plural(orders, "order")} sent.`, action: null }
      : { id: "order", title: "Send a first order", state: TODO, detail: "Click a machine and write one sentence about what should change there.", action: { kind: "focus", target: "compose", label: "Write an order" } },
    sealed
      ? { id: "seal", title: "Seal a lane", state: DONE, detail: "A scope is sealed, so workers may build inside it.", action: null }
      : { id: "seal", title: "Seal a lane", state: TODO, detail: "On the slip, adopt the drafted scope if it asks, read it, then press Seal to let workers build.", action: { kind: "focus", target: "slip", label: "Open the slip" } },
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

// Whether every step is done, a step this server does not offer counting as done. A step the page
// cannot check yet (unknown) keeps the checklist open.
export function allDone(list) {
  return list.every((step) => step.state === DONE || step.state === UNAVAILABLE);
}

// The folded checklist's count, such as "3/7".
export function progressShort(list) {
  const done = list.filter((step) => step.state === DONE || step.state === UNAVAILABLE).length;
  return `${done}/${list.length}`;
}

// Whether the checklist shows: never on a recorded replay, which has no server, and never once
// dismissed.
export function shouldShow({ replay = false, dismissed = false } = {}) {
  return !replay && !dismissed;
}
