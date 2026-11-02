// The command bar on "/": one field and a list of results, all from the keyboard. Arrows move,
// Enter runs the result, Tab fills a verb, Esc closes. The queries and verbs are in command.js;
// this file runs them on the page.
import { openCard, openQuestionCard } from "./cards.js";
import { announce, noteMachine, openCompose, orderHere, pulse } from "./compose.js";
import { VERB_WORDS, emptyNote, receiptItems, search } from "./command.js";
import { requestRender } from "./main.js";
import { shownOrder } from "./panels.js";
import { $, canvas, state } from "./state.js";

// A go asked during a ride waits for the ride to ease back, checking this often, this many times.
const GO_RETRY_MS = 100;
const GO_TRIES = 12;

// The zoom a go lands at when the eased move cannot run: this many times the fit zoom.
const GO_FOCUS_RATIO = 3;

const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

// The open bar, `{ results, active, returnFocus }`, or null.
let bar = null;

let bound = false;

export function commandOpen() {
  return bar !== null;
}

export function openCommand() {
  if (state.app === null) return;
  bind();
  bar = { results: [], active: 0, returnFocus: document.activeElement };
  $("command").hidden = false;
  $("command-input").value = "";
  $("command-note").textContent = "";
  refresh();
  $("command-input").focus();
  // The names under the bar move aside on the next frame.
  requestRender();
}

export function closeCommand({ restore = true } = {}) {
  if (bar === null) return;
  const { returnFocus } = bar;
  bar = null;
  $("command").hidden = true;
  $("command-input").setAttribute("aria-expanded", "false");
  requestRender();
  if (!restore) return;
  const back = returnFocus instanceof HTMLElement && returnFocus.isConnected && returnFocus !== document.body
    ? returnFocus
    : canvas;
  back.focus({ preventScroll: true });
}

function bind() {
  if (bound) return;
  bound = true;
  const input = $("command-input");
  input.addEventListener("input", refresh);
  input.addEventListener("keydown", (event) => {
    if (bar === null) return;
    switch (event.key) {
      case "ArrowDown":
      case "ArrowUp": {
        event.preventDefault();
        const step = event.key === "ArrowDown" ? 1 : -1;
        const count = bar.results.length;
        if (count > 0) bar.active = (bar.active + step + count) % count;
        paintList();
        break;
      }
      case "Enter":
        event.preventDefault();
        run(bar.results[bar.active]);
        break;
      case "Tab": {
        const result = bar.results[bar.active];
        if (result?.verb === "hint") {
          event.preventDefault();
          fill(result.target.fill);
        }
        break;
      }
      case "Escape":
        event.preventDefault();
        event.stopPropagation();
        closeCommand();
        break;
      default:
    }
  });
  input.addEventListener("blur", (event) => {
    // A click on a result keeps the bar; a click anywhere else closes it.
    if (!$("command").contains(event.relatedTarget)) setTimeout(() => {
      if (bar !== null && !$("command").contains(document.activeElement)) closeCommand({ restore: false });
    }, 0);
  });
}

function refresh() {
  if (bar === null) return;
  const text = $("command-input").value;
  bar.results = state.view === null
    ? []
    : search(state.view, text, {
      receipts: receiptItems(state.plantViews),
      shownOrder: shownOrder(state.view)?.id ?? null,
    });
  bar.active = 0;
  $("command-note").textContent = bar.results.length === 0 && state.view !== null ? emptyNote(state.view, text) : "";
  paintList();
}

function paintList() {
  const list = $("command-list");
  const input = $("command-input");
  list.replaceChildren(...bar.results.map((result, i) => {
    const item = document.createElement("li");
    item.id = `command-option-${i}`;
    item.setAttribute("role", "option");
    item.setAttribute("aria-selected", String(i === bar.active));
    item.className = `result ${result.kind}`;
    const kind = document.createElement("span");
    kind.className = "kind";
    kind.textContent = result.kind === "hint" ? "verb" : result.kind;
    const label = document.createElement("span");
    label.className = "label";
    label.textContent = result.label;
    const detail = document.createElement("span");
    detail.className = "detail";
    detail.textContent = result.detail;
    const verb = document.createElement("span");
    verb.className = "verb";
    verb.textContent = i === bar.active ? `${VERB_WORDS[result.verb]} ↵` : "";
    item.append(kind, label, detail, verb);
    // The field keeps the focus while a result is clicked.
    item.addEventListener("mousedown", (event) => event.preventDefault());
    item.addEventListener("click", () => run(result));
    return item;
  }));
  const shown = bar.results.length > 0;
  input.setAttribute("aria-expanded", String(shown));
  if (shown) input.setAttribute("aria-activedescendant", `command-option-${bar.active}`);
  else input.removeAttribute("aria-activedescendant");
  list.children[bar.active]?.scrollIntoView({ block: "nearest" });
}

function fill(text) {
  const input = $("command-input");
  input.value = text;
  input.setSelectionRange(text.length, text.length);
  refresh();
}

function say(text) {
  $("command-note").textContent = text;
}

function run(result) {
  if (result === undefined) return;
  switch (result.verb) {
    case "hint":
      fill(result.target.fill);
      return;
    case "go":
      if (result.target.machine === null) {
        say("That has no machine on the map yet.");
        return;
      }
      closeCommand();
      flyTo(result.target.machine);
      announce(`Going to ${result.target.machine}.`);
      return;
    case "ride":
      if (state.app.ride(result.target.agent)) {
        closeCommand();
        requestRender();
        announce(`Riding ${result.target.agent}.`);
      } else {
        say(`${result.target.agent} has no drone on the map now.`);
      }
      return;
    case "order":
      closeCommand();
      if (result.target.machine === null) {
        if (!orderHere()) announce("No machine is on screen to order on.");
      } else {
        flyTo(result.target.machine, () => openCompose(result.target.machine));
      }
      return;
    case "seal":
      openSlip(result.target.order);
      return;
    case "question":
      closeCommand();
      openQuestionCard(result.target.question);
      return;
    case "clash":
      closeCommand();
      openCard(result.target.clash);
      return;
    default:
  }
}

// Eases the camera to the machine, as a tour step does, and pulses the machine once it lands. A
// ride eases back first. `then` runs once the camera has the move.
function flyTo(name, then = () => {}) {
  noteMachine(name);
  if (state.ridden !== "") state.app.leave();
  const attempt = (tries) => {
    if (state.app.tour_go(name, "machine", reducedMotion.matches)) {
      requestRender();
      pulse(name);
      then();
      return;
    }
    if (tries > 0) {
      setTimeout(() => attempt(tries - 1), GO_RETRY_MS);
      return;
    }
    const count = state.app.machine_count();
    for (let i = 0; i < count; i += 1) {
      if (state.app.machine_name(i) === name) {
        state.app.focus(i, GO_FOCUS_RATIO);
        requestRender();
        break;
      }
    }
    then();
  };
  requestRender();
  attempt(GO_TRIES);
}

// The slip shows the scope of one order. Seal moves the focus to its Seal button, or to the slip
// when the Seal is not offered yet.
function openSlip(order) {
  const shown = shownOrder(state.view);
  if (shown === undefined || shown.id !== order) {
    say(`The slip shows ${shown === undefined ? "no order" : `o-${shown.id}`} now, so o-${order} cannot be sealed from it yet.`);
    return;
  }
  if (state.ridden !== "") state.app.leave();
  closeCommand();
  requestRender();
  // The slip comes back once the ride has eased out, so the focus waits a frame for it.
  requestAnimationFrame(() => {
    const slip = $("slip");
    const target = slip.querySelector('[data-focus-key="seal"]') ?? slip;
    if (target === slip) slip.tabIndex = -1;
    target.focus();
    slip.scrollIntoView({ block: "nearest" });
    announce(target === slip ? `The slip of o-${order} is open; it offers no Seal yet.` : `The Seal of o-${order} has the focus.`);
  });
}
