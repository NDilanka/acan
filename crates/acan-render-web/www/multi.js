// The page over an origin that serves several projects. Each project's plant is fetched from its
// own routes, packed on one canvas and kept where the user drags it; every project's log is
// folded, and one WebSocket (GET /api/stream/all) carries all their events. The project on screen
// is the one whose marks and panels show; orders and every per-project call go to it.
import { requestRender } from "./main.js";
import { installAddPlant } from "./addplant.js";
import { isEmptyWorld, noteAttached } from "./emptystart.js";
import {
  PACK_GAP,
  PLACES_KEY,
  apiPath,
  canvasState,
  chooseTarget,
  closeAction,
  pack,
  placeAfter,
  placesFrom,
  projectList,
  readFrame,
  resumeStep,
  stepCursor,
  subscription,
  swapState,
  worldBounds,
} from "./projects.js";
import { addPlate, installPlates, paintPlates } from "./nameplates.js";
import { nearestGap, parkedClick, pinNear } from "./overlays.js";
import { installParked } from "./parked.js";
import { applyPlantScan, applyStreamed, feed, foldScanFrame, forgetToken, get, hold, setLink } from "./stream.js";
import { scanFrameOf } from "./scanview.js";
import { canvas, freeze, repoEl, state } from "./state.js";

const RECONNECT_MS = 1000;

// The camera is read this long after a pan, zoom or drag ends, to see which project is under it.
const SETTLE_MS = 400;

// The project list is read again this often, for each project's status.
const LIST_MS = 20000;

// A fly asked during a ride waits for the ride to ease back, checking this often, this many times.
const FLY_RETRY_MS = 100;
const FLY_TRIES = 12;

const CLICK_PX = 4;

// How near a parked plant's clash pin a click must land, as marks.js picks the pins on screen.
const PIN_PICK_PX = 18;

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

const multi = {
  kit: null,
  // Each project's bounds from the origin and its place, by id.
  origin: new Map(),
  places: new Map(),
  // The projects whose plant and log are on the canvas.
  loaded: new Set(),
  // The events of each project not on screen; the feed holds the one on screen.
  events: new Map(),
  // Each project's next seq, as the stream has folded it.
  cursors: new Map(),
  // Each project's share of the page state while another is on screen. See `swapState`.
  stash: new Map(),
  // Projects whose log loads again before the stream reconnects.
  reload: new Set(),
  // Moves when anything a parked project shows changes.
  version: 0,
  views: new Map(),
};

// GET /api/projects with the token, or null when the page keeps one plant: an origin without the
// route, one that lists a single project, or a refused or failed read. A refused token is left
// for the one-plant path to report, as it always did.
// An origin with no codebase answers `[]`, which is an empty world, not an error: it resolves to [].
export async function listProjects(token) {
  try {
    const res = await fetch("/api/projects", { headers: { Authorization: `Bearer ${token}` } });
    if (!res.ok) return null;
    const body = await res.json();
    if (isEmptyWorld(res.status, body)) return [];
    return projectList(res.status, body);
  } catch {
    return null;
  }
}

// What the watch panels read of the other projects. See `installInbox`.
export function installMulti(kit) {
  multi.kit = kit;
  kit.projects = {
    on: () => canvasState.multi,
    active: () => canvasState.active,
    version: () => multi.version,
    others,
    select: selectProject,
    fly: flyProject,
    counts: (counts) => paintPlates(counts),
  };
  installAddPlant({ added: attachProject });
}

// Puts a codebase added while the page is open on the canvas, to the right of the others, and flies
// the camera to it. Its plant and log are read first, so a failed read leaves the canvas as it was.
// The stream is opened again, so it carries the new codebase's events and its scan. Resolves to
// false when this page keeps one plant, which cannot show it beside another without a reload.
export async function attachProject(project) {
  if (!canvasState.multi) return state.empty ? attachFirst(project) : false;
  const [text, log] = await Promise.all([
    get(apiPath(project.id, "/api/plant")),
    get(apiPath(project.id, "/api/events?from=0")),
  ]);
  const origin = Array.from(state.app.plant_bounds(text));
  let rightEdge = 0;
  for (const known of canvasState.list) {
    const box = boundsOf(known.id);
    if (box !== null) rightEdge = Math.max(rightEdge, box[2]);
  }
  const place = placeAfter(rightEdge, origin);
  const known = { id: project.id, name: project.name, default: false, status: "mapping", head: null };
  canvasState.list.push(known);
  addPlate(known);
  paintPlates();
  multi.origin.set(project.id, origin);
  multi.places.set(project.id, place);
  state.app.add_project(project.id, text, place[0], place[1]);
  foldLog(project.id, log);
  multi.loaded.add(project.id);
  if (!freeze) {
    const old = state.socket;
    state.socket = null;
    old?.close();
    connectAll();
  }
  flyProject(project.id);
  applyPlantScan(text);
  requestRender();
  return true;
}

// The first codebase added to an empty origin. The page turns on as a canvas of one, through the same
// start as any canvas, so its plant, scan, plate and stream are the ones a canvas of several has.
// Resolves to false, and leaves the page empty, when its plant cannot be read.
async function attachFirst(project) {
  try {
    await startMulti([{ id: project.id, name: project.name, default: false, status: "mapping", head: null }]);
  } catch (err) {
    console.error(`the new codebase ${project.id} could not be put on the canvas`, err);
    canvasState.multi = false;
    canvasState.list = [];
    canvasState.active = null;
    return false;
  }
  state.empty = false;
  noteAttached();
  // The sign-in panels and the workers read the first project, which they could not before.
  window.dispatchEvent(new Event("acan:attached"));
  flyProject(project.id);
  requestRender();
  return true;
}

export function nameOf(id) {
  return canvasState.list.find((project) => project.id === id)?.name ?? id;
}

// The place the canvas gives project `id`, as `[x, y]`, or the origin for one it does not know, such
// as the one-plant page's null target.
export function placeOf(id) {
  return multi.places.get(id) ?? [0, 0];
}

// The world bounds of a project's plant, or null for one the canvas does not know.
export function boundsOf(id) {
  const place = multi.places.get(id);
  return place === undefined ? null : worldBounds(multi.origin.get(id), place);
}

// A zoom-out stops once the two nearest plants are this far apart on screen, in CSS px. A plate is
// about 80 px wide, so at about half that gap each plate still hangs beside its own plant.
const ZOOM_FLOOR_GAP_PX = 48;

// A project's plant on screen, `[left, top, right, bottom]` in CSS px, or null when unknown.
function screenBoxOf(id) {
  const box = boundsOf(id);
  if (box === null) return null;
  const corners = [[box[0], box[1]], [box[2], box[1]], [box[0], box[3]], [box[2], box[3]]]
    .map(([x, y]) => state.app.world_to_screen(x, y));
  const xs = corners.map((p) => p[0]);
  const ys = corners.map((p) => p[1]);
  return [Math.min(...xs), Math.min(...ys), Math.max(...xs), Math.max(...ys)];
}

// The least screen gap between two plants, in CSS px, or null when there is no pair to measure.
function gapOnScreen() {
  return nearestGap(canvasState.list.map((project) => screenBoxOf(project.id)).filter((box) => box !== null));
}

// Whether a zoom by `step` keeps the nearest plants the floor apart. The zoom is applied, measured
// and undone about the same point, so the camera is left as it was.
function keepsFloor(x, y, step) {
  state.app.zoom_at(x, y, step);
  const gap = gapOnScreen();
  state.app.zoom_at(x, y, 1 / step);
  return gap === null || Number.isNaN(gap) || gap >= ZOOM_FLOOR_GAP_PX;
}

// Zooms about the CSS point (x, y) by `factor`. On a canvas of several plants a zoom-out stops where
// the nearest two plants are ZOOM_FLOOR_GAP_PX apart on screen, so each plate keeps the room beside
// its plant. Zoom-ins and a single plant are unchanged; a canvas already closer than the floor does
// not zoom out at all. The zoom is not linear in the gap, so the largest step that keeps the floor is
// found by bisection.
export function zoomAround(x, y, factor) {
  if (!canvasState.multi || !(factor < 1)) return state.app.zoom_at(x, y, factor);
  const gap = gapOnScreen();
  if (gap === null || Number.isNaN(gap)) return state.app.zoom_at(x, y, factor);
  if (gap < ZOOM_FLOOR_GAP_PX) return undefined;
  if (keepsFloor(x, y, factor)) return state.app.zoom_at(x, y, factor);
  let lo = factor;
  let hi = 1;
  for (let i = 0; i < 6; i += 1) {
    const mid = (lo + hi) / 2;
    if (keepsFloor(x, y, mid)) hi = mid;
    else lo = mid;
  }
  return state.app.zoom_at(x, y, hi);
}

export async function startMulti(list) {
  canvasState.multi = true;
  canvasState.list = list;
  const plants = await Promise.allSettled(list.map((project) => get(apiPath(project.id, "/api/plant"))));
  const texts = new Map();
  list.forEach((project, index) => {
    const result = plants[index];
    if (result.status !== "fulfilled") {
      console.warn(`the plant of ${project.id} did not load`, result.reason);
      return;
    }
    try {
      multi.origin.set(project.id, Array.from(state.app.plant_bounds(result.value)));
      texts.set(project.id, result.value);
    } catch (err) {
      console.error(`the plant of ${project.id} cannot be read`, err);
    }
  });
  const saved = placesFrom(localStorage.getItem(PLACES_KEY));
  const entries = list.map((project) => ({ id: project.id, bounds: multi.origin.get(project.id) ?? null }));
  multi.places = pack(entries, PACK_GAP, saved);
  const first = list.find((project) => project.default && texts.has(project.id)) ?? list.find((project) => texts.has(project.id));
  if (first === undefined) throw new Error("no project's plant loaded");
  const order = [first, ...list.filter((project) => project !== first && texts.has(project.id))];
  canvasState.active = first.id;
  for (const project of order) {
    const [x, y] = multi.places.get(project.id);
    state.app.add_project(project.id, texts.get(project.id), x, y);
  }
  // Only the project on screen follows its scan; the others read their plants as they stand.
  applyPlantScan(texts.get(first.id));
  const logs = await Promise.allSettled(order.map((project) => get(apiPath(project.id, "/api/events?from=0"))));
  order.forEach((project, index) => {
    const result = logs[index];
    if (result.status !== "fulfilled") {
      console.warn(`the log of ${project.id} did not load`, result.reason);
      return;
    }
    try {
      foldLog(project.id, result.value);
      multi.loaded.add(project.id);
    } catch (err) {
      console.error(`the log of ${project.id} cannot be folded`, err);
    }
  });
  repoEl.textContent = nameOf(canvasState.active);
  installPlates({ boundsOf, select: selectProject, fly: flyProject, move: moveProject, moved: savePlaces });
  installParked({ select: selectProject, fly: flyProject });
  paintPlates();
  bindSettle();
  bindParkedClick();
  setInterval(refreshList, LIST_MS);
  requestRender();
  if (freeze) {
    setLink(`frozen · seq ${state.app.next_seq()}`);
  } else {
    connectAll();
  }
}

// Folds a project's whole log: the one on screen through the app's own load, which the feed
// follows, and any other one beside it.
function foldLog(id, text) {
  if (id === canvasState.active) {
    if (hold.on) {
      hold.replace(JSON.parse(text));
    } else {
      state.app.load_log(text);
    }
    multi.cursors.set(id, hold.on ? hold.next() : state.app.next_seq());
  } else {
    state.app.project_load_log(id, text);
    multi.events.set(id, JSON.parse(text));
    multi.cursors.set(id, state.app.project_next_seq(id));
    multi.version += 1;
  }
}

function connectAll() {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${scheme}://${location.host}/api/stream/all`);
  state.socket = ws;
  ws.onopen = () => {
    ws.send(JSON.stringify({ token: state.token }));
    ws.send(JSON.stringify(subscription(canvasState.list.map((project) => project.id), nextSeqOf)));
    setLink(`live · seq ${state.app.next_seq()}`);
  };
  ws.onmessage = (message) => onFrame(ws, message.data);
  ws.onclose = (event) => {
    if (state.socket !== ws) return;
    state.socket = null;
    if (!state.token) return;
    const action = closeAction(event.code);
    if (action === "sign-in") {
      void recheckRefusal();
      return;
    }
    setLink("reconnecting");
    setTimeout(resync, action === "resume" ? 0 : RECONNECT_MS);
  };
}

// A 1008 close also means the token frame came after the server's wait, which a busy page can do.
// The token is asked again on a plain read: only a refusal there signs the owner out. Otherwise the
// stream simply connects again.
async function recheckRefusal() {
  const res = await fetch("/api/projects", { headers: { Authorization: `Bearer ${state.token}` } }).catch(() => null);
  if (res === null || (res.status !== 401 && res.status !== 403)) {
    setLink("reconnecting");
    setTimeout(resync, RECONNECT_MS);
    return;
  }
  forgetToken();
}

// A project whose log is not on the canvas starts past its head, so its old events are not sent.
function nextSeqOf(id) {
  if (multi.loaded.has(id)) return multi.cursors.get(id) ?? 0;
  const head = canvasState.list.find((project) => project.id === id)?.head;
  return head === null || head === undefined ? 0 : head + 1;
}

function onFrame(ws, data) {
  // A scan or a touch of the project on screen is folded into its plant, never into its log.
  const scan = scanFrameOf(data);
  if (scan !== null) {
    if (scan.project === null || scan.project === canvasState.active) foldScanFrame(scan);
    return;
  }
  const frame = readFrame(data);
  if (frame.kind === "resume") {
    if (resumeStep(multi.cursors, frame) === "reload") multi.reload.add(frame.project);
    return;
  }
  if (frame.kind !== "event" || !multi.loaded.has(frame.project)) return;
  const step = stepCursor(multi.cursors, frame.project, frame.event.seq);
  if (step === "skip") return;
  if (step === "gap") {
    multi.reload.add(frame.project);
    ws.close();
    return;
  }
  try {
    if (frame.project === canvasState.active) {
      if (!applyStreamed(frame.json)) throw new Error("the event did not apply");
    } else {
      state.app.project_ingest(frame.project, frame.json);
      multi.events.get(frame.project)?.push(frame.event);
      multi.version += 1;
      multi.kit?.requestRender();
    }
    multi.cursors.set(frame.project, frame.event.seq + 1);
  } catch (err) {
    console.error(`the stream of ${frame.project} could not be applied; reloading its log`, err);
    multi.reload.add(frame.project);
    ws.close();
  }
}

async function resync() {
  if (!state.token) return;
  try {
    for (const id of [...multi.reload]) {
      foldLog(id, await get(apiPath(id, "/api/events?from=0")));
      multi.reload.delete(id);
    }
    requestRender();
    connectAll();
  } catch (err) {
    console.error(err);
    setLink("reconnecting");
    setTimeout(resync, RECONNECT_MS * 2);
  }
}

async function refreshList() {
  if (!state.token) return;
  const list = await listProjects(state.token);
  if (list === null) return;
  for (const fresh of list) {
    const known = canvasState.list.find((project) => project.id === fresh.id);
    if (known !== undefined) Object.assign(known, { name: fresh.name, status: fresh.status, head: fresh.head });
  }
  paintPlates();
}

// Each other project with its log folded, as the inbox merges them: its view, its plant views
// and its events.
function others() {
  const out = [];
  for (const project of canvasState.list) {
    const id = project.id;
    if (id === canvasState.active || !multi.loaded.has(id)) continue;
    let cached = multi.views.get(id);
    if (cached === undefined || cached.version !== multi.version) {
      const json = state.app.project_view_json(id);
      cached = { version: multi.version, view: json === "" ? null : JSON.parse(json) };
      multi.views.set(id, cached);
    }
    if (cached.view === null) continue;
    out.push({
      project: id,
      view: cached.view,
      plantViews: multi.stash.get(id)?.plantViews ?? new Map(),
      events: multi.events.get(id) ?? [],
    });
  }
  return out;
}

// Puts project `id` on screen without moving the camera. Its marks, panels and calls take over.
// Returns false for a project whose log is not on the canvas.
export function selectProject(id) {
  if (id === canvasState.active) return true;
  if (!multi.loaded.has(id)) return false;
  if (hold.on) multi.kit?.goLive?.();
  const from = canvasState.active;
  if (!state.app.activate(id)) return false;
  multi.events.set(from, feed.events.slice());
  feed.load(multi.events.get(id) ?? []);
  multi.events.delete(id);
  swapState(state, multi.stash, from, id);
  // The machine names are kept by index and made with their words, so the last project's names
  // would stand on this plant's machines of the same index.
  for (const el of state.labels.values()) el.remove();
  state.labels.clear();
  state.usageDirty = true;
  canvasState.active = id;
  repoEl.textContent = nameOf(id);
  multi.version += 1;
  multi.kit?.workers.refresh();
  paintPlates();
  requestRender();
  return true;
}

// Puts project `id` on screen and eases the camera to fit it. A ride eases back first.
export function flyProject(id) {
  selectProject(id);
  if (state.ridden !== "") state.app.leave();
  let tries = FLY_TRIES;
  const go = () => {
    if (state.app.fly_to(id, reducedMotion.matches) || --tries <= 0) {
      requestRender();
      return;
    }
    requestRender();
    setTimeout(go, FLY_RETRY_MS);
  };
  go();
}

function moveProject(id, dx, dy) {
  const place = multi.places.get(id);
  if (place === undefined) return;
  const next = [place[0] + dx, place[1] + dy];
  multi.places.set(id, next);
  state.app.move_project(id, next[0], next[1]);
  requestRender();
}

function savePlaces() {
  const saved = placesFrom(localStorage.getItem(PLACES_KEY));
  for (const [id, place] of multi.places) saved[id] = place;
  localStorage.setItem(PLACES_KEY, JSON.stringify(saved));
}

// A click on a parked plant's pin or drone puts that project on screen before the page's own
// click handler runs, which then acts on it as on any project on screen: a pin flies the camera to
// the plant and opens the clash card, a drone is ridden, and the ride flies the camera. This runs
// in the capture phase so it comes before that handler on the same pointerup.
function bindParkedClick() {
  let down = null;
  canvas.addEventListener("pointerdown", (event) => {
    down = event.button === 0 && !event.altKey ? { x: event.clientX, y: event.clientY } : null;
  }, { capture: true });
  canvas.addEventListener("pointerup", (event) => {
    const pressed = down;
    down = null;
    if (pressed === null || Math.hypot(event.clientX - pressed.x, event.clientY - pressed.y) >= CLICK_PX) return;
    const rect = canvas.getBoundingClientRect();
    const x = event.clientX - rect.left;
    const y = event.clientY - rect.top;
    const order = JSON.parse(state.app.canvas_order());
    const skip = order.indexOf(canvasState.active);
    const drone = state.app.all_drone_at(x, y);
    const hit = parkedClick(
      pinNear(state.app.all_pin_layout(), x, y, skip, PIN_PICK_PX),
      drone === "" ? null : JSON.parse(drone),
      order,
      canvasState.active,
    );
    if (hit === null || !multi.loaded.has(hit.id)) return;
    if (hit.kind === "pin") flyProject(hit.id);
    else selectProject(hit.id);
  }, { capture: true });
}

// After the user moves the camera, the project under the screen centre goes on screen.
function bindSettle() {
  let timer = null;
  let down = null;
  const settle = () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      if (state.drag !== null || state.ridden !== "" || hold.on) return;
      const under = state.app.project_under(canvas.clientWidth / 2, canvas.clientHeight / 2);
      const next = chooseTarget({ active: canvasState.active, under, ready: (id) => multi.loaded.has(id) });
      if (next !== canvasState.active) selectProject(next);
    }, SETTLE_MS);
  };
  canvas.addEventListener("pointerdown", (event) => {
    down = { x: event.clientX, y: event.clientY };
  });
  canvas.addEventListener("pointerup", (event) => {
    if (down !== null && Math.hypot(event.clientX - down.x, event.clientY - down.y) >= CLICK_PX) settle();
    down = null;
  });
  canvas.addEventListener("wheel", settle, { passive: true });
  window.addEventListener("keyup", (event) => {
    if (event.key.startsWith("Arrow") || event.key === "0") settle();
  });
}
