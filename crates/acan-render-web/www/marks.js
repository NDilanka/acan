// The HTML marks over the canvas: agent flags with their witness chips, the leaders to each
// drone, the machine labels and the clash pins. Positions come from the wasm side; this file
// only places the elements.
import { openCard, openQuestionCard, paintCard, placeQuestion, syncQuestions } from "./cards.js";
import { placeCompose, syncCompose } from "./compose.js";
import { machineQuad } from "./order.js";
import { canvas, flagsEl, labelsEl, leadersEl, state } from "./state.js";
import { requestRender } from "./main.js";
import {
  BADGE_PX, GHOST_HOLD_MS, LAST_SEEN_KEY, agentFlagShown, badgeBoxes, boxAt, clearOf, foldColumn, inView,
  placeOverlays, seenOnReturn, standsBeside, unnamedSpots,
} from "./place.js";
import { questionOfFlag } from "./question.js";
import { isLive } from "./stream.js";

const ID_PREFIX = { order: "o", clash: "c", question: "q", task: "t" };

const LABEL_OFFSET_PX = 6;

const LABEL_PAD_PX = 4;

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

// An agent's name flag is a pill in its hue: PILL_PX high, with a POINTER_PX pointer at its
// bottom left, POINTER_INSET_PX in from its edge, that points at the drone's crown.
const PILL_PX = 18;
const POINTER_PX = 5;
const POINTER_INSET_PX = 6;

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
  shown.forEach(({ el, x, y, opacity }, i) => {
    el.style.visibility = labelShown[i] ? "visible" : "hidden";
    el.style.opacity = String(opacity);
    el.style.transform = `translate(${x}px, ${y}px) translate(-50%, ${LABEL_OFFSET_PX}px)`;
  });
  let next = leaders;
  plots.forEach((plot, i) => {
    const visible = showFlag(plot.el, plot.left, plotTops[i]);
    const leader = plotLeaders[i];
    if (visible && leader !== null) {
      // The leader takes the flag's own tint, so it reads as that flag's.
      const tint = getComputedStyle(plot.el).borderTopColor;
      drawLeader(next, leader.x1, leader.y1, leader.x2, leader.y2, tint);
      next += 1;
    }
  });
  hideLeadersFrom(next);
  for (const [index, el] of state.labels) {
    if (el.generation !== state.labelGeneration) {
      el.remove();
      state.labels.delete(index);
    }
  }
  placeCompose();
  placeQuestion();
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

// The page's fixed panels: the shell, the order line, the key bar, the usage panel, the notice,
// the ride chip, each panel of the right rail (the first-run checklist, the clash card, the slip,
// the inbox, the lanes, the history and the timeline), the receipt plate, the compose panel, the
// question card, the command bar, the vision viewer and its pins, the alpha banner and the replay
// bar. A flag or a name never sits under one. A panel that is hidden has no box, so it blocks
// nothing.
export const HUD_SELECTORS = [
  "#shell", "#order", "#hint", "#usage", "#notice", "#pov-chip", "#watch > .island", "#receipt",
  "#compose", "#question", "#command", "#vision", ".vpin", "#alpha", "#replay", ".flag.unplaced",
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
  el.textContent = state.app.machine_name(index);
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
    if (event.currentTarget === canvas) setHover(null);
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
  // An inline cursor wins over the canvas's grab cursor, and clearing it gives that back.
  canvas.style.cursor = agent === null ? "" : "pointer";
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
  state.flagEls = state.markState.flags.map((flag) => {
    const el = flagElement(flag);
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
  if (flag.check) el.append(checkChip(flag.check, flag.kind === "agent"));
  if (flag.kind === "plot") el.classList.add(flag.text.includes(" confirmed ") ? "confirmed" : "draft");
  if (flag.scope) {
    const scope = document.createElement("span");
    scope.className = "scope";
    scope.textContent = state.altDown ? flag.scope : "in confirmed scope";
    el.title = flag.scope;
    el.append(scope);
  }
  if (state.altDown && flag.detail.length > 0 && flag.kind !== "agent") {
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
    left: `${POINTER_INSET_PX}px`,
    top: "100%",
    width: "0",
    height: "0",
    borderTop: `${POINTER_PX}px solid ${flag.hue}`,
    borderRight: `${POINTER_PX}px solid transparent`,
  });
  el.append(pointer);
  el.pointer = pointer;
}

// An away dot is a bare hollow ring in the agent's hue with its quiet time in muted mono, so it
// reads as where the drone was and not as a flag.
function awayStyle(el, ring, hue) {
  Object.assign(el.style, {
    background: "none",
    border: "none",
    padding: "0",
    gap: "4px",
    font: "11px var(--mono)",
    color: "var(--muted)",
  });
  Object.assign(ring.style, {
    background: "none",
    border: `1.5px solid ${hue}`,
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
// it is off the canvas. An agent's flags stack in a column above their machine's drones, at most
// FLAGS_PER_COLUMN with the rest folded into `+N`, and a flag never sits on a drone because the
// column starts above the crowns. A lone flag that keeps its place points at its drone; any other
// draws a leader in its hue. An idle drone's flag shows only with Alt. A clash label starts to the right of its diamond, and the order flag sits
// above its machine. A flag that would cover a flag or a panel placed before it moves up clear of
// it. Each flag keeps its place as the drones and diamonds move. The plot flags are the exception:
// they are returned, not placed here, and placed after the names, so they move clear of a name.
// Returns the plot flags and how many leader lines the flags drew.
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
    const single = shown.length === 1;
    const centre = shown.reduce((sum, item) => sum + item.x, 0) / shown.length;
    let bottom = Math.min(...shown.map((item) => item.y)) - (single ? POINTER_PX : LABEL_OFFSET_PX);
    let lastLeft = centre;
    for (const item of shown) {
      const [w, h] = item.el.size;
      const left = single ? item.x - POINTER_INSET_PX : centre - w / 2;
      const top = clearOf(left, bottom - h, w, h, placedBoxes, LABEL_PAD_PX);
      placeFlag(item.el, left, top, placedBoxes);
      const pointed = single && top === bottom - h;
      if (item.el.pointer !== undefined) item.el.pointer.style.display = pointed ? "" : "none";
      if (!pointed) {
        drawLeader(leaders, left + (single ? POINTER_INSET_PX : w / 2), top + h, item.x, item.y, item.flag.hue ?? "", "1px");
        leaders += 1;
      }
      lastLeft = left;
      bottom = top - FLAG_ROW_GAP_PX;
    }
    if (folded > 0) {
      const el = moreElement(more, folded);
      more += 1;
      const [w, h] = el.size;
      const left = single ? lastLeft : centre - w / 2;
      placeFlag(el, left, clearOf(left, bottom - h, w, h, placedBoxes, LABEL_PAD_PX), placedBoxes);
    }
  }
  for (let i = more; i < moreEls.length; i += 1) moreEls[i].hidden = true;
  placeGhostFlags(placedBoxes);
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
