// The HTML marks of the projects not on screen, on a canvas of several projects: each parked
// plant's agent flags, clash badges, machine names and beacon rings, placed every frame from the
// project-tagged layouts. The project on screen keeps marks.js. A plant the wasm side counts as
// far lists no flags, badges or names, so they hide; its drones and pins stay on the canvas.
import { openCard } from "./cards.js";
import { HUD_SELECTORS } from "./marks.js";
import {
  BADGE_PX, agentFlagShown, badgeBoxes, boxAt, clearOf, foldColumn, inView, overlaps, standsBeside,
} from "./place.js";
import { flagGroups, parkedEntries, parkedKind, splitByProject } from "./overlays.js";
import { canvasState } from "./projects.js";
import { canvas, flagsEl, labelsEl, state } from "./state.js";

const PAD_PX = 4;
const LABEL_OFFSET_PX = 6;
const PILL_PX = 18;
const POINTER_PX = 5;
const POINTER_INSET_PX = 6;
const FLAG_ROW_GAP_PX = 4;
const BEACON_RING_PX = 1.5;

const parked = {
  // What `actions` the page gave: `select(id)` and `fly(id)` from multi.js.
  actions: null,
  version: -1,
  active: null,
  order: [],
  // Each parked project's marks by its index in `order`: `{ id, flags, flagEls, badgeEls }`.
  entries: new Map(),
  labels: new Map(),
  rings: [],
  flagsEl: null,
  labelsEl: null,
};

export function installParked(actions) {
  parked.actions = actions;
}

function layers() {
  if (parked.flagsEl === null) {
    parked.flagsEl = document.createElement("div");
    parked.flagsEl.id = "parked-flags";
    parked.flagsEl.setAttribute("aria-hidden", "true");
    Object.assign(parked.flagsEl.style, { position: "fixed", inset: "0", pointerEvents: "none", overflow: "hidden" });
    flagsEl.before(parked.flagsEl);
  }
  if (parked.labelsEl === null) {
    parked.labelsEl = document.createElement("div");
    parked.labelsEl.className = "parked";
    labelsEl.append(parked.labelsEl);
  }
}

// Rebuilds the parked marks when the wasm side says they changed or another project went on
// screen.
function sync() {
  const version = state.app.parked_version();
  if (version === parked.version && canvasState.active === parked.active && parked.altDown === state.altDown) return;
  parked.version = version;
  parked.active = canvasState.active;
  parked.altDown = state.altDown;
  parked.order = JSON.parse(state.app.canvas_order());
  parked.flagsEl.replaceChildren();
  parked.entries = new Map();
  parked.rings = [];
  for (const entry of parkedEntries(JSON.parse(state.app.all_marks_json()), canvasState.active)) {
    const flagEls = entry.flags.map((flag) => (parkedKind(flag.kind) ? flagElement(entry.id, flag) : null));
    const badgeEls = new Map();
    for (const badges of entry.badges) {
      const pair = [badgeElement(entry.id, badges.a, badges.clash), badgeElement(entry.id, badges.b, badges.clash)];
      parked.flagsEl.append(...pair);
      badgeEls.set(badges.clash, pair);
    }
    for (const el of flagEls) if (el !== null) parked.flagsEl.append(el);
    parked.entries.set(entry.project, { id: entry.id, flags: entry.flags, flagEls, badgeEls });
  }
}

function flagElement(id, flag) {
  const el = document.createElement("div");
  el.className = `flag ${flag.kind}`;
  el.dataset.project = id;
  if (flag.kind === "agent") {
    Object.assign(el.style, {
      background: flag.hue,
      color: "#ffffff",
      border: "none",
      borderRadius: `${PILL_PX / 2}px`,
      height: `${PILL_PX}px`,
      padding: "0 8px",
      boxSizing: "border-box",
      font: `600 11px/${PILL_PX}px var(--sans)`,
      pointerEvents: "auto",
      cursor: "pointer",
    });
    el.title = `${flag.drone.agent} · ride`;
    el.textContent = flag.text;
    if (flag.drone.quiet !== null) {
      const quiet = document.createElement("span");
      quiet.textContent = flag.drone.quiet;
      quiet.style.color = "rgba(255, 255, 255, 0.78)";
      quiet.style.fontWeight = "400";
      el.append(quiet);
    }
    const pointer = document.createElement("span");
    Object.assign(pointer.style, {
      position: "absolute",
      left: `${POINTER_INSET_PX}px`,
      top: "100%",
      width: "0",
      height: "0",
      borderTop: `${POINTER_PX}px solid ${flag.hue}`,
      borderRight: `${POINTER_PX}px solid transparent`,
    });
    el.append(pointer);
    el.pointer = pointer;
    const agent = flag.drone.agent;
    el.addEventListener("click", () => {
      // A drone's ride flies the camera to it, so the project only needs to be on screen first.
      if (parked.actions?.select(id)) state.app.ride(agent);
    });
    return el;
  }
  if (flag.hue) {
    const dot = document.createElement("span");
    dot.className = "dot";
    dot.style.background = flag.hue;
    el.append(dot);
  }
  if (flag.text !== "") {
    const text = document.createElement("span");
    text.textContent = flag.text;
    el.append(text);
  }
  return el;
}

function badgeElement(id, badge, clash) {
  const el = document.createElement("div");
  el.className = "badge";
  el.textContent = badge.text;
  el.title = "open the clash card";
  Object.assign(el.style, {
    position: "absolute",
    left: "0",
    top: "0",
    width: `${BADGE_PX}px`,
    height: `${BADGE_PX}px`,
    borderRadius: "50%",
    background: badge.hue,
    color: "#ffffff",
    font: `700 10px/${BADGE_PX}px var(--sans)`,
    textAlign: "center",
    letterSpacing: "-0.02em",
    pointerEvents: "auto",
    cursor: "pointer",
  });
  el.addEventListener("click", () => {
    parked.actions?.fly(id);
    openCard(clash);
  });
  return el;
}

function hudBoxes() {
  const origin = canvas.getBoundingClientRect();
  const boxes = [];
  for (const el of document.querySelectorAll(HUD_SELECTORS)) {
    const rect = el.getBoundingClientRect();
    if (rect.width === 0 && rect.height === 0) continue;
    boxes.push({
      left: rect.left - origin.left,
      right: rect.right - origin.left,
      top: rect.top - origin.top,
      bottom: rect.bottom - origin.top,
    });
  }
  return boxes;
}

function show(el, left, top, placed) {
  if (el.size === undefined) el.size = [el.offsetWidth, el.offsetHeight];
  const [w, h] = el.size;
  const box = boxAt(left, top, w, h, PAD_PX);
  const inside = inView(box, canvas.clientWidth, canvas.clientHeight);
  el.style.visibility = inside ? "visible" : "hidden";
  el.style.transform = `translate(${left}px, ${top}px)`;
  if (inside) placed.push(box);
  return inside;
}

// Places every parked plant's marks. A page with one plant has none and spends nothing here.
export function placeParked() {
  if (!canvasState.multi || state.app === null) return;
  layers();
  sync();
  const skip = parked.order.indexOf(canvasState.active);
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  const placed = hudBoxes();
  const discs = state.app.all_drone_layout();
  for (let k = 0; k + 4 <= discs.length; k += 4) {
    if (discs[k] === skip) continue;
    const [x, y, r] = [discs[k + 1], discs[k + 2], discs[k + 3]];
    placed.push({ left: x - r, right: x + r, top: y - r, bottom: y + r });
  }
  for (const entry of parked.entries.values()) {
    for (const el of entry.flagEls) if (el !== null) el.style.visibility = "hidden";
    for (const pair of entry.badgeEls.values()) for (const el of pair) el.style.visibility = "hidden";
  }
  for (const [project, rows] of splitByProject(state.app.all_badge_layout(), 5, skip)) {
    const entry = parked.entries.get(project);
    if (entry === undefined) continue;
    for (const [clash, lx, rx, y] of rows) {
      const pair = entry.badgeEls.get(clash);
      if (pair === undefined) continue;
      badgeBoxes(lx, rx, y).forEach((box, i) => {
        const inside = inView(box, width, height);
        pair[i].style.visibility = inside ? "visible" : "hidden";
        pair[i].style.transform = `translate(${box.left}px, ${box.top}px)`;
        if (inside) placed.push(box);
      });
    }
  }
  for (const [project, rows] of splitByProject(state.app.all_flag_layout(), 5, skip)) {
    const entry = parked.entries.get(project);
    if (entry !== undefined) placeFlags(entry, rows, placed);
  }
  placeNames(placed, skip, width, height);
  placeRings(skip);
}

function placeFlags(entry, rows, placed) {
  const { columns, alone } = flagGroups(rows);
  for (const { index, x, y } of alone) {
    const el = entry.flagEls[index];
    if (el === undefined || el === null) continue;
    if (el.size === undefined) el.size = [el.offsetWidth, el.offsetHeight];
    const [w, h] = el.size;
    const beside = standsBeside(entry.flags[index].kind);
    const left = beside ? x : x - w / 2;
    show(el, left, clearOf(left, beside ? y - h / 2 : y - LABEL_OFFSET_PX - h, w, h, placed, PAD_PX), placed);
  }
  for (const column of columns.values()) {
    const items = column
      .map(({ index, x, y }) => ({ el: entry.flagEls[index], flag: entry.flags[index], x, y }))
      .filter((item) => item.el !== undefined && item.el !== null)
      .map((item) => ({ ...item, state: item.flag.drone?.state ?? "working", hovered: false }));
    const { shown } = foldColumn(items.filter((item) => agentFlagShown(item.state, state.altDown, false)));
    if (shown.length === 0) continue;
    const single = shown.length === 1;
    const centre = shown.reduce((sum, item) => sum + item.x, 0) / shown.length;
    let bottom = Math.min(...shown.map((item) => item.y)) - (single ? POINTER_PX : LABEL_OFFSET_PX);
    for (const item of shown) {
      if (item.el.size === undefined) item.el.size = [item.el.offsetWidth, item.el.offsetHeight];
      const [w, h] = item.el.size;
      const wantLeft = single ? item.x - POINTER_INSET_PX : centre - w / 2;
      // A parked flag draws no leader, so it only moves up clear of what is placed, never
      // sideways, and one pushed off the top hides rather than stand over another plant.
      const left = wantLeft;
      const top = clearOf(left, bottom - h, w, h, placed, PAD_PX);
      show(item.el, left, top, placed);
      if (item.el.pointer !== undefined) item.el.pointer.style.display = single && top === bottom - h ? "" : "none";
      bottom = top - FLAG_ROW_GAP_PX;
    }
  }
}

// A parked plant's machine names, after its flags, each shown only clear of everything placed.
function placeNames(placed, skip, width, height) {
  const generation = (parked.generation = (parked.generation ?? 0) + 1);
  for (const [project, rows] of splitByProject(state.app.all_label_layout(state.altDown), 5, skip)) {
    const id = parked.order[project];
    for (const [machine, x, y, opacity] of rows) {
      const key = `${id}\u0000${machine}`;
      let el = parked.labels.get(key);
      if (el === undefined) {
        el = document.createElement("div");
        el.className = "label";
        el.textContent = state.app.project_machine_name(id, machine);
        parked.labelsEl.append(el);
        parked.labels.set(key, el);
      }
      el.generation = generation;
      if (el.size === undefined) el.size = [el.offsetWidth, el.offsetHeight];
      const [w, h] = el.size;
      const box = boxAt(x - w / 2, y + LABEL_OFFSET_PX, w, h, PAD_PX);
      const clear = inView(box, width, height) && placed.every((other) => !overlaps(box, other));
      if (clear) placed.push(box);
      el.style.visibility = clear ? "visible" : "hidden";
      el.style.opacity = String(opacity);
      el.style.transform = `translate(${x}px, ${y}px) translate(-50%, ${LABEL_OFFSET_PX}px)`;
    }
  }
  for (const [key, el] of parked.labels) {
    if (el.generation !== generation) {
      el.remove();
      parked.labels.delete(key);
    }
  }
}

function placeRings(skip) {
  const rows = [...splitByProject(state.app.all_beacon_layout(), 4, skip).values()].flat();
  rows.forEach(([x, y, r], i) => {
    let el = parked.rings[i];
    if (el === undefined) {
      el = document.createElement("div");
      el.className = "beacon-ring";
      Object.assign(el.style, {
        position: "absolute",
        left: "0",
        top: "0",
        borderRadius: "50%",
        border: `${BEACON_RING_PX}px solid var(--page)`,
        boxSizing: "border-box",
        pointerEvents: "none",
      });
      parked.flagsEl.prepend(el);
      parked.rings[i] = el;
    }
    const size = 2 * (r + BEACON_RING_PX);
    el.hidden = false;
    el.style.width = `${size}px`;
    el.style.height = `${size}px`;
    el.style.transform = `translate(${x - size / 2}px, ${y - size / 2}px)`;
  });
  for (let i = rows.length; i < parked.rings.length; i += 1) parked.rings[i].hidden = true;
}
