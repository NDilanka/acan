// The HTML panels: the order island, the point-of-view chip and reticle, the usage table, and
// the note that an older page passes events over.
import { label } from "./marks.js";
import {
  hintEl,
  hintRest,
  orderEl,
  orderLine,
  povAgent,
  povAlt,
  povBracket,
  povChip,
  povDot,
  povEls,
  povLine,
  stale,
  state,
  usageBody,
  usagePanel,
} from "./state.js";

// The usage panel shows this many rows at rest. Alt shows every row.
const USAGE_REST_ROWS = 3;

// The point of view. The reticle is a square POV_SQUARE_PX across, with arms POV_ARM_PX long. A
// click counts as on a drone within DRONE_PICK_PX of its disc's edge. The chip and the frame fade
// over POV_FADE_MS, the time the CSS gives --pov-fade.
const POV_FADE_MS = 200;

const POV_SQUARE_PX = 40;

const POV_ARM_PX = 8;

const RIDE_HINT = "<kbd>Esc</kbd> leave&nbsp;&nbsp;<kbd>Tab</kbd> next&nbsp;&nbsp;<kbd>Shift</kbd><kbd>Tab</kbd> back&nbsp;&nbsp;<kbd>Alt</kbd> words&nbsp;&nbsp;wheel closer";

// The ride as the wasm side has it now: the chip's words, the reticle on the beam's target, and
// the frame, vignette and chip shown or hidden as the ride starts or ends.
export function paintRide() {
  const ride = JSON.parse(state.app.pov_json());
  const agent = ride === null ? "" : ride.agent;
  if ((agent === "") !== (state.ridden === "")) rideToggled(agent !== "");
  state.ridden = agent;
  if (ride === null) return;
  document.documentElement.style.setProperty("--pov", ride.hue);
  paintChip(ride);
  paintReticle(ride.target_css);
}

function rideToggled(riding) {
  for (const el of povEls) {
    if (riding) showPov(el);
    else hidePov(el);
  }
  // The order island fades out with the point of view, and comes back when the ride ends.
  if (riding && !orderEl.hidden) hidePov(orderEl);
  if (!riding && state.hasOrder) showPov(orderEl);
  hintEl.innerHTML = riding ? RIDE_HINT : hintRest;
}

// Showing removes `hidden`, then adds `on` once the element has a style to fade from. Hiding
// removes `on` and sets `hidden` after the fade.
const povTimers = new WeakMap();

// The svg reticle has no `hidden` property (only HTML elements do), so the attribute is set and
// removed directly. Setting the property would leave the svg at display none.
function setHidden(el, hidden) {
  el.toggleAttribute("hidden", hidden);
}

function showPov(el) {
  clearTimeout(povTimers.get(el));
  setHidden(el, false);
  void el.getBoundingClientRect();
  requestAnimationFrame(() => el.classList.add("on"));
}

function hidePov(el) {
  el.classList.remove("on");
  povTimers.set(el, setTimeout(() => {
    setHidden(el, true);
  }, POV_FADE_MS));
}

function setText(el, text) {
  if (el.textContent !== text) el.textContent = text;
}

// The chip's words are the machine and the activity word its flag shows. Alt adds a row with the
// task and the file, and the row lines up under the agent's name.
function paintChip(ride) {
  povDot.classList.toggle("wait", ride.waiting);
  setText(povAgent, ride.agent);
  setText(povLine, `${ride.machine} \u00B7 ${ride.activity}`);
  povAlt.hidden = !state.altDown;
  if (!state.altDown) return;
  setText(povAlt, [ride.task, ride.file].filter((part) => part).join(" \u00B7 "));
  const pad = parseFloat(getComputedStyle(povChip).paddingLeft);
  povAlt.style.marginLeft = `${povAgent.offsetLeft - pad}px`;
}

// The reticle's four brackets sit on the target, a square POV_SQUARE_PX across. Off screen or
// behind the camera it is hidden at once, without a fade.
function paintReticle(target) {
  if (target === null) {
    povBracket.style.visibility = "hidden";
    return;
  }
  const [x, y] = target;
  const half = POV_SQUARE_PX / 2;
  const [x0, x1] = [x - half, x + half];
  const [y0, y1] = [y - half, y + half];
  const arm = POV_ARM_PX;
  povBracket.setAttribute("d", [
    `M${x0 + arm} ${y0}H${x0}V${y0 + arm}`,
    `M${x1 - arm} ${y0}H${x1}V${y0 + arm}`,
    `M${x0 + arm} ${y1}H${x0}V${y1 - arm}`,
    `M${x1 - arm} ${y1}H${x1}V${y1 - arm}`,
  ].join(""));
  povBracket.style.visibility = "visible";
}

// A page older than the origin passes over the events it cannot read, and says so here.
export function paintStale() {
  const passed = state.app.not_shown();
  stale.hidden = passed === 0;
  stale.textContent = passed === 0
    ? ""
    : `this page is older than the server: ${passed} ${passed === 1 ? "event" : "events"} not shown, rebuild the UI`;
}

export function paintOrderLine() {
  const order = state.view.orders.find((item) => item.phase !== "cancelled" && item.phase !== "landed")
    ?? state.view.orders.at(-1);
  state.hasOrder = order !== undefined;
  orderLine.textContent = state.hasOrder
    ? `${label("order", order.id)} · ${order.machine} · ${order.phase.replace("_", " ")} · ${order.sentence}`
    : "";
  paintOrderShown();
}

// A ride holds the island: it fades with the point of view, so rideToggled shows and hides it.
function paintOrderShown() {
  if (state.ridden !== "") return;
  orderEl.hidden = !state.hasOrder;
  orderEl.classList.toggle("on", state.hasOrder);
}

// The usage panel reads the model calls the view has recorded: one row per model and step. It is
// hidden until the first call. At rest it shows the first few rows; Alt shows every row.
export function paintUsage() {
  const rows = state.view === null ? [] : state.view.usage;
  usagePanel.hidden = rows.length === 0;
  const shown = state.altDown ? rows : rows.slice(0, USAGE_REST_ROWS);
  usageBody.replaceChildren(...shown.map(usageRow));
}

function usageRow(row) {
  const tr = document.createElement("tr");
  const cells = [row.role, row.model, row.calls, row.input, row.output];
  cells.forEach((value, i) => {
    const td = document.createElement("td");
    td.textContent = typeof value === "number" ? value.toLocaleString("en-US") : String(value);
    if (i >= 2) td.className = "num";
    tr.append(td);
  });
  return tr;
}
