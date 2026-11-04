// The canvas of several projects: one origin that serves N projects, each with its own routes,
// log, seq and plant. This module holds what the page decides about them. It has no imports and
// touches no DOM, so Node can test it. A page whose origin lists one project, or answers
// GET /api/projects with 404, never turns it on, and every path stays as it was.

// The gap between two packed plants, in plan units: about three machine columns.
export const PACK_GAP = 240;

// Where the plants were dragged to, by project id.
export const PLACES_KEY = "acan.places";

// The size a project takes on the canvas when its plant did not load, so its plate has a place.
export const PLACEHOLDER = [0, 0, 160, 120];

// `multi` turns on once the origin lists several projects. `active` is the project on screen,
// which orders, compose and every per-project fetch go to. `list` is GET /api/projects in its
// order, the order the plants are packed in.
export const canvasState = { multi: false, active: null, list: [] };

// The project the page's calls go to, or null on a page with one plant.
export function target() {
  return canvasState.multi ? canvasState.active : null;
}

// `path` under project `project`'s prefix, such as /api/p/shop/commands for /api/commands. Without
// a project the path is unchanged, so a one-project origin sees the routes it always did.
export function apiPath(project, path) {
  if (!project || !String(path).startsWith("/api/")) return path;
  return `/api/p/${encodeURIComponent(project)}/${path.slice("/api/".length)}`;
}

// The projects from GET /api/projects, or null when the page keeps one plant: any answer but a
// list of two or more. Only the fields the page shows are kept.
export function projectList(status, body) {
  if (status !== 200 || !Array.isArray(body)) return null;
  const list = body
    .filter((entry) => entry !== null && typeof entry === "object" && typeof entry.id === "string" && entry.id !== "")
    .map((entry) => ({
      id: entry.id,
      name: typeof entry.name === "string" && entry.name.trim() !== "" ? entry.name : entry.id,
      default: entry.default === true,
      status: entry.status === "degraded" ? "degraded" : "ready",
      head: Number.isInteger(entry.head?.seq) ? entry.head.seq : null,
    }));
  return list.length > 1 ? list : null;
}

// The place of each project's layout origin, by id: left to right in list order, one `gap` apart,
// tops in line. `entries` are `{ id, bounds }` with the bounds from the origin, or null for a
// plant that did not load. A place the user dragged to, in `saved`, is kept; the others pack as if
// nothing moved, so dragging one plant never shifts another.
export function pack(entries, gap = PACK_GAP, saved = {}) {
  const places = new Map();
  let cursor = 0;
  for (const { id, bounds } of entries) {
    const box = validBox(bounds) ? bounds : PLACEHOLDER;
    const kept = saved[id];
    const ok = Array.isArray(kept) && kept.length === 2 && kept.every(Number.isFinite);
    places.set(id, ok ? [kept[0], kept[1]] : [cursor - box[0], 0 - box[1]]);
    cursor += box[2] - box[0] + gap;
  }
  return places;
}

// wasm hands bounds over as a Float32Array, which Array.isArray rejects.
function validBox(box) {
  return (Array.isArray(box) || ArrayBuffer.isView(box)) && box.length === 4 && box.every(Number.isFinite) && box[2] >= box[0] && box[3] >= box[1];
}

// A box from the origin moved to `place`.
export function worldBounds(bounds, place) {
  const box = validBox(bounds) ? bounds : PLACEHOLDER;
  return [box[0] + place[0], box[1] + place[1], box[2] + place[0], box[3] + place[1]];
}

// The place of a project added while the page is open: its plant's left edge `gap` past
// `rightEdge`, the right edge of the plants already on the canvas, with its top on the same line
// as `pack` puts one. Nothing else moves. `bounds` are the new plant's bounds from its origin.
export function placeAfter(rightEdge, bounds, gap = PACK_GAP) {
  const box = validBox(bounds) ? bounds : PLACEHOLDER;
  return [rightEdge + gap - box[0], 0 - box[1]];
}

// The saved places from local storage, or none for a missing or broken entry.
export function placesFrom(text) {
  try {
    const parsed = JSON.parse(text ?? "{}");
    return parsed !== null && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : {};
  } catch {
    return {};
  }
}

// The multiplexed stream's subscription: the next seq of every project, by id.
export function subscription(ids, nextSeq) {
  return { from: Object.fromEntries(ids.map((id) => [id, nextSeq(id)])) };
}

// One frame of GET /api/stream/all: an event of one project, the origin asking the page to resume
// one project from a seq (the socket closes next), or something else to pass over.
export function readFrame(text) {
  let frame;
  try {
    frame = JSON.parse(text);
  } catch {
    return { kind: "skip" };
  }
  if (frame?.control === "resume" && typeof frame.project === "string" && Number.isInteger(frame.from)) {
    return { kind: "resume", project: frame.project, from: frame.from };
  }
  if (typeof frame?.project === "string" && frame.event !== null && typeof frame.event === "object"
    && Number.isInteger(frame.event.seq)) {
    return { kind: "event", project: frame.project, event: frame.event, json: JSON.stringify(frame.event) };
  }
  return { kind: "skip" };
}

// What an event at `seq` of `project` does to the cursors, which hold each project's next seq:
// "apply" the next one, "skip" one already folded, or "gap" when events were missed and the
// project's log must load again. Seqs are per project, so one project's cursor never moves another.
export function stepCursor(cursors, project, seq) {
  const next = cursors.get(project) ?? 0;
  if (seq < next) return "skip";
  return seq === next ? "apply" : "gap";
}

// A resume frame asks for the project from `from`. Past the page's cursor some events were never
// sent, so the project reloads; otherwise the reconnect's cursor map resumes it.
export function resumeStep(cursors, frame) {
  return frame.from > (cursors.get(frame.project) ?? 0) ? "reload" : "resume";
}

// What a closed multiplexed socket means: 1008 is a token that is a member of no project, so the
// page asks for a token again; 4001 follows a resume frame and reconnects at once; anything else,
// such as 1001 at shutdown, reconnects after a pause.
export function closeAction(code) {
  if (code === 1008) return "sign-in";
  if (code === 4001) return "resume";
  return "retry";
}

// Ids repeat across projects, so a merged entity is named by its project too.
export function entityKey(project, key) {
  return `${project}/${key}`;
}

// The project orders and compose go to after the camera settles: the one under the screen centre
// when it is on the canvas with its log, else the one on screen.
export function chooseTarget({ active, under, ready }) {
  return under && under !== active && ready(under) ? under : active;
}

// The count of merged items of each project.
export function countsByProject(items) {
  const counts = new Map();
  for (const item of items) counts.set(item.project, (counts.get(item.project) ?? 0) + 1);
  return counts;
}

// The page state that belongs to the project on screen. Orders, tasks and settles repeat their ids
// across projects, so each project keeps its own copy, and a switch swaps them.
export const STASHED = [
  "plantViews", "sealViews", "sealSent", "sealPicks", "drafts", "postedOrder",
  "hasSlip", "strikeArmed", "slipPending", "slipNote", "serverUsage",
];

export function freshStash() {
  return {
    plantViews: new Map(),
    sealViews: new Map(),
    sealSent: new Map(),
    sealPicks: { hash: null, choices: new Map(), acks: new Set() },
    drafts: new Map(),
    postedOrder: null,
    hasSlip: false,
    strikeArmed: null,
    slipPending: false,
    slipNote: "",
    serverUsage: null,
  };
}

// Keeps `from`'s share of `state` in `stash` and gives `state` `to`'s, or a fresh one.
export function swapState(state, stash, from, to) {
  const kept = {};
  for (const name of STASHED) kept[name] = state[name];
  stash.set(from, kept);
  Object.assign(state, stash.get(to) ?? freshStash());
  stash.delete(to);
}
