// The Set kind menu. A right click on a machine opens it beside the pointer, with the kinds the
// machine can take and the one it has now marked. A choice writes it through PUT /api/plant/kinds,
// then fetches the plant again so the machine redraws in its new kind. The words and requests are
// in kinds.js.
import { requestRender } from "./main.js";
import { machineAt } from "./order.js";
import { $, canvas, freeze, replay, state } from "./state.js";
import { apiPath, canvasState, target } from "./projects.js";
import { replaceProjectPlant } from "./multi.js";
import { forgetToken, get, hold } from "./stream.js";
import { canSetKind, kindBody, menuPlace, menuRows, setKindRefusal } from "./kinds.js";

// The open menu, `{ machine, returnFocus }`, or null.
let open = null;

// Whether a PUT is in flight, so a second choice waits for the first.
let busy = false;

const menuEl = $("kind-menu");
const titleEl = $("kind-menu-title");
const itemsEl = $("kind-menu-items");
const noteEl = $("kind-menu-note");

// Opens the menu for the machine under the pointer at (clientX, clientY). Returns whether it
// opened. Nothing opens on open ground, or where this page may not write a kind.
export function openKindMenuAt(clientX, clientY) {
  if (state.view === null || !canSetKind({ replay, held: hold.on, token: state.token })) return false;
  const rect = canvas.getBoundingClientRect();
  const project = (x, y) => state.app.world_to_screen(x, y);
  const name = machineAt(state.view.machines, project, clientX - rect.left, clientY - rect.top);
  if (name === null) {
    closeKindMenu();
    return false;
  }
  show(name, clientX, clientY);
  return true;
}

function show(machine, clientX, clientY) {
  open = { machine, returnFocus: open?.returnFocus ?? document.activeElement };
  titleEl.textContent = `Set the kind of ${machine}`;
  const current = state.app.machine_kind(machine);
  itemsEl.replaceChildren(...menuRows(current).map(item));
  setNote("", false);
  menuEl.hidden = false;
  const { width, height } = menuEl.getBoundingClientRect();
  const { left, top } = menuPlace(clientX, clientY, width, height, { width: innerWidth, height: innerHeight });
  menuEl.style.left = `${left}px`;
  menuEl.style.top = `${top}px`;
  itemsEl.querySelector("button")?.focus();
}

function item(row) {
  const button = document.createElement("button");
  button.type = "button";
  button.setAttribute("role", "menuitemradio");
  button.setAttribute("aria-checked", String(row.current));
  button.textContent = row.label;
  button.addEventListener("click", () => choose(row.kind));
  return button;
}

function closeKindMenu() {
  if (open === null) return;
  const { returnFocus } = open;
  open = null;
  const hadFocus = menuEl.contains(document.activeElement);
  menuEl.hidden = true;
  if (hadFocus && returnFocus instanceof HTMLElement) returnFocus.focus();
}

function setNote(text, bad) {
  noteEl.textContent = text;
  noteEl.classList.toggle("bad", bad);
}

// Writes the kind for the machine the menu is open on. `kind` is null to clear it. A refusal keeps
// the menu open with the reason; a taken kind closes it once the plant has redrawn.
async function choose(kind) {
  if (open === null || busy) return;
  const { machine } = open;
  busy = true;
  setNote("Setting the kind...", false);
  try {
    const res = await fetch(apiPath(target(), "/api/plant/kinds"), {
      method: "PUT",
      headers: { Authorization: `Bearer ${state.token}`, "Content-Type": "application/json" },
      body: JSON.stringify(kindBody(machine, kind)),
    });
    if (res.status === 401) {
      forgetToken();
      closeKindMenu();
      return;
    }
    const words = setKindRefusal(res.status);
    if (words !== null) {
      setNote(words, true);
      return;
    }
    const events = await res.json();
    if (freeze) {
      for (const event of events) state.app.ingest(JSON.stringify(event));
    }
  } catch (err) {
    console.error(err);
    setNote(setKindRefusal(0), true);
    return;
  } finally {
    busy = false;
  }
  try {
    await redrawPlant();
    closeKindMenu();
  } catch (err) {
    console.error(err);
    setNote("The kind is set, but the map did not redraw. Reload the page to see it.", true);
  }
}

// Fetches the plant again and puts it on the canvas: the project on screen in a multi-project
// canvas, else the one plant. The log is not fetched again, since the change's own events came in
// through the stream or the answer above.
async function redrawPlant() {
  const text = await get(apiPath(target(), "/api/plant"));
  if (canvasState.multi) replaceProjectPlant(canvasState.active, text);
  else state.app.set_plant(text);
  requestRender();
}

// Keeps the menu open only while it is needed: a press outside it closes it, and Escape closes it
// from inside, with the arrow keys moving between the kinds.
export function bindKindMenu() {
  document.addEventListener("pointerdown", (event) => {
    if (open !== null && !menuEl.contains(event.target)) closeKindMenu();
  }, true);
  menuEl.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      closeKindMenu();
      return;
    }
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    event.preventDefault();
    const buttons = [...itemsEl.querySelectorAll("button")];
    const at = buttons.indexOf(document.activeElement);
    const step = event.key === "ArrowDown" ? 1 : -1;
    buttons[(at + step + buttons.length) % buttons.length]?.focus();
  });
}
