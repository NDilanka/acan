// The first-run checklist on the page: an island at the top right that walks a new tester from
// sign-in to watching the map, until every step is done or they dismiss it. What each step says is decided in firstrun.js.
// It reads GET /api/setup and GET /api/plant itself, with its own fetch, so a server without a
// route (404) or a refused read never signs the tester out. GET /api/workers is read once for the
// page, in workers.js, so the lanes strip says the same.
import { $, gate, params, replay, state } from "./state.js";
import { DONE, FIRSTRUN_KEY, UNAVAILABLE, UNKNOWN, allDone, contractOf, progressShort, shouldShow, steps } from "./firstrun.js";
import { isLive } from "./stream.js";
import { loadWorkers, onWorkers, refreshWorkers, workersAnswer } from "./workers.js";

const panel = $("firstrun");

let setup = null;
// undefined until the plant is asked, then null (the server did not say) or `{ present, version }`.
let contract;
let checking = false;
let copied = false;
// The tester's own open or closed choice, or null while the checklist rests folded to one line.
let collapsedByUser = null;

function dismissed() {
  try {
    return localStorage.getItem(FIRSTRUN_KEY) === "done";
  } catch {
    return false;
  }
}

function remember() {
  try {
    localStorage.setItem(FIRSTRUN_KEY, "done");
  } catch {
    // A page that cannot store it shows the checklist again on the next load.
  }
}

// Starts the checklist. `?firstrun=1` shows it again after it was dismissed.
export function installFirstRun() {
  if (params.get("firstrun") === "1") {
    try {
      localStorage.removeItem(FIRSTRUN_KEY);
    } catch {
      // Nothing was stored.
    }
  }
  if (!shouldShow({ replay, dismissed: dismissed() })) return;
  // The token form's own handler runs first and sets the token, so the key is read after it.
  $("token-form").addEventListener("submit", () => setTimeout(loadSetup, 0));
  $("setup-dialog").addEventListener("close", loadSetup);
  // Signing in settles the first step, so the gate hiding repaints the list.
  const observer = new MutationObserver(paintFirstRun);
  observer.observe(gate, { attributes: true, attributeFilter: ["hidden"] });
  onWorkers(paintFirstRun);
  loadSetup();
}

async function loadSetup() {
  if (!state.token) {
    setup = null;
  } else {
    try {
      const res = await fetch("/api/setup", { headers: { Authorization: `Bearer ${state.token}` } });
      setup = { status: res.status, body: res.ok ? await res.json().catch(() => null) : null };
    } catch {
      setup = { status: 0, body: null };
    }
  }
  paintFirstRun();
  loadWorkers();
  if (state.token && contract === undefined) checkContract();
}

// Asks the plant for its contract field. A read only: it changes nothing on the server.
async function checkContract() {
  if (!state.token || checking) return;
  checking = true;
  paintFirstRun();
  try {
    const res = await fetch("/api/plant", { headers: { Authorization: `Bearer ${state.token}` } });
    contract = res.ok ? contractOf(await res.json()) : null;
  } catch {
    contract = null;
  }
  checking = false;
  paintFirstRun();
}

function shown(el) {
  return el !== null && !el.hidden;
}

function collapsed() {
  return collapsedByUser ?? true;
}

// Words with `code` in backticks, as text nodes and <code> elements.
function words(text) {
  return text.split("`").map((part, i) => {
    if (i % 2 === 0) return document.createTextNode(part);
    const code = document.createElement("code");
    code.textContent = part;
    return code;
  });
}

function element(tag, className, text) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  if (text !== undefined) el.append(...words(text));
  return el;
}

// Paints the checklist from the page's state. Called at boot, when the view changes, and when an
// answer arrives.
export function paintFirstRun() {
  if (panel === null) return;
  if (!shouldShow({ replay, dismissed: dismissed() })) {
    panel.hidden = true;
    return;
  }
  refreshWorkers();
  const list = steps({
    token: Boolean(state.token),
    setup,
    contract,
    workers: workersAnswer(),
    view: state.view,
    watched: false,
    live: isLive(),
    origin: location.origin,
  });
  // All seven done: the checklist goes for good, as a dismiss would, so a reload keeps it gone.
  if (allDone(list)) {
    remember();
    if (panel.contains(document.activeElement)) $("plant").focus();
    panel.hidden = true;
    return;
  }
  const focused = panel.contains(document.activeElement) ? document.activeElement.dataset.focus : undefined;
  const folded = collapsed();
  const head = element("div", "fr-head");
  const toggle = element("button", "fr-toggle");
  toggle.type = "button";
  toggle.dataset.focus = "toggle";
  toggle.setAttribute("aria-expanded", String(!folded));
  const chevron = element("span", "chev", "›");
  chevron.setAttribute("aria-hidden", "true");
  toggle.append(element("b", undefined, "Setup"), element("span", "muted", progressShort(list)), chevron);
  toggle.addEventListener("click", () => {
    collapsedByUser = !folded;
    paintFirstRun();
  });
  const hide = element("button", "fr-hide", "hide");
  hide.type = "button";
  hide.dataset.focus = "hide";
  hide.hidden = folded;
  hide.setAttribute("aria-label", "Hide the checklist for good");
  hide.addEventListener("click", dismiss);
  head.append(toggle, hide);
  const parts = [head];
  if (!folded) {
    const ol = element("ol", "fr-steps");
    ol.append(...list.map(stepItem));
    parts.push(ol);
  }
  panel.replaceChildren(...parts);
  panel.hidden = false;
  if (focused) {
    const again = [...panel.querySelectorAll("[data-focus]")].find((el) => el.dataset.focus === focused);
    (again ?? toggle).focus();
  }
}

// One step: its mark, its title, and for the current step its words and the control that does it.
function stepItem(step, index) {
  const li = element("li", `fr-step state-${step.state}${step.current ? " current" : ""}`);
  if (step.current) li.setAttribute("aria-current", "step");
  const mark = element("span", "fr-mark", step.state === DONE ? "✓" : step.state === UNAVAILABLE ? "–" : String(index + 1));
  mark.setAttribute("aria-hidden", "true");
  const title = element("span", "fr-title", step.title);
  const status = element("span", "fr-sr", step.state === DONE ? ", done" : step.state === UNAVAILABLE ? ", not offered here" : "");
  li.append(mark, title, status);
  // A step this page cannot check, or this server does not offer, keeps its words open, since no
  // later step can say it is settled.
  if (!step.current && step.state !== UNAVAILABLE && step.state !== UNKNOWN) return li;
  if (!step.current && !state.token) return li;
  li.append(element("p", "fr-detail", step.detail));
  if (step.command) li.append(element("code", "fr-command", step.command));
  const control = actionButton(step);
  if (control) li.append(control);
  return li;
}

function actionButton(step) {
  const action = step.action;
  if (!action) return null;
  const button = element("button", "fr-action", action.label);
  button.type = "button";
  button.dataset.focus = `step:${step.id}`;
  switch (action.kind) {
    case "focus": {
      const target = $(action.target);
      // The order composer and the slip come from other parts of the page; with none showing, the
      // step's words say what to do instead.
      if (action.target === "token" ? gate.hidden : !shown(target)) return null;
      button.addEventListener("click", () => focusInside(target));
      break;
    }
    case "setup": {
      const opener = [$("setup-open"), $("setup-manage")].find(shown);
      if (!opener) return null;
      button.addEventListener("click", () => opener.click());
      break;
    }
    case "contract":
      button.disabled = checking;
      if (checking) button.textContent = "checking…";
      button.addEventListener("click", checkContract);
      break;
    case "copy":
      if (copied) button.textContent = "Copied";
      button.addEventListener("click", () => copyCommand(step.command));
      break;
    case "dismiss":
      button.addEventListener("click", dismiss);
      break;
    default:
      return null;
  }
  return button;
}

function focusInside(el) {
  const control = el.matches("input, button, textarea, select") ? el : el.querySelector("input, textarea, button, select");
  (control ?? el).focus();
  el.scrollIntoView?.({ block: "nearest" });
}

async function copyCommand(command) {
  try {
    await navigator.clipboard.writeText(command);
    copied = true;
  } catch {
    copied = false;
  }
  paintFirstRun();
}

function dismiss() {
  remember();
  panel.hidden = true;
  $("plant").focus();
}
