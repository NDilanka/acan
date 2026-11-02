// The compose panel: an order written where it happens. A click on a machine, or O, opens it
// beside that machine; one sentence and Enter post the order there. A dashed ring marks the spot
// the order will be pinned on while the panel is open, and once the origin's view carries the new
// order the ring pulses out from its machine. The panel follows its machine as the camera moves.
import { requestRender } from "./main.js";
import {
  centreMachine,
  composeSpot,
  machineAt,
  machineCentre,
  machineContext,
  orderCommand,
  orderRefusal,
  postedOrder,
} from "./order.js";
import { closeViewer } from "./vision-pins.js";
import { $, canvas, freeBand, freeze, replay, state } from "./state.js";
import { historyBlock } from "./guard.js";
import { forgetToken, hold } from "./stream.js";

// An accepted order's panel shows its line this long, then closes.
const SENT_HOLD_MS = 900;

// A pulse is kept this long, the length of its CSS animation.
const PULSE_MS = 1900;

// The label's centre sits this far under its anchor, as marks.js hangs it.
const LABEL_CENTRE_DY = 13;

// The open panel, `{ machine, sending, note, bad, returnFocus }`, or null.
let compose = null;

// The machine the last order or go named, so O opens there again.
let lastMachine = null;

// The order an accepted PostOrder minted, until the view carries it: `{ order, machine }`.
let awaited = null;

// The pulses showing, each `{ machine, el }`.
let pulses = [];

let holdTimer = null;

let bound = false;

const project = (x, y) => state.app.world_to_screen(x, y);

function machines() {
  return state.view === null ? [] : state.view.machines;
}

function byName(name) {
  return machines().find((machine) => machine.name === name);
}

export function composeOpen() {
  return compose !== null;
}

// The machine under a click on the canvas opens the panel there. A click on open ground closes a
// panel that holds no words, and keeps one that does.
export function clickMachine(x, y) {
  const labels = shownLabels();
  const name = machineAt(machines(), project, x, y, labels);
  if (name !== null) {
    openCompose(name);
  } else if (compose !== null && $("compose-sentence").value.trim() === "") {
    closeCompose();
  }
}

// O: the panel opens on the machine already open, the ridden agent's machine, the machine last
// ordered on, or else the machine nearest the middle of the screen.
export function orderHere() {
  const riderMachine = state.ridden === ""
    ? null
    : state.view?.agents.find((agent) => agent.id === state.ridden)?.machine ?? null;
  const name = compose?.machine
    ?? riderMachine
    ?? (lastMachine !== null && byName(lastMachine) ? lastMachine : null)
    ?? centreMachine(machines(), project, canvas.clientWidth, canvas.clientHeight);
  if (name === null) return false;
  openCompose(name);
  return true;
}

// Remembers a machine the page went to, so O orders there.
export function noteMachine(name) {
  if (byName(name)) lastMachine = name;
}

// Opens the panel on `name`. A draft already written moves with the panel to the new machine.
export function openCompose(name) {
  if (state.view === null || !byName(name)) return;
  bind();
  // One panel opens beside the map at a time, so the compose panel never lands on the viewer.
  closeViewer();
  const returnFocus = compose?.returnFocus ?? document.activeElement;
  compose = { machine: name, sending: false, note: "", bad: false, returnFocus };
  lastMachine = name;
  clearTimeout(holdTimer);
  paintCompose();
  $("compose").hidden = false;
  placeCompose();
  $("compose-sentence").focus();
  requestRender();
}

export function closeCompose() {
  if (compose === null) return;
  const { returnFocus } = compose;
  compose = null;
  clearTimeout(holdTimer);
  $("compose").hidden = true;
  $("compose-target").hidden = true;
  const back = returnFocus instanceof HTMLElement && returnFocus.isConnected && returnFocus !== document.body
    ? returnFocus
    : canvas;
  back.focus({ preventScroll: true });
  requestRender();
}

function bind() {
  if (bound) return;
  bound = true;
  $("compose-form").addEventListener("submit", (event) => {
    event.preventDefault();
    send();
  });
  $("compose-sentence").addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      closeCompose();
    }
  });
  $("compose-close").addEventListener("click", closeCompose);
}

function paintCompose() {
  if (compose === null) return;
  $("compose-title").textContent = `Order on ${compose.machine}`;
  $("compose-context").textContent = machineContext(state.view, compose.machine);
  $("compose-send").textContent = compose.sending ? "Sending…" : "Send";
  const blocked = historyBlock("send", hold.on);
  $("compose-send").disabled = compose.sending || blocked !== null;
  $("compose-send").title = blocked ?? "";
  $("compose-sentence").readOnly = compose.sending;
  $("compose-note").textContent = blocked ?? compose.note;
  $("compose-note").classList.toggle("bad", blocked === null && compose.bad);
}

async function send() {
  if (compose === null || compose.sending) return;
  if (historyBlock("send", hold.on) !== null) {
    paintCompose();
    return;
  }
  const input = $("compose-sentence");
  const built = orderCommand(compose.machine, input.value);
  if (!built.ok) {
    compose.note = built.line;
    compose.bad = true;
    paintCompose();
    input.focus();
    return;
  }
  if (replay) {
    compose.note = "The replay is read-only, so nothing was sent.";
    compose.bad = true;
    paintCompose();
    return;
  }
  if (!state.token) {
    compose.note = "Sign in with the owner token first. Nothing was sent.";
    compose.bad = true;
    paintCompose();
    return;
  }
  const machine = compose.machine;
  compose.sending = true;
  compose.note = "";
  compose.bad = false;
  paintCompose();
  let line;
  let accepted = false;
  try {
    const res = await fetch("/api/commands", {
      method: "POST",
      headers: { Authorization: `Bearer ${state.token}`, "Content-Type": "application/json" },
      body: JSON.stringify(built.command),
    });
    if (!res.ok) {
      line = orderRefusal(res.status, await res.text());
      if (res.status === 401) forgetToken();
    } else {
      const events = await res.json();
      const order = postedOrder(events);
      accepted = true;
      line = order === null ? `Posted on ${machine}.` : `Posted o-${order} on ${machine}.`;
      awaited = { order, machine };
      // The frozen page does not hear the stream, so it folds the order's own events here.
      if (freeze) {
        for (const event of events) state.app.ingest(JSON.stringify(event));
      }
      if (order === null) arrived();
      requestRender();
    }
  } catch {
    line = "The order did not reach the server. Try again.";
  }
  announce(line);
  if (compose === null || compose.machine !== machine) return;
  compose.sending = false;
  compose.note = line;
  compose.bad = !accepted;
  if (accepted) {
    input.value = "";
    holdTimer = setTimeout(closeCompose, SENT_HOLD_MS);
  }
  paintCompose();
  if (!accepted) input.focus();
}

// The view changed. Once it carries the order the panel posted, the order pulses at its machine.
// A panel whose machine left the plant closes, and an open one refreshes what its machine holds.
export function syncCompose() {
  if (state.view === null) return;
  if (awaited !== null && state.view.orders.some((order) => order.id === awaited.order)) arrived();
  if (compose !== null) {
    if (byName(compose.machine)) paintCompose();
    else closeCompose();
  }
}

function arrived() {
  const { order, machine } = awaited;
  awaited = null;
  pulse(machine);
  if (order !== null) announce(`o-${order} is on the map at ${machine}.`);
}

// A ring that spreads out from the machine and fades, twice. Reduced motion shows a still ring
// that fades instead.
export function pulse(machine) {
  const el = document.createElement("div");
  el.className = "pulse sent";
  $("pulses").append(el);
  pulses.push({ machine, el });
  setTimeout(() => {
    el.remove();
    pulses = pulses.filter((item) => item.el !== el);
  }, PULSE_MS);
  requestRender();
}

// Places the panel beside its machine, the target ring on the machine, and each pulse, as the
// camera stands this frame. Called after the labels are placed.
export function placeCompose() {
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  for (const item of pulses) {
    const machine = byName(item.machine);
    if (machine === undefined) continue;
    placeRing(item.el, machineCentre(machine, project));
  }
  const target = $("compose-target");
  if (compose === null) {
    target.hidden = true;
    return;
  }
  const machine = byName(compose.machine);
  if (machine === undefined) return;
  const at = machineCentre(machine, project);
  target.hidden = false;
  placeRing(target, at);
  const panel = $("compose");
  const band = freeBand();
  const spot = composeSpot(at, panel.offsetWidth, panel.offsetHeight, band.right, height, band.left);
  panel.style.left = `${spot.left}px`;
  panel.style.top = `${spot.top}px`;
  panel.dataset.side = spot.side;
}

function placeRing(el, at) {
  el.style.left = `${at.x}px`;
  el.style.top = `${at.y}px`;
}

// The machine names shown now, at the centre of each name, for a click near a name.
function shownLabels() {
  const quads = state.app.label_layout(state.altDown);
  const labels = [];
  for (let k = 0; k < quads.length; k += 4) {
    const el = state.labels.get(quads[k]);
    if (el === undefined || el.style.visibility !== "visible") continue;
    labels.push({ name: state.app.machine_name(quads[k]), x: quads[k + 1], y: quads[k + 2] + LABEL_CENTRE_DY });
  }
  return labels;
}

// One line for a screen reader: what the page just did.
export function announce(text) {
  const live = $("direct-live");
  // Clearing the line first makes a screen reader read it again when the words repeat.
  live.textContent = "";
  requestAnimationFrame(() => {
    live.textContent = text;
  });
}
