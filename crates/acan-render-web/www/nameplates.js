// One quiet plate per project on a canvas of several: its name, the count of what waits on you
// there when above zero, and its status when it is degraded. A plate hangs under its plant. A
// click puts the project on screen, a double-click flies there, and a drag moves the plant.
import { canvasState } from "./projects.js";
import { state } from "./state.js";

// The plate's top sits this far under the plant's lowest corner, clear of the machines' names.
const DROP_PX = 30;

const CLICK_PX = 4;

let layer = null;

let hooks = null;

const plates = new Map();

// The counts the plates last showed, by project.
let shown = new Map();

function element(tag, className, text) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  if (text !== undefined) el.textContent = text;
  return el;
}

// `boundsOf(id)` gives a project's world bounds; `select`, `fly`, `move(id, dx, dy)` and `moved()`
// act on it.
export function installPlates(given) {
  hooks = given;
  layer = element("div");
  layer.id = "nameplates";
  document.getElementById("labels").after(layer);
  for (const project of canvasState.list) {
    const el = element("div", "nameplate");
    el.dataset.project = project.id;
    el.tabIndex = 0;
    el.setAttribute("role", "button");
    const name = element("span", "name");
    const badge = element("span", "badge");
    const status = element("span", "status");
    el.append(name, badge, status);
    bindPlate(el, project.id);
    layer.append(el);
    plates.set(project.id, { el, name, badge, status });
  }
}

function bindPlate(el, id) {
  let drag = null;
  el.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    el.setPointerCapture(event.pointerId);
    drag = { x: event.clientX, y: event.clientY, last: ground(event), moved: false };
  });
  el.addEventListener("pointermove", (event) => {
    if (drag === null) return;
    if (!drag.moved && Math.hypot(event.clientX - drag.x, event.clientY - drag.y) < CLICK_PX) return;
    drag.moved = true;
    el.classList.add("dragging");
    const now = ground(event);
    if (now !== null && drag.last !== null) hooks.move(id, now[0] - drag.last[0], now[1] - drag.last[1]);
    drag.last = now;
  });
  const end = () => {
    if (drag === null) return;
    const moved = drag.moved;
    drag = null;
    el.classList.remove("dragging");
    if (moved) hooks.moved();
    else hooks.select(id);
  };
  el.addEventListener("pointerup", end);
  el.addEventListener("pointercancel", end);
  el.addEventListener("dblclick", () => hooks.fly(id));
  el.addEventListener("keydown", (event) => {
    if (event.key === "Enter") hooks.fly(id);
  });
}

function ground(event) {
  const rect = document.getElementById("plant").getBoundingClientRect();
  const point = state.app.screen_to_ground(event.clientX - rect.left, event.clientY - rect.top);
  return point.length === 2 ? [point[0], point[1]] : null;
}

// Paints each plate's words: its name, its count when given or as last given, and its status.
export function paintPlates(counts = shown) {
  shown = counts;
  for (const project of canvasState.list) {
    const plate = plates.get(project.id);
    if (plate === undefined) continue;
    const count = counts.get(project.id) ?? 0;
    plate.name.textContent = project.name;
    plate.badge.textContent = String(count);
    plate.badge.hidden = count === 0;
    const degraded = project.status === "degraded";
    plate.status.textContent = degraded ? "degraded" : "";
    plate.status.hidden = !degraded;
    plate.el.classList.toggle("here", project.id === canvasState.active);
    const waits = count === 0 ? "" : `, ${count} ${count === 1 ? "thing waits" : "things wait"} on you`;
    plate.el.setAttribute("aria-label", `${project.name}${waits}${degraded ? ", degraded" : ""}`);
  }
}

// Hangs each plate under its plant, as the camera stands this frame.
export function placePlates() {
  if (layer === null || state.app === null) return;
  const width = layer.clientWidth;
  const height = layer.clientHeight;
  for (const [id, plate] of plates) {
    const box = hooks.boundsOf(id);
    if (box === null) {
      plate.el.style.visibility = "hidden";
      continue;
    }
    const corners = [[box[0], box[1]], [box[2], box[1]], [box[0], box[3]], [box[2], box[3]]]
      .map(([x, y]) => state.app.world_to_screen(x, y));
    const xs = corners.map((p) => p[0]);
    const x = (Math.min(...xs) + Math.max(...xs)) / 2;
    const y = Math.max(...corners.map((p) => p[1])) + DROP_PX;
    const on = x > -100 && x < width + 100 && y > 0 && y < height + 40;
    plate.el.style.visibility = on ? "visible" : "hidden";
    plate.el.style.transform = `translate(${Math.round(x)}px, ${Math.round(y)}px) translate(-50%, 0)`;
  }
}
