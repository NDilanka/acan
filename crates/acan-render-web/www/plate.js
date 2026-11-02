// The receipt plate: the facts of the receipt the order island shows, the directed and plain
// toggle, and the playback of the admitted tour. The wasm side admits the tour, fills every caption
// and moves the camera. This file shows what the payload says and never a model's words. Every
// item is listed in view order whatever the tour does, so a tour cannot hide a failure.
import { shownOrder } from "./panels.js";
import { requestRender } from "./main.js";
import {
  DIRECTED,
  PLAIN,
  STEP_MS,
  advance,
  chipText,
  clampIndex,
  countLines,
  countSummary,
  itemTone,
  shortHash,
  shortSha,
  stepAt,
} from "./tour.js";
import { $, state } from "./state.js";

const plateEl = $("receipt");

// The plate's entry: the order whose play or error the page holds, preferring the order the order
// island shows. Null when no receipt has a view yet.
function plateEntry() {
  const held = [...state.plantViews.entries()].filter(([, entry]) => entry.play || entry.error || entry.playError);
  if (held.length === 0) return null;
  const shown = state.view ? shownOrder(state.view)?.id : undefined;
  const [order, entry] = held.find(([id]) => id === shown) ?? held.at(-1);
  return { order, ...entry };
}

// Keeps the plate's playback to the order it shows. A different order starts from its first step,
// stopped, in the mode the page is in.
function syncOrder(order) {
  if (state.plate.order === order) return;
  stopTimer();
  state.app?.tour_stop();
  state.plate = { ...state.plate, order, index: 0, playing: false };
}

function stopTimer() {
  if (state.plate.timer !== null) clearTimeout(state.plate.timer);
  state.plate.timer = null;
}

function reducedMotion() {
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}

// Plays step `index`: the camera eases to it, unless the page is reduced-motion, where it jumps.
function go(index) {
  const entry = plateEntry();
  const step = stepAt(entry?.play, index);
  if (!step) return;
  state.plate.index = clampIndex(index, entry.play.steps.length);
  state.app.tour_go(step.machine_id, step.zoom, reducedMotion());
  requestRender();
  paintPlate();
}

function schedule() {
  stopTimer();
  if (!state.plate.playing) return;
  state.plate.timer = setTimeout(() => {
    state.plate.timer = null;
    const entry = plateEntry();
    const count = entry?.play?.steps.length ?? 0;
    const next = advance({ index: state.plate.index, playing: state.plate.playing }, count);
    if (!next.playing) {
      state.plate.playing = false;
      paintPlate();
      return;
    }
    go(next.index);
    schedule();
  }, STEP_MS);
}

export function playTour() {
  const entry = plateEntry();
  if (!entry?.play || entry.play.steps.length === 0) return;
  state.plate.mode = DIRECTED;
  const count = entry.play.steps.length;
  const start = state.plate.index >= count - 1 ? 0 : state.plate.index;
  state.plate.playing = true;
  go(start);
  schedule();
}

export function stopTour() {
  stopTimer();
  state.plate.playing = false;
  state.app?.tour_stop();
  paintPlate();
}

export function togglePlay() {
  if (state.plate.playing) stopTour();
  else playTour();
}

// A user step interrupts the move in progress, and a playing walk goes on from the new step.
export function stepBy(delta) {
  const entry = plateEntry();
  if (!entry?.play || state.plate.mode !== DIRECTED) return;
  go(state.plate.index + delta);
  if (state.plate.playing) schedule();
}

export function setMode(mode) {
  if (mode === state.plate.mode) return;
  if (mode === PLAIN) {
    stopTimer();
    state.plate.playing = false;
    state.app?.tour_stop();
  }
  state.plate.mode = mode;
  // Directed mode shows the step it is on, so the camera goes there as well.
  if (mode === DIRECTED && state.app && plateEntry()?.play) go(state.plate.index);
  requestRender();
  paintPlate();
}

export function toggleMode() {
  setMode(state.plate.mode === PLAIN ? DIRECTED : PLAIN);
}

function element(tag, text, className) {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (className) el.className = className;
  return el;
}

// Paints the plate from the held plays. Hidden until a receipt has a view or an error to show.
export function paintPlate() {
  const entry = plateEntry();
  plateEl.hidden = entry === null;
  if (entry === null) return;
  syncOrder(entry.order);
  const { play } = entry;
  const mode = state.plate.mode;
  const count = play?.steps.length ?? 0;
  const step = mode === DIRECTED ? stepAt(play, state.plate.index) : null;

  $("receipt-title").textContent = `Receipt ${play ? shortHash(play.receipt_hash) : ""}`.trim();
  $("receipt-fixture").hidden = !entry.fixture;
  $("receipt-meta").textContent = play
    ? `${play.order} · gate ${play.gate_state}`
    : `${entry.order} · ${entry.error ? "the plant view was not kept" : "no tour plays"}`;
  $("receipt-shas").textContent = play ? `base ${shortSha(play.base_sha)} → merged ${shortSha(play.merged_sha)}` : "";

  // The counts open under one summary line, so the item list keeps the height the plate has.
  $("receipt-counts-wrap").hidden = !play;
  $("receipt-counts-summary").textContent = play ? countSummary(play) : "counts";
  const counts = $("receipt-counts");
  counts.replaceChildren(...(play ? countLines(play).map((line) => element("li", line)) : []));

  const playBtn = $("tour-play");
  playBtn.textContent = state.plate.playing ? "stop" : "play";
  playBtn.disabled = !play || count === 0;
  $("tour-prev").disabled = mode !== DIRECTED || state.plate.index <= 0 || count === 0;
  $("tour-next").disabled = mode !== DIRECTED || state.plate.index >= count - 1 || count === 0;
  for (const [id, pressed] of [["mode-directed", mode === DIRECTED], ["mode-plain", mode === PLAIN]]) {
    const button = $(id);
    button.setAttribute("aria-pressed", String(pressed));
    button.disabled = id === "mode-directed" && !play;
  }
  $("tour-chip").textContent = chipText(mode, play);

  $("tour-progress").textContent = step
    ? `step ${state.plate.index + 1} of ${count} · ${step.item_id} · ${step.machine_id} · ${step.zoom}`
    : mode === PLAIN
      ? "plain card: every item below, no tour"
      : "";
  $("tour-caption").textContent = step ? step.caption : "";
  $("tour-stage").replaceChildren(...(step ? step.stage.map((stage) => element("li", stage.label)) : []));

  // A note says why the order plays; only a plant view that was not kept, or a tour that could
  // not be played at all, is an error.
  const note = play?.note || entry.playError || "";
  $("receipt-note").textContent = note;
  $("receipt-note").hidden = note === "";
  $("receipt-note").classList.toggle("error", !play && note !== "");

  const items = $("receipt-items");
  const rows = play ? play.items : (entry.items ?? []);
  items.replaceChildren(
    ...rows.map((item) => {
      const row = element("li", undefined, `item tone-${itemTone(item)}`);
      row.append(
        element("span", item.class.replaceAll("_", " "), "class"),
        element("span", item.state, "state"),
        element("span", item.item_id, "id"),
        element("span", item.machine_id, "machine"),
      );
      if (item.mandatory) row.append(element("span", "must reach", "must"));
      return row;
    }),
  );
  paintMore();
  if (entry.error) {
    $("receipt-note").hidden = false;
    $("receipt-note").classList.add("error");
    $("receipt-note").textContent = `The plant view was not kept: ${entry.error}`;
  }
}

// Counts the items below the list's visible rows, and says so with a fade and a line. The list
// scrolls inside the plate, so a row is never cut off at the plate's edge without a sign.
function paintMore() {
  const list = $("receipt-items");
  const bottom = list.scrollTop + list.clientHeight;
  const below = [...list.children].filter((row) => row.offsetTop >= bottom - 2).length;
  const more = $("receipt-more");
  more.hidden = below === 0;
  more.textContent = below === 0 ? "" : `+${below} more below, scroll the list`;
  list.classList.toggle("fade", below > 0);
}

// Wires the plate's buttons and keys. Called once at boot.
export function bindPlate() {
  $("receipt-items").addEventListener("scroll", paintMore);
  // The list's height follows the window's, so the count of rows below does too.
  window.addEventListener("resize", paintMore);
  $("tour-play").addEventListener("click", togglePlay);
  $("tour-prev").addEventListener("click", () => stepBy(-1));
  $("tour-next").addEventListener("click", () => stepBy(1));
  $("mode-directed").addEventListener("click", () => setMode(DIRECTED));
  $("mode-plain").addEventListener("click", () => setMode(PLAIN));
}
