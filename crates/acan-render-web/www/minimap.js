// The projects map in the bottom right corner of a canvas of several projects: each plant's box,
// north-up, the ground the screen shows laid over them, and a pan to where it is clicked. A click
// on a plant flies there; a click or a drag elsewhere pans the camera, and the plant nearest the
// new centre goes on screen. Tab belongs to the rides, so G then M is the way in from the keys.
// The geometry is in minimap-model.js; this file binds it to `#minimap`.
import { reducedNow, requestRender } from "./main.js";
import { goKeys } from "./lanes.js";
import {
  CLICK_PX,
  MINIMAP_KEY,
  collapsedFrom,
  fitWorld,
  focusOrder,
  footprints,
  insideQuad,
  keepFit,
  labelOf,
  nearestId,
  offMap,
  pointsAttr,
  readBoxes,
  rovingNext,
  sameFloats,
  shouldShow,
  toWorld,
  union,
  upTick,
  viewShape,
} from "./minimap-model.js";
import { flyProject, nameOf, selectProject } from "./multi.js";
import { canvasState } from "./projects.js";
import { hold } from "./stream.js";
import { state } from "./state.js";

// A pan asked during a ride waits for the ride to ease back, as `flyProject` does.
const PAN_RETRY_MS = 100;
const PAN_TRIES = 12;

// The gap kept between the map and the right rail above it, in CSS px.
const ROOM_GAP_PX = 8;

const TICK_PX = 4;

// The chevron for a view off the map sits this far inside the border.
const OFF_INSET_PX = 6;

const mm = {
  root: null,
  toggle: null,
  nameEl: null,
  map: null,
  svg: null,
  parts: null,
  // Off after the first error, so a broken read never throws each frame.
  broken: false,
  collapsed: false,
  floats: null,
  order: [],
  entries: [],
  t: null,
  size: [0, 0],
  view: null,
  // Each plant's button by id, and their order for the arrow keys.
  buttons: new Map(),
  roving: [],
  words: "",
  room: -1,
  // The plant under the pointer or the focus, which the head names.
  pointed: null,
  press: null,
  // Keys taken on keydown, whose keyup is taken too.
  taken: new Set(),
};

// Reads the canvas each frame and redraws what moved. Called from main.js after the plates.
export function placeMinimap() {
  if (mm.broken || state.app === null) return;
  try {
    if (mm.root === null && !bind()) return;
    place();
  } catch (err) {
    mm.broken = true;
    if (mm.root !== null) mm.root.hidden = true;
    syncRoom();
    console.error("the projects map is off", err);
  }
}

function bind() {
  const root = document.getElementById("minimap");
  if (root === null) {
    mm.broken = true;
    return false;
  }
  mm.root = root;
  mm.toggle = document.getElementById("minimap-toggle");
  mm.nameEl = document.getElementById("minimap-name");
  mm.map = document.getElementById("minimap-map");
  mm.svg = document.getElementById("minimap-view");
  mm.parts = Object.fromEntries(["fill", "edge", "top", "tick", "off"].map((name) => [name, mm.svg.querySelector(`.${name}`)]));
  try {
    mm.collapsed = collapsedFrom(localStorage.getItem(MINIMAP_KEY));
  } catch {
    mm.collapsed = false;
  }
  applyCollapsed();
  // No focus moves and no text is selected by a press on the map.
  root.addEventListener("mousedown", (event) => event.preventDefault());
  mm.toggle.addEventListener("click", toggleCollapsed);
  root.addEventListener("keydown", onKeyDown);
  root.addEventListener("keyup", onKeyUp);
  root.addEventListener("focusin", (event) => point(plantOf(event.target)));
  root.addEventListener("focusout", () => point(null));
  mm.map.addEventListener("pointerover", (event) => point(plantOf(event.target)));
  mm.map.addEventListener("pointerleave", () => point(null));
  mm.map.addEventListener("pointerdown", onPointerDown);
  mm.map.addEventListener("pointermove", onPointerMove);
  mm.map.addEventListener("pointerup", (event) => endPress(event, true));
  mm.map.addEventListener("pointercancel", (event) => endPress(event, false));
  // A click with no pointer behind it, such as a screen reader's, flies to the plant.
  mm.map.addEventListener("click", (event) => {
    const id = plantOf(event.target);
    if (event.detail === 0 && id !== null) flyProject(id);
  });
  goKeys.set("m", focusEntry);
  return true;
}

function place() {
  const floats = state.app.canvas_boxes();
  const boxesMoved = !sameFloats(floats, mm.floats);
  if (boxesMoved) {
    // The order only grows, as projects are added, so it is read again when the count changes.
    if (floats.length !== mm.order.length * 5) mm.order = JSON.parse(state.app.canvas_order());
    mm.floats = Array.from(floats);
    mm.entries = readBoxes(mm.floats, mm.order);
  }
  const show = shouldShow(mm.entries);
  if (mm.root.hidden === show) {
    mm.root.hidden = !show;
    // A map that shows again draws in full.
    mm.t = null;
  }
  syncRoom();
  if (!show || mm.collapsed) return;
  let moved = boxesMoved || mm.t === null;
  if (moved) {
    mm.size = [mm.map.clientWidth, mm.map.clientHeight];
    const box = union(mm.entries);
    if (!keepFit(mm.t, box, mm.size)) mm.t = fitWorld(box, mm.size);
    syncPlants();
  }
  syncWords();
  const view = state.app.minimap_view();
  if (moved || !sameFloats(view, mm.view)) {
    mm.view = Array.from(view);
    paintView();
  }
}

// Makes, places and removes the plants' buttons to match the boxes.
function syncPlants() {
  const rects = footprints(mm.entries, mm.t);
  const seen = new Set();
  for (const rect of rects) {
    let plant = mm.buttons.get(rect.id);
    if (plant === undefined) {
      plant = makePlant(rect.id);
      mm.buttons.set(rect.id, plant);
      mm.map.insertBefore(plant.el, mm.svg);
    }
    seen.add(rect.id);
    const style = plant.el.style;
    const px = (v) => `${Math.round(v * 2) / 2}px`;
    style.left = px(rect.left);
    style.top = px(rect.top);
    style.width = px(rect.width);
    style.height = px(rect.height);
    plant.el.classList.toggle("filling", rect.look === "filling");
    plant.el.classList.toggle("unmapped", rect.look === "unmapped");
    plant.bar.style.width = `${Math.round(rect.built * 100)}%`;
  }
  for (const [id, plant] of mm.buttons) {
    if (seen.has(id)) continue;
    plant.el.remove();
    mm.buttons.delete(id);
  }
  mm.roving = focusOrder(rects);
  mm.words = "";
}

function makePlant(id) {
  const el = document.createElement("button");
  el.type = "button";
  el.className = "plant";
  el.tabIndex = -1;
  el.dataset.project = id;
  const bar = document.createElement("span");
  bar.className = "bar";
  bar.setAttribute("aria-hidden", "true");
  el.append(bar);
  return { el, bar };
}

// The buttons' labels, the plant on screen and the head's words, written only when they change.
function syncWords() {
  const active = canvasState.active;
  const words = mm.entries.map((entry) => {
    const degraded = canvasState.list.find((project) => project.id === entry.id)?.status === "degraded";
    return [entry.id, labelOf({ name: nameOf(entry.id), here: entry.id === active, built: entry.built, degraded })];
  });
  const named = words.some(([id]) => id === mm.pointed) ? mm.pointed : active;
  const signature = JSON.stringify([active, named, words]);
  if (signature === mm.words) return;
  mm.words = signature;
  for (const [id, label] of words) {
    const el = mm.buttons.get(id)?.el;
    if (el === undefined) continue;
    el.setAttribute("aria-label", label.aria);
    el.classList.toggle("here", id === active);
    if (id === active) el.setAttribute("aria-current", "true");
    else el.removeAttribute("aria-current");
  }
  mm.nameEl.textContent = words.find(([id]) => id === named)?.[1].text ?? "";
}

function put(el, name, value) {
  if (el.getAttribute(name) !== value) el.setAttribute(name, value);
}

function paintView() {
  const { fill, edge, top, tick, off } = mm.parts;
  const shape = viewShape(mm.view, mm.t);
  const drawn = shape === null ? "hidden" : "visible";
  for (const el of [fill, edge, top, tick]) put(el, "visibility", drawn);
  const away = offMap(shape, mm.size, OFF_INSET_PX);
  put(off, "visibility", away === null ? "hidden" : "visible");
  if (shape === null) return;
  const [tl, tr, br, bl] = shape.points;
  put(fill, "points", pointsAttr(shape.points));
  put(edge, "points", pointsAttr([tr, br, bl, tl]));
  const [a, b] = pointsAttr([tl, tr]).split(" ").map((pair) => pair.split(","));
  put(top, "x1", a[0]);
  put(top, "y1", a[1]);
  put(top, "x2", b[0]);
  put(top, "y2", b[1]);
  top.classList.toggle("cut", shape.cut);
  const { from, to } = upTick(shape.points, TICK_PX);
  put(tick, "d", `M ${pointsAttr([from]).replace(",", " ")} L ${pointsAttr([to]).replace(",", " ")}`);
  if (away !== null) {
    put(off, "d", "M -3 -4 L 1 0 L -3 4");
    put(off, "transform", `translate(${pointsAttr([[away.x, away.y]]).replace(",", " ")}) rotate(${Math.round(away.angle)})`);
  }
}

// The right rail ends this far above the map, so it never runs under it.
function syncRoom() {
  const height = mm.root === null || mm.root.hidden ? 0 : mm.root.offsetHeight;
  const room = height > 0 ? height + ROOM_GAP_PX : 0;
  if (room === mm.room) return;
  mm.room = room;
  document.documentElement.style.setProperty("--minimap-room", `${room}px`);
}

function applyCollapsed() {
  mm.toggle.setAttribute("aria-expanded", String(!mm.collapsed));
  mm.map.hidden = mm.collapsed;
  mm.root.classList.toggle("collapsed", mm.collapsed);
}

function toggleCollapsed() {
  mm.collapsed = !mm.collapsed;
  applyCollapsed();
  try {
    if (mm.collapsed) localStorage.setItem(MINIMAP_KEY, "collapsed");
    else localStorage.removeItem(MINIMAP_KEY);
  } catch {
    // A page without storage keeps the choice until it reloads.
  }
  // An opened map draws in full on the next frame.
  mm.t = null;
  mm.view = null;
  mm.words = "";
  syncRoom();
  requestRender();
}

function plantOf(target) {
  return target instanceof Element ? target.closest("button.plant")?.dataset.project ?? null : null;
}

function point(id) {
  if (mm.pointed === id) return;
  mm.pointed = id;
  requestRender();
}

// The point under the pointer, in the map's own px.
function mapPoint(event) {
  const rect = mm.map.getBoundingClientRect();
  return [event.clientX - rect.left - mm.map.clientLeft, event.clientY - rect.top - mm.map.clientTop];
}

function onPointerDown(event) {
  if (event.button !== 0 || mm.t === null) return;
  mm.map.setPointerCapture(event.pointerId);
  const at = mapPoint(event);
  // A press inside the view keeps the grab, so the view moves with the pointer; elsewhere the
  // view's centre jumps under it.
  const shape = viewShape(mm.view, mm.t);
  const grabbed = shape !== null && insideQuad(shape.points, at);
  const world = toWorld(mm.t, at);
  mm.press = {
    x: event.clientX,
    y: event.clientY,
    plant: plantOf(event.target),
    offset: grabbed ? [mm.view[9] - world[0], mm.view[10] - world[1]] : [0, 0],
    moved: false,
    to: null,
  };
  if (state.ridden !== "") {
    state.app.leave();
    requestRender();
  }
}

function onPointerMove(event) {
  const press = mm.press;
  if (press === null) return;
  if (!press.moved && Math.hypot(event.clientX - press.x, event.clientY - press.y) < CLICK_PX) return;
  press.moved = true;
  const world = toWorld(mm.t, mapPoint(event));
  press.to = [world[0] + press.offset[0], world[1] + press.offset[1]];
  state.app.pan_to(press.to[0], press.to[1], true);
  requestRender();
}

function endPress(event, up) {
  const press = mm.press;
  mm.press = null;
  if (press === null) return;
  if (mm.map.hasPointerCapture(event.pointerId)) mm.map.releasePointerCapture(event.pointerId);
  if (press.moved) {
    if (press.to !== null) settle(press.to);
  } else if (up && press.plant !== null) {
    flyProject(press.plant);
  } else if (up && mm.t !== null) {
    panTo(toWorld(mm.t, mapPoint(event)));
  }
}

// Eases the camera's centre to the world point, then puts the plant there on screen. A ride is
// left on the press, and the pan waits for it to ease back.
function panTo(to) {
  let tries = PAN_TRIES;
  const go = () => {
    if (state.app.pan_to(to[0], to[1], reducedNow())) {
      requestRender();
      settle(to);
      return;
    }
    requestRender();
    if (--tries > 0) setTimeout(go, PAN_RETRY_MS);
  };
  go();
}

// The settle bindSettle makes after a canvas move, made at once: its 400 ms timer would fire
// before the 600 ms ease ends.
function settle(to) {
  if (state.drag !== null || state.ridden !== "" || hold.on) return;
  const id = nearestId(mm.entries, to);
  if (id !== null && id !== canvasState.active) selectProject(id);
}

// G then M: the plant on screen, or the toggle when the map is folded.
function focusEntry() {
  if (mm.root === null || mm.root.hidden || mm.root.offsetHeight === 0) return;
  const target = mm.collapsed
    ? mm.toggle
    : mm.buttons.get(canvasState.active)?.el ?? mm.buttons.get(mm.roving[0])?.el ?? mm.toggle;
  target.focus();
}

// The toggle, then the plants in place order.
function rovingSet() {
  if (mm.collapsed) return [mm.toggle];
  return [mm.toggle, ...mm.roving.map((id) => mm.buttons.get(id)?.el).filter((el) => el !== undefined)];
}

// The window's key handlers would pan the camera on an arrow and settle on its keyup, so a key
// the map takes stops here, down and up.
function take(event) {
  event.preventDefault();
  event.stopPropagation();
  mm.taken.add(event.key);
}

function onKeyDown(event) {
  if (event.ctrlKey || event.metaKey || event.altKey) return;
  const items = rovingSet();
  const next = rovingNext(items.length, items.indexOf(document.activeElement), event.key);
  if (next !== null) {
    take(event);
    items[next].focus();
    return;
  }
  if (event.key !== "Enter" && event.key !== " ") return;
  if (event.target === mm.toggle) {
    take(event);
    if (!event.repeat) toggleCollapsed();
    return;
  }
  const id = plantOf(event.target);
  if (id === null) return;
  take(event);
  if (!event.repeat) flyProject(id);
}

function onKeyUp(event) {
  if (!mm.taken.delete(event.key)) return;
  event.preventDefault();
  event.stopPropagation();
}
