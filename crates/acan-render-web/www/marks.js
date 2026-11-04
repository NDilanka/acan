// The HTML marks over the canvas: agent flags with their witness chips, the leaders to each
// drone, the machine labels and the clash pins. Positions come from the wasm side; this file
// only places the elements.
import { openCard, openQuestionCard, paintCard, placeQuestion, syncQuestions } from "./cards.js";
import { placeCompose, syncCompose } from "./compose.js";
import { guessTop, machineAt, machineQuad } from "./order.js";
import { canvas, flagsEl, labelsEl, leadersEl, state } from "./state.js";
import { requestRender } from "./main.js";
import {
  BADGE_PX, GHOST_HOLD_MS, LAST_SEEN_KEY, agentFlagShown, badgeBoxes, boxAt, clearOf, flagSpot, foldColumn, inView,
  isDraftPlot, markWords, overlaps, placeOverlays, plotLeads, seenOnReturn, standsBeside, unnamedSpots,
} from "./place.js";
import { questionOfFlag } from "./question.js";
import { isLive } from "./stream.js";

const ID_PREFIX = { order: "o", clash: "c", question: "q", task: "t" };

const LABEL_OFFSET_PX = 6;

const LABEL_PAD_PX = 4;

// A district's sign stands this far above its spot on screen, inside the front edge of its pad.
const SIGN_OFFSET_PX = 4;

const PIN_PICK_PX = 18;

// The gap between the flags of one machine, stacked in a column above its bots.
const FLAG_ROW_GAP_PX = 4;

// A machine the wasm side left unnamed gets a name once its footprint is this wide on screen, the
// size the wasm side names a machine at rest (`labels::REST_MIN_CSS_PX`).
const UNNAMED_MIN_PX = 18;

// The note of the scope machines the plant lacks: its top, its left with no receipt plate, and
// its gap from the plate when one shows.
const UNPLACED_TOP_PX = 96;
const UNPLACED_LEFT_PX = 12;
const UNPLACED_GAP_PX = 12;

const SVG_NS = "http://www.w3.org/2000/svg";

// The riding agent's own flag is hidden: the chip replaces it.
function isRiderFlag(flag) {
  return state.ridden !== "" && flag?.drone?.agent === state.ridden;
}

// An agent's name flag is a pill in its hue: PILL_PX high, with a POINTER_PX pointer hanging from
// the middle of its bottom edge, whose tip points down at the drone.
const PILL_PX = 18;
const POINTER_PX = 5;

// Labels come in priority order, larger and nearer first. A label that would overlap one
// already placed stays hidden, so the plant never shows a pile of names. The page's fixed panels,
// the drones and the flags that stay put are placed first, so no name lands on them. The names
// come next, and the plot flags last: a plot flag moves clear of a name, so a name is never hidden
// by one. A plot flag that moves draws a leader to its machine.
export function placeLabels() {
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  state.labelGeneration += 1;
  placeUnplacedNote();
  const placedBoxes = hudBoxes();
  const { plots, leaders } = placeFlags(placedBoxes);
  const quads = state.app.label_layout(state.altDown);
  const shown = [];
  for (let k = 0; k < quads.length; k += 4) {
    const index = quads[k];
    const x = quads[k + 1];
    const y = quads[k + 2];
    const opacity = quads[k + 3];
    shown.push({ index, ...labelAt(index), x, y, opacity });
  }
  const named = new Set(shown.map(({ index }) => index));
  for (const { index, x, y } of unnamedSpots(footprints(), named, width, height, UNNAMED_MIN_PX)) {
    shown.push({ index, ...labelAt(index), x, y, opacity: 1 });
  }
  const { labelShown, plotTops, plotLeaders } = placeOverlays({
    placed: placedBoxes,
    labels: shown.map(({ x, y, w, h }) => ({
      box: boxAt(x - w / 2, y + LABEL_OFFSET_PX, w, h, LABEL_PAD_PX),
    })),
    plots,
    width,
    height,
    pad: LABEL_PAD_PX,
  });
  const taken = [...placedBoxes];
  shown.forEach(({ el, x, y, w, h, opacity }, i) => {
    el.style.visibility = labelShown[i] ? "visible" : "hidden";
    el.style.opacity = String(opacity);
    el.style.transform = `translate(${x}px, ${y}px) translate(-50%, ${LABEL_OFFSET_PX}px)`;
    if (labelShown[i]) taken.push(boxAt(x - w / 2, y + LABEL_OFFSET_PX, w, h, LABEL_PAD_PX));
  });
  let next = leaders;
  plots.forEach((plot, i) => {
    const visible = showFlag(plot.el, plot.left, plotTops[i]);
    plot.el.placed = visible ? { left: plot.left, top: plotTops[i], w: plot.w, h: plot.h } : null;
    if (visible) taken.push(boxAt(plot.left, plotTops[i], plot.w, plot.h, LABEL_PAD_PX));
    const leader = plotLeaders[i];
    if (visible && leader !== null) {
      // The leader takes the flag's own tint, so it reads as that flag's.
      const tint = getComputedStyle(plot.el).borderTopColor;
      drawLeader(next, leader.x1, leader.y1, leader.x2, leader.y2, tint);
      next += 1;
    }
  });
  hideLeadersFrom(next);
  placeSigns(taken, width, height);
  for (const [index, el] of state.labels) {
    if (el.generation !== state.labelGeneration) {
      el.remove();
      state.labels.delete(index);
    }
  }
  placeCompose();
  placeQuestion();
}

// Each district's sign element, by sign index.
const signs = new Map();

// One quiet name per district, standing just inside the front edge of its ground pad beside its
// front machine. The wasm side gives each sign its spots best first; the sign takes the first one
// in view and clear of the machine names and flags, which are placed before it, so a sign never
// hides a machine's name. A sign with no clear spot stays hidden. A plant that reads as one place
// has no pads and no signs.
function placeSigns(taken, width, height) {
  const triples = state.app.district_layout();
  const spots = new Map();
  for (let k = 0; k < triples.length; k += 3) {
    const index = triples[k];
    if (!spots.has(index)) spots.set(index, []);
    spots.get(index).push([triples[k + 1], triples[k + 2]]);
  }
  for (const [index, list] of spots) {
    const name = state.app.district_sign(index);
    let el = signs.get(index);
    if (el === undefined) {
      el = document.createElement("div");
      el.className = "sign";
      labelsEl.appendChild(el);
      signs.set(index, el);
    }
    if (el.textContent !== name) {
      el.textContent = name;
      el.size = undefined;
    }
    if (el.size === undefined) el.size = [el.offsetWidth, el.offsetHeight];
    const [w, h] = el.size;
    const boxOf = ([x, y]) => boxAt(x - w / 2, y - h - SIGN_OFFSET_PX, w, h, LABEL_PAD_PX);
    const spot = name === "" ? undefined : list.find((at) => {
      const box = boxOf(at);
      return inView(box, width, height) && taken.every((other) => !overlaps(box, other));
    });
    el.style.visibility = spot === undefined ? "hidden" : "visible";
    if (spot === undefined) continue;
    taken.push(boxOf(spot));
    const [x, y] = spot;
    el.style.transform = `translate(${x}px, ${y}px) translate(-50%, ${-h - SIGN_OFFSET_PX}px)`;
  }
  for (const [index, el] of signs) {
    if (!spots.has(index)) {
      el.remove();
      signs.delete(index);
    }
  }
}

// The label element of machine `index`, made on first use and measured, with its size.
function labelAt(index) {
  let el = state.labels.get(index);
  if (el === undefined) {
    el = makeLabel(index);
    state.labels.set(index, el);
    labelsEl.appendChild(el);
  }
  el.generation = state.labelGeneration;
  if (el.size === undefined || el.sizedAlt !== state.altDown) {
    el.size = [el.offsetWidth, el.offsetHeight];
    el.sizedAlt = state.altDown;
  }
  const [w, h] = el.size;
  return { el, w, h };
}

// The wasm side's machine index of each name, for the view the page holds.
let indexes = { view: null, of: new Map() };

// Each machine of the view with its wasm index and its footprint on screen.
function footprints() {
  if (state.view === null) return [];
  if (indexes.view !== state.view) {
    const of = new Map();
    for (let i = 0; i < state.app.machine_count(); i += 1) of.set(state.app.machine_name(i), i);
    indexes = { view: state.view, of };
  }
  const project = (x, y) => state.app.world_to_screen(x, y);
  return state.view.machines
    .filter((machine) => indexes.of.has(machine.name))
    .map((machine) => ({ index: indexes.of.get(machine.name), quad: machineQuad(machine, project) }));
}

// The note sits at the top left, and beside the receipt plate while that shows, so it never
// covers the plate. The note is fixed, so these are window coordinates.
function placeUnplacedNote() {
  const note = flagsEl.querySelector(".flag.unplaced");
  if (note === null) return;
  const receipt = document.getElementById("receipt");
  const box = receipt !== null && !receipt.hidden ? receipt.getBoundingClientRect() : null;
  const left = box !== null && box.width > 0 ? box.right + UNPLACED_GAP_PX : UNPLACED_LEFT_PX;
  note.style.left = `${left}px`;
  note.style.top = `${UNPLACED_TOP_PX}px`;
}

// The page's fixed panels: the shell, the key bar, the usage panel, the notice,
// the ride chip, each panel of the right rail (the first-run checklist, the clash card, the slip,
// the inbox, the lanes, the history and the timeline), the receipt plate, the compose panel, the
// question card, the command bar, the vision viewer and its pins, the alpha banner, the replay
// bar and the projects map. A flag or a name never sits under one. A panel that is hidden has no box, so it blocks
// nothing.
export const HUD_SELECTORS = [
  "#shell", "#hint", "#usage", "#notice", "#pov-chip", "#watch > .island", "#receipt",
  "#compose", "#question", "#command", "#vision", ".vpin", "#alpha", "#replay", ".flag.unplaced",
  "#minimap",
].join(", ");

// Each shown panel's box in canvas coordinates, as `{ left, right, top, bottom }`.
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

function makeLabel(index) {
  const el = document.createElement("div");
  el.className = "label";
  // The label shows the short name; the full name is the tooltip and what a screen reader reads.
  const name = state.app.machine_name(index);
  el.textContent = state.app.machine_label(index) || name;
  el.title = name;
  el.setAttribute("aria-label", name);
  const files = state.app.machine_files(index);
  const count = document.createElement("span");
  count.className = "count";
  count.textContent = `${files} ${files === 1 ? "file" : "files"}`;
  el.appendChild(count);
  return el;
}

export function label(kind, id) {
  return `${ID_PREFIX[kind]}-${id}`;
}

// How often the wasm side hears the time, so a quiet agent's drone goes stale and an away stamp
// counts up. A frozen, replayed or rewound page gives it no time, so a recorded run never reads
// stale.
const CLOCK_MS = 30_000;

let clockTimer = null;

function startClock() {
  if (clockTimer !== null || state.app === null) return;
  const tick = () => {
    state.app.set_now(isLive() ? Date.now() : 0);
    requestRender();
  };
  tick();
  clockTimer = setInterval(tick, CLOCK_MS);
}

function readSeen() {
  try {
    return localStorage.getItem(LAST_SEEN_KEY);
  } catch {
    return null;
  }
}

// Only a live page keeps the time, so a frozen or replayed visit does not move it.
function noteSeen() {
  if (!isLive()) return;
  try {
    localStorage.setItem(LAST_SEEN_KEY, String(Date.now()));
  } catch {
    // Storage is off, so the page shows no ghosts.
  }
}

let ghostTimer = null;

// The reader is back, on load or when the tab shows again. After a long enough absence the
// machines where work landed meanwhile show ghosts, which go after GHOST_HOLD_MS if nothing
// clears them first. The time is kept again at once, so a reload does not bring them back.
function comeBack() {
  const now = Date.now();
  const seen = seenOnReturn(readSeen(), now, isLive());
  noteSeen();
  if (seen === 0) return;
  state.app.set_now(now);
  state.app.set_seen(seen, now);
  clearTimeout(ghostTimer);
  ghostTimer = setTimeout(() => {
    if (state.app.clear_ghosts()) requestRender();
  }, GHOST_HOLD_MS);
  requestRender();
}

let seenWatched = false;

function watchSeen() {
  if (seenWatched || state.app === null) return;
  seenWatched = true;
  comeBack();
  window.addEventListener("pagehide", noteSeen);
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") noteSeen();
    else comeBack();
  });
}

// The canvas the pointer listeners are on. The page swaps in a fresh canvas when a backend fails,
// so they move to the new one.
let watchedCanvas = null;

// A pointer passing over a ghost, or a click on it, clears it; a drag does not. A pointer that
// leaves the canvas, or presses to drag, hovers no drone.
function watchCanvas() {
  if (watchedCanvas === canvas) return;
  watchedCanvas = canvas;
  const clear = (event) => {
    if (state.app === null || event.currentTarget !== canvas) return;
    if (event.type === "pointermove" && event.buttons !== 0) return;
    const rect = canvas.getBoundingClientRect();
    if (state.app.clear_ghost_at(event.clientX - rect.left, event.clientY - rect.top)) requestRender();
  };
  const unhover = (event) => {
    if (event.currentTarget !== canvas) return;
    setHover(null);
    setMapHover(null);
  };
  canvas.addEventListener("pointermove", clear);
  canvas.addEventListener("pointerup", clear);
  canvas.addEventListener("pointerleave", unhover);
  canvas.addEventListener("pointerdown", unhover);
}

// The agent whose drone the pointer is over, or null.
let hovered = null;

// Hovers `agent`'s drone, or none for null. Only a change tells the page, as an
// `acan:drone-hover` event on `document` whose detail names the agent as `view.agents[].id` does.
function setHover(agent) {
  if (agent === hovered || state.app === null) return;
  hovered = agent;
  state.app.set_hovered(agent ?? "");
  pointCursor();
  document.dispatchEvent(new CustomEvent("acan:drone-hover", { detail: { agent } }));
  requestRender();
}

// The pointer is at the canvas point (x, y) with no drag: a drone within its radius and a few px
// is hovered. Its tether inks in, its flag comes to the front with its detail, and a paper halo
// rings it.
export function droneHover(x, y) {
  if (state.app === null) return;
  const agent = state.app.drone_at(x, y);
  setHover(agent === "" ? null : agent);
  setMapHover(agent === "" ? markAt(x, y) : null);
}

// What else on the map the pointer is over, as `markWords` reads it: a clash diamond as
// `{ clash }`, else a machine as `{ machine }`, or null. Machines are looked up only while a
// flag waits on one, so a plant with no order spends nothing on the pointer.
function markAt(x, y) {
  const clash = pinAt(x, y);
  if (clash !== null) return { clash };
  const mark = plotMarkAt(x, y);
  if (mark !== null) return { machine: mark };
  if (state.view === null || !state.markState.flags.some((flag) => flag.on && flag.kind !== "clash")) return null;
  const project = (wx, wy) => state.app.world_to_screen(wx, wy);
  const topOf = (machine) => {
    const top = state.app.machine_top(machine.name);
    return top >= 0 ? top : guessTop(machine);
  };
  // A machine behind a close camera projects to a huge outline that would swallow the pointer,
  // so only machines whose footprint lies near the view are picked.
  const [w, h] = [canvas.clientWidth, canvas.clientHeight];
  // A machine the scan has not filled takes no pointer, so its outline is not picked either.
  const near = state.view.machines.filter((machine) => !state.app.machine_hidden(machine.name)
    && machineQuad(machine, project).every(([px, py]) => px >= -w && px <= 2 * w && py >= -h && py <= 2 * h));
  const machine = machineAt(near, project, x, y, [], topOf);
  return machine === null ? null : { machine };
}

// How far past a plot mark's box the pointer still hovers it, in CSS px.
const MARK_PICK_PX = 4;

// The machine whose plot mark, or open plot flag, is under the CSS point, or null.
function plotMarkAt(x, y) {
  for (let i = 0; i < state.flagEls.length; i += 1) {
    const box = state.flagEls[i].placed;
    if (!box || state.flagEls[i].style.visibility !== "visible") continue;
    if (x >= box.left - MARK_PICK_PX && x <= box.left + box.w + MARK_PICK_PX
      && y >= box.top - MARK_PICK_PX && y <= box.top + box.h + MARK_PICK_PX) {
      return state.markState.flags[i].on ?? null;
    }
  }
  return null;
}

// The map mark under the pointer, besides a drone, as `markAt` gives it.
let mapHover = null;

function setMapHover(next) {
  if (next?.clash === mapHover?.clash && next?.machine === mapHover?.machine) return;
  mapHover = next;
  pointCursor();
  requestRender();
}

// A drone or a clash under the pointer takes a click, so it shows the pointer cursor. An inline
// cursor wins over the canvas's grab cursor, and clearing it gives that back.
function pointCursor() {
  canvas.style.cursor = hovered !== null || mapHover?.clash !== undefined ? "pointer" : "";
}

// The paper halo round the hovered drone, 2 px wide.
const HALO_PX = 2;
let haloEl = null;

function placeHalo() {
  const disc = hovered === null ? [] : state.app.drone_disc_of(hovered);
  if (disc.length === 0) {
    if (haloEl !== null) haloEl.hidden = true;
    return;
  }
  if (haloEl === null) {
    haloEl = document.createElement("div");
    haloEl.className = "halo";
    haloEl.setAttribute("aria-hidden", "true");
    Object.assign(haloEl.style, {
      position: "absolute",
      left: "0",
      top: "0",
      borderRadius: "50%",
      border: `${HALO_PX}px solid var(--page)`,
      boxSizing: "border-box",
      pointerEvents: "none",
    });
  }
  if (haloEl.parentNode !== flagsEl) flagsEl.prepend(haloEl);
  const [x, y, r] = disc;
  const size = 2 * (r + HALO_PX);
  haloEl.hidden = false;
  haloEl.style.width = `${size}px`;
  haloEl.style.height = `${size}px`;
  haloEl.style.transform = `translate(${x - size / 2}px, ${y - size / 2}px)`;
}

// The 1.5 px paper ring round each working drone's beacon, so the 7 px hue dot reads on a grey
// machine top. One ring per beacon, made on first use and kept across paints.
const BEACON_RING_PX = 1.5;
const beaconRingEls = [];

function placeBeaconRings() {
  const discs = state.app.beacon_layout();
  const count = discs.length / 3;
  for (let i = 0; i < count; i += 1) {
    if (beaconRingEls[i] === undefined) {
      const el = document.createElement("div");
      el.className = "beacon-ring";
      el.setAttribute("aria-hidden", "true");
      Object.assign(el.style, {
        position: "absolute",
        left: "0",
        top: "0",
        borderRadius: "50%",
        border: `${BEACON_RING_PX}px solid var(--page)`,
        boxSizing: "border-box",
        pointerEvents: "none",
      });
      beaconRingEls[i] = el;
    }
    const el = beaconRingEls[i];
    if (el.parentNode !== flagsEl) flagsEl.prepend(el);
    const [x, y, r] = [discs[3 * i], discs[3 * i + 1], discs[3 * i + 2]];
    const size = 2 * (r + BEACON_RING_PX);
    el.hidden = false;
    el.style.width = `${size}px`;
    el.style.height = `${size}px`;
    el.style.transform = `translate(${x - size / 2}px, ${y - size / 2}px)`;
  }
  for (let i = count; i < beaconRingEls.length; i += 1) beaconRingEls[i].hidden = true;
}

// The ghosts' Alt chips, made on first use. paintMarks empties the flags, and these with them.
let ghostEls = [];

// The `+N` flags of the columns that fold, made on first use and emptied with the flags.
let moreEls = [];

// The `index`th `+N` flag, saying `folded` more agents are on the machine, measured.
function moreElement(index, folded) {
  let el = moreEls[index];
  if (el === undefined) {
    el = document.createElement("div");
    el.className = "flag more";
    Object.assign(el.style, {
      height: `${PILL_PX}px`,
      borderRadius: `${PILL_PX / 2}px`,
      padding: "0 7px",
      boxSizing: "border-box",
      font: `600 11px/${PILL_PX}px var(--sans)`,
      color: "var(--muted)",
    });
    flagsEl.append(el);
    moreEls[index] = el;
  }
  const words = `+${folded}`;
  if (el.textContent !== words) {
    el.textContent = words;
    el.size = undefined;
  }
  el.hidden = false;
  if (el.size === undefined) el.size = [el.offsetWidth, el.offsetHeight];
  return el;
}

// Each clash's two badge elements, by clash index, as paintMarks last made them.
let badgeEls = new Map();

function badgeElement(badge, clash) {
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
  el.addEventListener("click", () => openCard(clash));
  // A badge takes the pointer from the canvas, so it opens its clash's detail itself.
  el.addEventListener("pointerenter", () => setMapHover({ clash }));
  el.addEventListener("pointerleave", () => setMapHover(null));
  return el;
}

// The badges sit either side of their diamonds. They are placed before the flags, so no flag or
// name lands on one.
function placeBadges(placedBoxes) {
  const quads = state.app.badge_layout();
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  for (let k = 0; k < quads.length; k += 4) {
    const pair = badgeEls.get(quads[k]);
    if (pair === undefined) continue;
    badgeBoxes(quads[k + 1], quads[k + 2], quads[k + 3]).forEach((box, i) => {
      const inside = inView(box, width, height);
      pair[i].style.visibility = inside ? "visible" : "hidden";
      pair[i].style.transform = `translate(${box.left}px, ${box.top}px)`;
      if (inside) placedBoxes.push(box);
    });
  }
}

// With Alt down, each ghost says how many tasks landed on its machine, above the corner of its
// outline nearest the viewer.
function placeGhostFlags(placedBoxes) {
  const triples = state.altDown ? state.app.ghost_layout() : [];
  const shown = triples.length / 3;
  for (let i = 0; i < shown; i += 1) {
    let el = ghostEls[i];
    if (el === undefined) {
      el = document.createElement("div");
      el.className = "flag ghost";
      el.setAttribute("aria-hidden", "true");
      flagsEl.append(el);
      ghostEls[i] = el;
    }
    const words = `${triples[3 * i + 2]} landed`;
    if (el.textContent !== words) {
      el.textContent = words;
      el.size = undefined;
    }
    el.hidden = false;
    if (el.size === undefined) el.size = [el.offsetWidth, el.offsetHeight];
    const [w, h] = el.size;
    const left = triples[3 * i] - w / 2;
    const top = clearOf(left, triples[3 * i + 1] - LABEL_OFFSET_PX - h, w, h, placedBoxes, LABEL_PAD_PX);
    placeFlag(el, left, top, placedBoxes);
  }
  for (let i = shown; i < ghostEls.length; i += 1) ghostEls[i].hidden = true;
}

// The plot flag that leads each machine, by flag index, as `plotLeads` gives it.
let plotLeadOf = new Map();

// A plot rests as one small amber mark on its machine: a ring while any scope there is a draft,
// a dot once every one is confirmed. Its line and Alt detail open on hover.
const PLOT_MARK_PX = 10;
const AMBER = "#d9a441";
// The draft ring is a deeper amber, wider and a little larger than the dot, so it holds at 3:1 or
// better against the pale amber a draft plot tints its machine.
export const DRAFT_RING_PX = 12;
export const DRAFT_RING_WIDTH_PX = 3;
export const DRAFT_AMBER = "#a86f12";

function plotMark(el, draft) {
  const mark = document.createElement("span");
  const size = draft ? DRAFT_RING_PX : PLOT_MARK_PX;
  mark.className = "mark";
  mark.title = draft ? "draft scope" : "confirmed scope";
  Object.assign(mark.style, {
    display: "none",
    width: `${size}px`,
    height: `${size}px`,
    borderRadius: "50%",
    boxSizing: "border-box",
    background: draft ? "var(--page)" : AMBER,
    border: draft ? `${DRAFT_RING_WIDTH_PX}px solid ${DRAFT_AMBER}` : "none",
    boxShadow: "0 0 0 1.5px var(--page)",
  });
  el.prepend(mark);
  el.mark = mark;
}

// Shows a plot flag as its mark or its words, as `markWords` says, measuring it again on a change.
function setPlotMode(el, mode) {
  if (el.mode === mode || el.mark === undefined) return;
  el.mode = mode;
  const rest = mode === "mark";
  for (const child of el.children) child.style.display = (child === el.mark) === rest ? "" : "none";
  Object.assign(el.style, rest
    ? { border: "none", background: "none", padding: "0", boxShadow: "none" }
    : { border: "", background: "", padding: "", boxShadow: "" });
  el.size = undefined;
}

// The marks come from the wasm side: each agent with its activity, the active order, each open
// clash and each question. They are rebuilt when the view changes, and placed every frame.
export function paintMarks() {
  startClock();
  watchSeen();
  watchCanvas();
  state.markState = JSON.parse(state.app.marks_json());
  flagsEl.replaceChildren();
  ghostEls = [];
  moreEls = [];
  badgeEls = new Map();
  for (const badges of state.markState.badges) {
    const pair = [badgeElement(badges.a, badges.clash), badgeElement(badges.b, badges.clash)];
    flagsEl.append(...pair);
    badgeEls.set(badges.clash, pair);
  }
  plotLeadOf = plotLeads(state.markState.flags);
  state.flagEls = state.markState.flags.map((flag, index) => {
    const el = flagElement(flag);
    if (flag.kind === "plot") plotMark(el, plotLeadOf.get(index)?.draft ?? isDraftPlot(flag));
    flagsEl.append(el);
    return el;
  });
  const unplaced = state.markState.unplaced;
  if (unplaced.length > 0) {
    const note = document.createElement("div");
    note.className = "flag unplaced";
    note.textContent = `scope names ${unplaced.length} machine${unplaced.length === 1 ? "" : "s"} this plant lacks: ${unplaced.join(", ")}`;
    flagsEl.append(note);
  }
  paintCard();
  syncCompose();
  syncQuestions();
}

function flagElement(flag) {
  const el = document.createElement("div");
  el.className = `flag ${flag.kind}`;
  el.classList.toggle("leaving", flag.leaving);
  // A tour step's chips say what the plate's stage list says in words, so a screen reader hears
  // them once, from the list.
  if (flag.kind === "stage") el.setAttribute("aria-hidden", "true");
  // The pill itself carries an agent's hue, so it needs no hue dot.
  if (flag.hue && flag.kind !== "agent") {
    const dot = document.createElement("span");
    dot.className = "dot";
    dot.style.background = flag.hue;
    el.append(dot);
    if (flag.kind === "away") awayStyle(el, dot, flag.hue);
  }
  if (flag.kind === "landed") {
    const dot = document.createElement("span");
    dot.className = "dot land";
    el.append(dot);
    el.classList.toggle("quiet", flag.text === "");
  }
  if (flag.text !== "") {
    const text = document.createElement("span");
    text.textContent = flag.text;
    el.append(text);
  }
  if (flag.kind === "agent") pillStyle(el, flag);
  if (flag.kind === "agent" && flag.drone?.state === "drift") chevronStyle(el);
  if (flag.check) el.append(checkChip(flag.check, flag.kind === "agent"));
  if (flag.kind === "plot") el.classList.add(flag.text.includes(" confirmed ") ? "confirmed" : "draft");
  if (flag.scope) {
    const scope = document.createElement("span");
    scope.className = "scope";
    scope.textContent = state.altDown ? flag.scope : "in confirmed scope";
    el.title = flag.scope;
    el.append(scope);
  }
  if (flag.kind === "clash") clashStyle(el);
  if ((state.altDown || flag.kind === "clash") && flag.detail.length > 0 && flag.kind !== "agent") {
    const lines = document.createElement("span");
    lines.className = "detail";
    for (const line of flag.detail) {
      const row = document.createElement("span");
      row.textContent = line;
      lines.append(row);
    }
    el.append(lines);
  }
  if (flag.waiting) {
    const amber = document.createElement("span");
    amber.className = "dot wait";
    // A paper ring keeps the amber clear of the hue under it.
    if (flag.kind === "agent") amber.style.boxShadow = "0 0 0 1px #ffffff";
    el.append(amber);
  }
  if (flag.clash !== null) {
    el.addEventListener("click", () => openCard(flag.clash));
  }
  if (flag.kind === "question") {
    el.addEventListener("click", () => {
      const question = questionOfFlag(state.view, flag);
      if (question !== undefined) openQuestionCard(question.id);
    });
  }
  return el;
}

// A clash's flag is its detail, shown only while its diamond is hovered: the id and the overlap,
// then its two agents. It takes no pointer, so the pointer stays on the diamond under it.
function clashStyle(el) {
  Object.assign(el.style, { flexDirection: "column", alignItems: "flex-start", gap: "2px", pointerEvents: "none" });
}

// Opens or closes a pill's detail, measuring it again when that changes its size.
function openDetail(el, open) {
  if (el.detail === undefined || el.detailOpen === open) return;
  el.detailOpen = open;
  el.detail.style.display = open ? "" : "none";
  el.size = undefined;
}

// The pill: white 11 px text on the agent's hue, a stale agent's quiet time in softer white, and
// with Alt its activity and task title. A pointer at its bottom left points at the crown, unless
// the flag stands in a column or moved, where a leader in its hue takes over.
function pillStyle(el, flag) {
  Object.assign(el.style, {
    background: flag.hue,
    color: "#ffffff",
    border: "none",
    borderRadius: `${PILL_PX / 2}px`,
    height: `${PILL_PX}px`,
    padding: "0 8px",
    boxSizing: "border-box",
    font: `600 11px/${PILL_PX}px var(--sans)`,
    gap: "5px",
  });
  el.title = flag.drone.agent;
  const soft = (words) => {
    const span = document.createElement("span");
    span.textContent = words;
    span.style.color = "rgba(255, 255, 255, 0.78)";
    span.style.fontWeight = "400";
    return span;
  };
  if (flag.drone.quiet !== null) el.append(soft(flag.drone.quiet));
  if (flag.detail.length > 0) {
    // The detail shows with Alt or while the drone is hovered; placeFlags opens and closes it.
    el.detail = soft(flag.detail.join(" \u00B7 "));
    el.detailOpen = state.altDown;
    el.detail.style.display = el.detailOpen ? "" : "none";
    el.append(el.detail);
  }
  const pointer = document.createElement("span");
  pointer.className = "pointer";
  Object.assign(pointer.style, {
    position: "absolute",
    left: "50%",
    top: "100%",
    width: "0",
    height: "0",
    borderTop: `${POINTER_PX}px solid ${flag.hue}`,
    borderRight: `${POINTER_PX}px solid transparent`,
  });
  el.append(pointer);
  el.pointer = pointer;
}

// A drifting drone's flag carries a hollow grey chevron, 10 px, just above its pill. The drone keeps
// its hue, so the drift reads in grey and needs no colour of its own.
function chevronStyle(el) {
  const chevron = document.createElement("span");
  chevron.className = "chevron";
  chevron.setAttribute("aria-hidden", "true");
  Object.assign(chevron.style, {
    position: "absolute",
    left: "50%",
    bottom: "100%",
    width: "10px",
    height: "10px",
    marginLeft: "-5px",
    boxSizing: "border-box",
    background: "none",
    borderTop: "2px solid var(--muted)",
    borderLeft: "2px solid var(--muted)",
    transform: "rotate(45deg)",
  });
  el.append(chevron);
}

// An away dot is a bare hollow ring in the agent's hue with its quiet time in muted mono, so it
// reads as where the drone was and not as a flag.
function awayStyle(el, ring, hue) {
  Object.assign(el.style, {
    background: "none",
    border: "none",
    padding: "0",
    gap: "4px",
    font: "12px var(--mono)",
    color: "var(--muted)",
  });
  Object.assign(ring.style, {
    width: "10px",
    height: "10px",
    flex: "none",
    background: "none",
    border: `2px solid ${hue}`,
    boxSizing: "border-box",
  });
}

// A witness chip: a grey ring while the check is pending, a dark tick when it passed, a red dot
// when it failed. Alt adds its words, such as `w3 · passed`.
function checkChip(check, onPill = false) {
  const chip = document.createElement("span");
  chip.className = `chip ${check.state}`;
  // On a hue pill the chip keeps a paper ground, so its grey ring and dark tick still read.
  if (onPill) {
    Object.assign(chip.style, { background: "#ffffff", borderRadius: "6px", padding: "0 3px", lineHeight: "12px" });
  }
  chip.title = `${check.name} · ${check.state}`;
  chip.append(document.createElement("i"));
  if (state.altDown) {
    const words = document.createElement("span");
    words.textContent = `${check.name} · ${check.state}`;
    chip.append(words);
  }
  return chip;
}

// Flags are placed before the names and are never hidden for overlap; a flag is hidden only when
// it is off the canvas. An agent's name sits directly above its own drone, centred on it, and
// follows the drone as it bobs. At most FLAGS_PER_COLUMN of a machine's names show, with the rest
// folded into `+N`. A name that keeps its place points at its drone with a small pointer. A name
// that would cover a box placed before it moves up clear of it, and a leader in its hue then runs
// down to its drone. An idle drone's flag shows only with Alt. A clash's detail shows only while
// its diamond is hovered, and starts to the right of it; the order's tag sits above its machine
// and shows only while that machine is hovered. A flag that would cover a flag or a panel placed
// before it moves up clear of it. Each flag keeps its place as the drones and diamonds move. The
// plot flags are the exception: they are returned, not placed here, and placed after the names, so
// they move clear of a name. Returns the plot flags and how many leader lines the flags drew.
function placeFlags(placedBoxes) {
  const plots = [];
  // The drones come first, so no flag or name is placed over one.
  const discs = state.app.drone_layout();
  for (let k = 0; k < discs.length; k += 3) {
    const [x, y, r] = [discs[k], discs[k + 1], discs[k + 2]];
    placedBoxes.push({ left: x - r, right: x + r, top: y - r, bottom: y + r });
  }
  placeBadges(placedBoxes);
  const quads = state.app.flag_layout();
  const columns = new Map();
  for (let k = 0; k < quads.length; k += 4) {
    const el = state.flagEls[quads[k]];
    if (el === undefined) continue;
    el.hidden = isRiderFlag(state.markState.flags[quads[k]]);
    if (el.hidden) continue;
    const machine = quads[k + 1];
    const x = quads[k + 2];
    const y = quads[k + 3];
    const agent = state.markState.flags[quads[k]].drone?.agent;
    if (agent !== undefined) openDetail(el, state.altDown || agent === hovered);
    if (el.classList.contains("plot")) {
      const mode = markWords(state.markState.flags[quads[k]], mapHover, state.altDown, plotLeadOf.has(quads[k]));
      if (mode === "hidden") {
        el.style.visibility = "hidden";
        el.placed = null;
        continue;
      }
      setPlotMode(el, mode);
    }
    if (el.size === undefined) el.size = [el.offsetWidth, el.offsetHeight];
    if (machine < 0 && el.classList.contains("plot")) {
      const [w, h] = el.size;
      plots.push({ el, left: x - w / 2, top: y - LABEL_OFFSET_PX - h, w, h, x, y });
      continue;
    }
    if (machine >= 0) {
      const flag = state.markState.flags[quads[k]];
      if (!columns.has(machine)) columns.set(machine, []);
      const isHovered = agent !== undefined && agent === hovered;
      el.style.zIndex = isHovered ? "1" : "";
      columns.get(machine).push({ el, x, y, flag, state: flag.drone?.state ?? "working", hovered: isHovered });
      continue;
    }
    if (markWords(state.markState.flags[quads[k]], mapHover, state.altDown) === "hidden") {
      el.style.visibility = "hidden";
      continue;
    }
    const [w, h] = el.size;
    const beside = standsBeside(state.markState.flags[quads[k]].kind);
    const left = beside ? x : x - w / 2;
    const top = clearOf(
      left,
      beside ? y - h / 2 : y - LABEL_OFFSET_PX - h,
      w,
      h,
      placedBoxes,
      LABEL_PAD_PX,
    );
    placeFlag(el, left, top, placedBoxes);
  }
  let leaders = 0;
  let more = 0;
  for (const column of columns.values()) {
    const wanted = column.filter((item) => agentFlagShown(item.state, state.altDown, item.hovered));
    for (const item of column) item.el.style.visibility = "hidden";
    const { shown, folded } = foldColumn(wanted);
    if (shown.length === 0) continue;
    const centre = shown.reduce((sum, item) => sum + item.x, 0) / shown.length;
    let highest = Infinity;
    // Each name sits directly above its own drone, centred on it, so it follows the drone. It
    // moves up only where it would cover a box placed before it, and a leader then runs to it.
    for (const item of shown) {
      const [w, h] = item.el.size;
      const wantLeft = item.x - w / 2;
      const wantTop = item.y - POINTER_PX - h;
      const { left, top } = flagSpot(
        wantLeft,
        wantTop,
        w,
        h,
        placedBoxes,
        LABEL_PAD_PX,
        canvas.clientWidth,
      );
      placeFlag(item.el, left, top, placedBoxes);
      const pointed = left === wantLeft && top === wantTop;
      if (item.el.pointer !== undefined) item.el.pointer.style.display = pointed ? "" : "none";
      if (!pointed) {
        drawLeader(leaders, left + w / 2, top + h, item.x, item.y, item.flag.hue ?? "", "1px");
        leaders += 1;
      }
      highest = Math.min(highest, top);
    }
    if (folded > 0) {
      const el = moreElement(more, folded);
      more += 1;
      const [w, h] = el.size;
      const left = centre - w / 2;
      const top = clearOf(left, highest - FLAG_ROW_GAP_PX - h, w, h, placedBoxes, LABEL_PAD_PX);
      placeFlag(el, left, top, placedBoxes);
    }
  }
  for (let i = more; i < moreEls.length; i += 1) moreEls[i].hidden = true;
  placeGhostFlags(placedBoxes);
  placeBeaconRings();
  placeHalo();
  return { plots, leaders };
}

// Hides the leader lines from `index` on: the ones this placement did not draw.
function hideLeadersFrom(index) {
  for (let i = index; i < state.leaderEls.length; i += 1) {
    state.leaderEls[i].style.display = "none";
  }
}

function placeFlag(el, left, top, placedBoxes) {
  const [w, h] = el.size;
  placedBoxes.push(boxAt(left, top, w, h, LABEL_PAD_PX));
  showFlag(el, left, top);
}

// Shows a flag at (left, top), unless it, with its padding, is off the canvas. Returns whether it
// shows.
function showFlag(el, left, top) {
  const [w, h] = el.size;
  const inside = inView(boxAt(left, top, w, h, LABEL_PAD_PX), canvas.clientWidth, canvas.clientHeight);
  el.style.visibility = inside ? "visible" : "hidden";
  el.style.transform = `translate(${left}px, ${top}px)`;
  return inside;
}

// A thin line from a flag's bottom edge to its bot's crown. The leaders sit under the flags, so
// a flag hides the start of its own line. A tint, when given, colours the line and makes it a
// little heavier, so a moved plot or keep flag's line reads at a glance; else it is grey. An agent's
// leader passes its own 1 px width, the weight of the pointer it stands in for.
function drawLeader(index, x1, y1, x2, y2, tint = "", width = "1.5px") {
  let line = state.leaderEls[index];
  if (line === undefined) {
    line = document.createElementNS(SVG_NS, "line");
    leadersEl.append(line);
    state.leaderEls[index] = line;
  }
  line.setAttribute("x1", String(x1));
  line.setAttribute("y1", String(y1));
  line.setAttribute("x2", String(x2));
  line.setAttribute("y2", String(y2));
  line.style.stroke = tint;
  line.style.strokeWidth = tint === "" ? "" : width;
  line.style.display = "";
}

// The pin whose tip lies within PIN_PICK_PX of the CSS point, nearest first, or null.
export function pinAt(x, y) {
  const quads = state.app.pin_layout();
  let found = null;
  let best = PIN_PICK_PX;
  for (let k = 0; k < quads.length; k += 3) {
    const distance = Math.hypot(quads[k + 1] - x, quads[k + 2] - y);
    if (distance < best) {
      best = distance;
      found = quads[k];
    }
  }
  return found;
}
