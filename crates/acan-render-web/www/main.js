// The Acan product screen. The wasm renderer draws the plant and folds the origin's log into a
// view. This file owns the frame loop, the canvas and key input, and boot. It draws only when
// something changed: input, a resize, new data, or a turn that is still easing. An idle page
// asks for no frames. Other modules import `requestRender` from here; the cycle is safe
// because no module runs code from another at load time.
import init, { App } from "./pkg/acan_render_web.js";
import { openCard, settleByKey, toggleCard } from "./cards.js";
import { paintMarks, pinAt, placeLabels } from "./marks.js";
import { paintOrderLine, paintRide, paintStale, paintUsage } from "./panels.js";
import {
  $,
  TOKEN_KEY,
  canvas,
  freeze,
  gate,
  labelsEl,
  noticeEl,
  params,
  repoEl,
  state,
} from "./state.js";
import { setLink, start } from "./stream.js";

const PAN_KEY_PX = 80;

const ZOOM_KEY = 1.25;

const WHEEL_RATE = 0.0015;

const TURN_DEG = 45;

// A press that moves less than this is a click, and a click within PIN_PICK_PX of a clash pin's
// tip opens its card.
const CLICK_PX = 4;

const DRONE_PICK_PX = 6;

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

const timing = { started: performance.now() };

export function requestRender() {
  if (state.scheduled || state.app === null) return;
  state.scheduled = true;
  requestAnimationFrame(drawFrame);
}

// `now` is the frame time. The bench calls this with no argument, which reads as a frame with
// no time elapsed on a turn.
function drawFrame(now = performance.now()) {
  state.scheduled = false;
  const elapsed = state.lastFrame === null ? 0 : now - state.lastFrame;
  state.lastFrame = now;
  try {
    const turning = state.app.advance(elapsed);
    // A landed dot going quiet or a fade ending changes the flag list, so the flags are redrawn.
    if (state.app.take_flags_changed()) paintMarks();
    if (!state.app.render()) requestRender();
    if (timing.firstFrame === undefined) timing.firstFrame = performance.now();
    syncView();
    paintRide();
    placeLabels();
    if (turning) {
      requestRender();
    } else {
      state.lastFrame = null;
    }
  } catch (err) {
    console.error(err);
  }
}

// The view is rebuilt by the wasm side after the log changes. The panels are redrawn only when
// the version moves, so a camera move never rebuilds them.
function syncView() {
  const version = state.app.view_version();
  if (version === 0 || version === state.renderedVersion) return;
  state.renderedVersion = version;
  state.view = JSON.parse(state.app.view_json());
  paint();
  if (state.povWanted !== null && state.app.ride(state.povWanted)) state.povWanted = null;
  // The marks ease in from where they start, so the next frame must advance them.
  requestRender();
  if (state.socket) setLink(`live · seq ${state.app.next_seq()}`);
}

function paint() {
  paintOrderLine();
  paintMarks();
  paintUsage();
  paintStale();
}

// Sets the drawing buffer to the CSS box times the device pixel ratio. The wasm side rounds
// the same product, so the two sizes agree.
function setDrawingSize() {
  const rect = canvas.getBoundingClientRect();
  const dpr = window.devicePixelRatio || 1;
  canvas.width = Math.max(1, Math.round(rect.width * dpr));
  canvas.height = Math.max(1, Math.round(rect.height * dpr));
  return { rect, dpr };
}

function sizeCanvas() {
  const { rect, dpr } = setDrawingSize();
  if (state.app !== null) {
    state.app.resize(rect.width, rect.height, dpr);
    requestRender();
  }
}

function showNotice(text) {
  noticeEl.textContent = text;
  noticeEl.hidden = false;
}

function zoomAtCentre(factor) {
  const rect = canvas.getBoundingClientRect();
  state.app.zoom_at(rect.width / 2, rect.height / 2, factor);
  requestRender();
}

function turn(degrees) {
  state.app.turn(degrees);
  requestRender();
}

// Whether a drone's disc lies under the CSS point, within its radius plus DRONE_PICK_PX.
function droneAt(x, y) {
  const discs = state.app.drone_layout();
  for (let k = 0; k < discs.length; k += 3) {
    if (Math.hypot(discs[k] - x, discs[k + 1] - y) <= discs[k + 2] + DRONE_PICK_PX) return true;
  }
  return false;
}

// Left drag moves the ground under the pointer, so the point you grab stays under the cursor.
// Right drag, or Alt with left drag, orbits and tilts about the ground point under the screen
// centre.
function bindCanvas() {
  canvas.addEventListener("contextmenu", (event) => event.preventDefault());

  canvas.addEventListener("pointerdown", (event) => {
    if (state.app === null || (event.button !== 0 && event.button !== 2)) return;
    canvas.setPointerCapture(event.pointerId);
    state.drag = {
      x: event.clientX,
      y: event.clientY,
      orbit: event.button === 2 || event.altKey,
    };
    state.press = event.button === 0 ? { x: event.clientX, y: event.clientY } : null;
    canvas.classList.add("dragging");
  });

  canvas.addEventListener("pointermove", (event) => {
    if (state.drag === null) return;
    if (state.drag.orbit) {
      state.app.orbit(event.clientX - state.drag.x, event.clientY - state.drag.y);
    } else {
      const rect = canvas.getBoundingClientRect();
      state.app.drag(
        state.drag.x - rect.left,
        state.drag.y - rect.top,
        event.clientX - rect.left,
        event.clientY - rect.top,
      );
    }
    state.drag.x = event.clientX;
    state.drag.y = event.clientY;
    requestRender();
  });

  // A press that did not move is a click. A click near a clash pin's tip opens its card; the
  // camera stays where it is.
  // A click on a clash pin opens its card. A click on a drone rides it, or switches the ride to
  // the drone nearest the click.
  const endDrag = (event) => {
    const pressed = state.press;
    state.press = null;
    state.drag = null;
    canvas.classList.remove("dragging");
    if (event.type !== "pointerup" || pressed === null) return;
    if (Math.hypot(event.clientX - pressed.x, event.clientY - pressed.y) >= CLICK_PX) return;
    const rect = canvas.getBoundingClientRect();
    const x = event.clientX - rect.left;
    const y = event.clientY - rect.top;
    const clash = pinAt(x, y);
    if (clash !== null) {
      openCard(clash);
    } else if (droneAt(x, y)) {
      state.app.ride_nearest(x, y);
      requestRender();
    }
  };
  canvas.addEventListener("pointerup", endDrag);
  canvas.addEventListener("pointercancel", endDrag);

  canvas.addEventListener(
    "wheel",
    (event) => {
      event.preventDefault();
      if (state.app === null) return;
      const rect = canvas.getBoundingClientRect();
      const lines = event.deltaMode === 1 ? 16 : 1;
      const factor = Math.exp(-event.deltaY * lines * WHEEL_RATE);
      // Wheel up brings the drone closer: the distance scale shrinks as the wheel turns up.
      if (state.ridden !== "") state.app.pov_scale(1 / factor);
      else state.app.zoom_at(event.clientX - rect.left, event.clientY - rect.top, factor);
      requestRender();
    },
    { passive: false },
  );
}

const ARROW_PAN = {
  ArrowLeft: [PAN_KEY_PX, 0],
  ArrowRight: [-PAN_KEY_PX, 0],
  ArrowUp: [0, PAN_KEY_PX],
  ArrowDown: [0, -PAN_KEY_PX],
};

function setAlt(down) {
  if (state.altDown === down) return;
  state.altDown = down;
  labelsEl.classList.toggle("alt", down);
  paintUsage();
  // The chips and the landed dots change their words with Alt, so the flags are redrawn.
  if (state.view !== null) paintMarks();
  requestRender();
}

function bindKeys() {
  window.addEventListener("keydown", (event) => {
    if (event.target instanceof HTMLInputElement) return;
    if (event.key === "Alt") {
      event.preventDefault();
      setAlt(true);
      return;
    }
    if (state.app === null || event.ctrlKey || event.metaKey) return;
    if (ARROW_PAN[event.key]) {
      event.preventDefault();
      state.app.pan(...ARROW_PAN[event.key]);
      requestRender();
      return;
    }
    if (event.key === "5" || event.code === "Numpad5") {
      if (!event.repeat) {
        state.app.toggle_projection();
        requestRender();
      }
      return;
    }
    if (/^[1-3]$/.test(event.key)) {
      settleByKey(Number(event.key));
      return;
    }
    switch (event.key) {
      case "c":
      case "C":
        if (!event.repeat) toggleCard();
        break;
      case "+":
      case "=":
        zoomAtCentre(ZOOM_KEY);
        break;
      case "-":
      case "_":
        zoomAtCentre(1 / ZOOM_KEY);
        break;
      case "0":
        state.app.fit();
        requestRender();
        break;
      case "f":
      case "F":
        if (!event.repeat) {
          const rect = canvas.getBoundingClientRect();
          state.app.ride_nearest(rect.width / 2, rect.height / 2);
          requestRender();
        }
        break;
      case "Tab":
        event.preventDefault();
        state.app.ride_step(event.shiftKey ? -1 : 1);
        requestRender();
        break;
      case "Escape":
        if (state.ridden !== "") {
          state.app.leave();
          requestRender();
        }
        break;
      case "q":
      case "Q":
        if (!event.repeat) turn(-TURN_DEG);
        break;
      case "e":
      case "E":
        if (!event.repeat) turn(TURN_DEG);
        break;
      default:
    }
  });

  window.addEventListener("keyup", (event) => {
    if (event.key === "Alt") setAlt(false);
  });
  window.addEventListener("blur", () => setAlt(false));

  $("token-form").addEventListener("submit", (event) => {
    event.preventDefault();
    const value = $("token").value.trim();
    $("token").value = "";
    if (!value) return;
    sessionStorage.setItem(TOKEN_KEY, value);
    state.token = value;
    gate.hidden = true;
    start().catch((err) => {
      console.error(err);
      setLink("failed");
    });
  });
}

// The owner token arrives as `#token=` from `acan serve`. It is kept for the session only, then
// the fragment is removed so it does not sit in the address bar or the history.
function adoptToken() {
  const fragment = new URLSearchParams(location.hash.slice(1)).get("token");
  if (fragment) {
    sessionStorage.setItem(TOKEN_KEY, fragment);
    history.replaceState(null, "", location.pathname + location.search);
  }
  return sessionStorage.getItem(TOKEN_KEY);
}

async function createApp() {
  const backend = params.get("backend") || (navigator.gpu ? "webgpu" : "webgl");
  try {
    return await App.create(canvas, backend);
  } catch (err) {
    if (backend === "webgl") throw err;
    console.error(err);
    return App.create(canvas, "webgl");
  }
}

// The page loads its plant one of three ways. `?plant=stress&machines=N` builds a generated
// plant for the benchmark. `?plant=fixture` loads the demo plant with no log. Otherwise the
// page reads the live origin, which needs the owner token.
async function loadPlant() {
  const source = params.get("plant");
  if (source === "stress") {
    const machines = Number(params.get("machines") || 2000);
    const started = performance.now();
    state.app.set_stress(machines);
    timing.load = performance.now() - started;
  } else if (source === "fixture") {
    const response = await fetch("fixtures/demo-plant.json");
    if (!response.ok) throw new Error(`fixture HTTP ${response.status}`);
    state.app.set_plant(await response.text());
  } else {
    state.token = adoptToken();
    if (!state.token) {
      gate.hidden = false;
      setLink("sign in");
      return;
    }
    try {
      await start();
    } catch (err) {
      console.error(err);
      setLink("failed");
    }
  }
  repoEl.textContent = state.app.repo_name();
}

function followReducedMotion() {
  state.app.set_bob(!reducedMotion.matches);
  requestRender();
}

async function boot() {
  const started = performance.now();
  await init();
  timing.init = performance.now() - started;
  setDrawingSize();
  const createStarted = performance.now();
  state.app = await createApp();
  timing.create = performance.now() - createStarted;
  sizeCanvas();
  bindCanvas();
  bindKeys();
  window.addEventListener("resize", sizeCanvas);
  state.app.set_still(freeze);
  // Reduced motion turns the hover bob off, and the page follows the setting as it changes.
  followReducedMotion();
  reducedMotion.addEventListener("change", followReducedMotion);
  await loadPlant();
  // `?view=azimuth,elevation` turns to a camera angle in degrees, such as a low tilt.
  if (params.has("view")) {
    const [azimuth, elevation] = params.get("view").split(",").map(Number);
    state.app.view(azimuth, elevation);
  }
  // `?focus=agent-a&zoom=8` closes up on one agent's drone, at 8 times the fit zoom. The zoom is
  // the scale at the drone, so 8 draws it about 375 px tall. It applies once the log places the
  // drone, so the page can ask before the log arrives.
  if (params.has("focus")) {
    state.app.focus_agent(params.get("focus"), Number(params.get("zoom") || 8));
  }
  requestRender();
  if (params.get("bench") === "1") {
    const { installBench } = await import("./bench.js");
    installBench({ App, app: state.app, canvas, timing, redraw: drawFrame, setAlt });
  }
}

boot().catch((error) => {
  console.error(error);
  showNotice(String(error?.message ?? error));
});
