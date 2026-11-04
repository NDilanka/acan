// The replay page. A recorded run is read from `replay/` beside the page: `manifest.json`,
// `plant.json` and `events.json`, as `acan export-replay` writes them. The log folds one event at
// a time on the recorded clock, with a play or pause button and a progress bar. The page makes no API call, opens
// no socket and sends no command.
import { requestRender } from "./main.js";
import { noteLogEntry, readStateMachines, resetLight, setStateMachines } from "./statemachine.js";
import { $, state } from "./state.js";
import { setLink } from "./stream.js";

const DIR = "replay/";

// The shortest and longest wait between two events on screen, in milliseconds. A long pause in
// the recording does not stall the page, and a burst of events stays readable.
const MIN_GAP_MS = 25;
const MAX_GAP_MS = 1200;

// The gap divisor; the bar keeps one speed.
const SPEEDS = [1];

const playback = { events: [], next: 0, playing: false, timer: null };

const panel = $("replay");
const progressEl = $("replay-progress");
const toggleEl = $("replay-toggle");

toggleEl.addEventListener("click", togglePlay);

async function fetchText(name) {
  const res = await fetch(DIR + name);
  if (!res.ok) throw new Error(`replay/${name} answered ${res.status}`);
  return res.text();
}

// Loads the recording and starts it. The manifest comes first, so a page built for another
// format says so instead of misreading the files. `machines`, when given, stands in for the
// plant's own state machines, so they are in place before the first event folds.
export async function startReplay(machines = null) {
  const manifest = JSON.parse(await fetchText("manifest.json"));
  if (manifest.format !== 1) {
    throw new Error(`this page reads replay format 1, not ${manifest.format}`);
  }
  const events = JSON.parse(await fetchText("events.json"));
  if (events.length === 0) throw new Error("the recording holds no events");
  const plant = await fetchText("plant.json");
  state.app.set_plant(plant);
  setStateMachines(machines ?? readStateMachines(plant));
  playback.events = events;
  panel.hidden = false;
  setLink("replay");
  restart();
}

// The log must open with the world's first event, so the page folds that one first and then
// follows the rest on the recorded clock.
function restart() {
  clearTimer();
  state.app.load_log(JSON.stringify(playback.events.slice(0, 1)));
  resetLight();
  noteLogEntry(playback.events[0]);
  playback.next = 1;
  setPlaying(true);
}

function togglePlay() {
  if (playback.next >= playback.events.length) {
    restart();
  } else {
    setPlaying(!playback.playing);
  }
}

function setPlaying(on) {
  clearTimer();
  playback.playing = on;
  if (on) {
    step();
  } else {
    paint();
  }
}

function clearTimer() {
  clearTimeout(playback.timer);
  playback.timer = null;
}

// Folds the next event, then waits for the gap to the one after it.
function step() {
  playback.timer = null;
  if (!playback.playing) return;
  const event = playback.events[playback.next];
  if (event === undefined) {
    finish();
    return;
  }
  try {
    state.app.ingest(JSON.stringify(event));
  } catch (err) {
    console.error(err);
    playback.playing = false;
    setLink(`replay stopped at seq ${event.seq}`);
    paint();
    return;
  }
  noteLogEntry(event);
  playback.next += 1;
  requestRender();
  const following = playback.events[playback.next];
  if (following === undefined) {
    finish();
    return;
  }
  playback.timer = setTimeout(step, waitMs(event, following));
  paint();
}

function finish() {
  playback.playing = false;
  paint();
  requestRender();
}

// The pause before the next event: the recorded gap, divided by the speed and kept in range.
function waitMs(event, following) {
  const gap = (following.at - event.at) / SPEEDS[0];
  if (!Number.isFinite(gap)) return MIN_GAP_MS;
  return Math.min(MAX_GAP_MS, Math.max(MIN_GAP_MS, gap));
}

function paint() {
  const total = playback.events.length;
  const done = playback.next >= total;
  toggleEl.textContent = playback.playing ? "\u23F8" : "\u25B6";
  toggleEl.setAttribute("aria-label", done ? "Play again" : playback.playing ? "Pause" : "Play");
  progressEl.max = String(total);
  progressEl.value = String(playback.next);
}
