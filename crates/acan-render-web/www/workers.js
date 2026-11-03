// The page's one read of GET /api/workers, shared by the lanes strip and the first-run checklist,
// so the two never show different workers. The route is read with its own fetch: a server without
// it answers 404, and a refused read (401, 403) must not sign the owner out, so stream.js's `get`
// is not used. What the answer means is decided by `workerSummary` in firstrun.js.
import { workersState } from "./firstrun.js";
import { state } from "./state.js";
import { apiPath, target } from "./projects.js";

// While the workers wait (no key, or none running), they are read again at most this often.
const EVERY_MS = 5000;

// The last answer, `{ status, body }`, or null before one.
let answer = null;
let askedAt = 0;
let inFlight = false;
const listeners = [];

export function workersAnswer() {
  return answer;
}

// Runs `listener` after each answer.
export function onWorkers(listener) {
  listeners.push(listener);
}

export async function loadWorkers() {
  if (!state.token || inFlight) return;
  inFlight = true;
  askedAt = Date.now();
  const project = target();
  try {
    const res = await fetch(apiPath(project, "/api/workers"), { headers: { Authorization: `Bearer ${state.token}` } });
    answer = { status: res.status, body: res.ok ? await res.json().catch(() => null) : null, project };
  } catch {
    answer = { status: 0, body: null, project };
  }
  inFlight = false;
  // Another project went on screen while this read was out, so its own workers are read now.
  if (project !== target()) {
    loadWorkers();
    return;
  }
  for (const listener of listeners) listener();
}

// The workers change with no event the view carries, such as a key arriving, so a waiting answer
// is read again now and then as the view moves. A running answer, or a server without the route,
// is not asked again.
export function refreshWorkers() {
  if (!state.token) return;
  // Each project has its own workers, so a project going on screen reads its own at once.
  if (answer === null || answer.project !== target()) {
    loadWorkers();
    return;
  }
  if (answer.status !== 200 || workersState(answer).kind === "running") return;
  if (Date.now() - askedAt >= EVERY_MS) loadWorkers();
}
