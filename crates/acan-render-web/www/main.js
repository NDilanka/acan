// The Acan product screen. The wasm renderer draws the plant and folds the origin's log into a
// view. This file owns the frame loop, the canvas and key input, and boot. It draws only when
// something changed: input, a resize, new data, or a turn that is still easing. An idle page
// asks for no frames. Other modules import `requestRender` from here; the cycle is safe
// because no module runs code from another at load time.
import init, { App } from "./pkg/acan_render_web.js";
import { backendPlan, createOnFirst, replaceCanvas } from "./backend.js";
import {
  closeClashCard,
  closeQuestionCard,
  openCard,
  pressDigit,
  questionOpen,
  toggleCard,
  toggleQuestionCard,
} from "./cards.js";
import { closeCommand, commandOpen, openCommand } from "./commandbar.js";
import { clickMachine, closeCompose, composeOpen, orderHere } from "./compose.js";
import { keyAction } from "./keys.js";
import { droneHover, paintMarks, pinAt, placeLabels } from "./marks.js";
import { paintRide, paintStale, paintUsage, toggleHelp } from "./panels.js";
import { syncSeals } from "./seal.js";
import { paintSlip } from "./slip.js";
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
  replay,
  setCanvas,
  state,
} from "./state.js";
import { playOf, syncPlantViews } from "./plant.js";
import { bindPlate, paintPlate, stepBy, toggleMode, togglePlay } from "./plate.js";
import { startReplay } from "./replay.js";
import { bindShell, startShell } from "./shell.js";
import {
  clock,
  feed,
  forgetToken,
  get,
  hold,
  installFeed,
  isLive,
  loadEvents,
  onHoldEvent,
  setLink,
  showHold,
  start,
} from "./stream.js";
import { requestUsage } from "./usage.js";
import { goKeys, installLanes } from "./lanes.js";
import { installRailTabs } from "./rail.js";
import { installInbox } from "./inbox.js";
import { installHistory } from "./history.js";
import { installScrub } from "./scrub.js";
import { installFirstRun, paintFirstRun } from "./firstrun-panel.js";
import { onWorkers, refreshWorkers, workersAnswer } from "./workers.js";
import { closeViewer, installVision, placeVisionPins, syncVision, viewerOpen, viewerTakesEscape } from "./vision-pins.js";
import { installMulti, nameOf } from "./multi.js";
import { placePlates } from "./nameplates.js";
import { placeParked } from "./parked.js";
import { canvasState } from "./projects.js";

const ZOOM_KEY = 1.25;

const WHEEL_RATE = 0.0015;

const TURN_DEG = 45;

// A press that moves less than this is a click, and a click within PIN_PICK_PX of a clash pin's
// tip opens its card.
const CLICK_PX = 4;

const DRONE_PICK_PX = 6;

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

const timing = { started: performance.now() };

// The registration point for the watch panels (lanes, inbox, history, scrubber). Each paint
// hook runs when the view changes. The keys are all in keys.js, so they have one order; a key hook
// is asked only about a key the table does not use, and returning true takes it. A panel that
// answers a key the table names sets its action on the kit instead: `rideLane(index)` for G and
// a digit, `goLive()` for Esc on a rewound map.
const paintHooks = [];
const keyHooks = [];
const kit = {
  state,
  feed,
  clock,
  isLive,
  freeze,
  replay,
  hold,
  loadEvents,
  onHoldEvent,
  showHold,
  forgetToken,
  get,
  setLink,
  requestRender,
  // The shared GET /api/workers answer, which the first-run checklist reads too.
  workers: { answer: workersAnswer, refresh: refreshWorkers, onChange: onWorkers },
  onPaint: (hook) => paintHooks.push(hook),
  onKey: (hook) => keyHooks.push(hook),
};

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
    placeVisionPins();
    // The parked names are placed before the plates, which move clear of them.
    placeParked();
    placePlates();
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
  paintMarks();
  paintUsage();
  paintStale();
  syncPlantViews(state, get, paintPlate);
  paintPlate();
  syncSeals(state, get, paintSlip);
  paintSlip();
  requestUsage(state, get, paintUsage);
  syncVision();
  paintFirstRun();
  for (const hook of paintHooks) hook();
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
    if (state.drag === null) return void droneHover(event.clientX - canvas.getBoundingClientRect().left, event.clientY - canvas.getBoundingClientRect().top);
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

  // A press that did not move is a click, and the camera stays where it is. A click on a clash
  // pin opens its card. A click on a drone rides it, or switches the ride to the drone nearest the
  // click. A click on a machine opens the compose panel there, to post an order on it.
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
    } else {
      clickMachine(x, y);
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

function setAlt(down) {
  if (state.altDown === down) return;
  state.altDown = down;
  labelsEl.classList.toggle("alt", down);
  paintUsage();
  // The chips and the landed dots change their words with Alt, so the flags are redrawn.
  if (state.view !== null) paintMarks();
  requestRender();
}

// Whether the key went to a text field, which keeps its keys.
function typingIn(target) {
  return target instanceof Element && (target.matches("input, textarea, select") || target.isContentEditable);
}

// Runs the action keys.js named for a key.
function runKey(action, event) {
  switch (action.kind) {
    case "arm-go":
      break;
    case "ride-lane":
      kit.rideLane?.(action.index);
      break;
    case "go":
      goKeys.get(action.letter)?.();
      break;
    case "close":
      CLOSERS[action.panel]();
      break;
    case "go-live":
      kit.goLive?.();
      break;
    case "leave-ride":
      state.app.leave();
      requestRender();
      break;
    case "open-command":
      openCommand();
      break;
    case "toggle-help":
      toggleHelp();
      break;
    case "toggle-question":
      toggleQuestionCard();
      break;
    case "order-here":
      orderHere();
      break;
    case "toggle-card":
      toggleCard();
      break;
    case "digit":
      pressDigit(action.n);
      break;
    case "pan":
      state.app.pan(action.dx, action.dy);
      requestRender();
      break;
    case "projection":
      state.app.toggle_projection();
      requestRender();
      break;
    case "zoom":
      zoomAtCentre(action.dir > 0 ? ZOOM_KEY : 1 / ZOOM_KEY);
      break;
    case "fit":
      state.app.fit();
      requestRender();
      break;
    case "ride-centre": {
      const rect = canvas.getBoundingClientRect();
      state.app.ride_nearest(rect.width / 2, rect.height / 2);
      requestRender();
      break;
    }
    case "ride-step":
      state.app.ride_step(action.dir);
      requestRender();
      break;
    case "turn":
      turn(action.dir * TURN_DEG);
      break;
    case "play":
      togglePlay();
      break;
    case "mode":
      toggleMode();
      break;
    case "step":
      stepBy(action.delta);
      break;
    default:
      console.warn("no handler for key action", action.kind, event.key);
  }
}

const CLOSERS = {
  command: () => closeCommand(),
  compose: closeCompose,
  viewer: closeViewer,
  question: closeQuestionCard,
  clash: closeClashCard,
};

// When G was pressed, while the chord waits for its digit or letter.
let goArmed = null;

function bindKeys() {
  window.addEventListener("keydown", (event) => {
    // A text field keeps its keys, except that Esc in a field of the vision viewer closes it.
    if (typingIn(event.target) && !(event.key === "Escape" && viewerTakesEscape(event))) return;
    if (event.key === "Alt") {
      event.preventDefault();
      setAlt(true);
      return;
    }
    // A modal dialog, such as the key setup, keeps its own keys and its own Esc.
    if (document.querySelector("dialog[open]") !== null) return;
    if (state.app === null || event.ctrlKey || event.metaKey) return;
    const step = keyAction(
      { key: event.key, code: event.code, repeat: event.repeat, shift: event.shiftKey },
      {
        armed: goArmed,
        now: performance.now(),
        letters: goKeys,
        held: hold.on,
        riding: state.ridden !== "",
        open: {
          command: commandOpen(),
          compose: composeOpen(),
          viewer: viewerOpen(),
          question: questionOpen(),
          clash: Boolean(state.markState.card),
        },
      },
    );
    goArmed = step.armed;
    if (step.prevent) event.preventDefault();
    if (step.action !== null) {
      runKey(step.action, event);
      return;
    }
    keyHooks.some((hook) => hook(event));
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
    startShell();
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
  const plan = backendPlan(params.get("backend"), Boolean(navigator.gpu));
  const made = await createOnFirst(plan, canvas, {
    create: (target, backend) => App.create(target, backend),
    freshCanvas: (old) => {
      setCanvas(replaceCanvas(old));
      setDrawingSize();
      return canvas;
    },
    onFail: (backend, err) => console.warn(`${backend} renderer failed:`, err),
  });
  console.info(`renderer: ${made.backend} (${made.app.backend()})`);
  return made.app;
}

// The page loads its plant one of four ways. `?replay=1` plays a recorded run from `replay/`,
// with no server, and holds no token. `?plant=stress&machines=N` builds a generated plant for the
// benchmark. `?plant=fixture` loads the demo plant with no log. Otherwise the page reads the live
// origin, which needs the owner token.
async function loadPlant() {
  // The token is adopted in every other mode, so the shell's calls can use it, even over a fixture.
  state.token = replay ? null : adoptToken();
  const source = params.get("plant");
  if (replay) {
    await startReplay();
  } else if (source === "stress") {
    const machines = Number(params.get("machines") || 2000);
    const started = performance.now();
    state.app.set_stress(machines);
    timing.load = performance.now() - started;
  } else if (source === "fixture") {
    const response = await fetch("fixtures/demo-plant.json");
    if (!response.ok) throw new Error(`fixture HTTP ${response.status}`);
    state.app.set_plant(await response.text());
  } else {
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
  repoEl.textContent = canvasState.multi ? nameOf(canvasState.active) : state.app.repo_name();
}

function followReducedMotion() {
  state.app.set_bob(!reducedMotion.matches);
  requestRender();
}

const PLATE_FIXTURES = {
  fixture: "fixtures/plant-view-v1.json",
  plain: "fixtures/plant-view-v1-plain.json",
};

// Shows the receipt plate over a fixture plant view, marked FIXTURE, as if the origin had served it.
async function loadPlateFixture(name) {
  const file = PLATE_FIXTURES[name];
  if (!file) throw new Error(`no plate fixture named ${name}`);
  const response = await fetch(file);
  if (!response.ok) throw new Error(`fixture HTTP ${response.status}`);
  const text = await response.text();
  const order = JSON.parse(text).order.id;
  const { play, playError, items } = playOf(state.app, text);
  const summary = JSON.parse(state.app.plant_view_summary(text));
  state.plantViews.set(order, { recorded: -1, summary, error: null, play, playError, items, fixture: true });
  paintPlate();
}

async function boot() {
  const started = performance.now();
  await init();
  timing.init = performance.now() - started;
  setDrawingSize();
  const createStarted = performance.now();
  state.app = await createApp();
  installFeed(state.app);
  timing.create = performance.now() - createStarted;
  sizeCanvas();
  bindCanvas();
  bindKeys();
  installRailTabs(kit);
  installLanes(kit);
  installInbox(kit);
  installHistory(kit);
  installScrub(kit);
  installMulti(kit);
  window.addEventListener("resize", sizeCanvas);
  state.app.set_still(freeze);
  // Reduced motion turns the hover bob off, and the page follows the setting as it changes.
  followReducedMotion();
  reducedMotion.addEventListener("change", followReducedMotion);
  await loadPlant();
  bindPlate();
  await installVision();
  // The replay page makes no API call, so the shell (version, update, bug report, setup) stays off.
  if (!replay) {
    bindShell();
    startShell();
    installFirstRun();
  }
  // `?plate=fixture` or `?plate=plain` shows the receipt plate over a fixture plant view, without
  // the origin. The plate is watermarked FIXTURE.
  if (params.has("plate")) await loadPlateFixture(params.get("plate"));
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
