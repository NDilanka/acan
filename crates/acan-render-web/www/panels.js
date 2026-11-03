// The HTML panels: the point-of-view chip and reticle, the usage table, and the note that an
// older page passes events over.
import { shownOrderOf } from "./order.js";
import {
  hintEl,
  hintRest,
  historyBody,
  historyEl,
  metaBody,
  metaLegend,
  metaNote,
  metaSection,
  povAgent,
  povAlt,
  povBracket,
  povChip,
  povDot,
  povEls,
  povLine,
  slipEl,
  stale,
  state,
  usagePanel,
} from "./state.js";
import { OFFLINE_PROVIDER, roleLabel, splitUsage } from "./usage.js";

// The usage panel shows this many rows at rest. Alt shows every row.
const USAGE_REST_ROWS = 3;

// The point of view. The reticle is a square POV_SQUARE_PX across, with arms POV_ARM_PX long. A
// click counts as on a drone within DRONE_PICK_PX of its disc's edge. The chip and the frame fade
// over POV_FADE_MS, the time the CSS gives --pov-fade.
const POV_FADE_MS = 200;

const POV_SQUARE_PX = 40;

const POV_ARM_PX = 8;

const RIDE_HINT = "<span class=\"opt\"><kbd>Alt</kbd> words</span><span class=\"opt\"><kbd>Shift</kbd><kbd>Tab</kbd> back</span><span><kbd>Tab</kbd> next</span><span><kbd>Esc</kbd> leave</span>";

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
  // The slip has no fade of its own, so it is hidden and shown with the ride directly.
  slipEl.hidden = riding || !state.hasSlip;
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

// The order the slip shows the scope of. See `shownOrderOf`.
export function shownOrder(view) {
  return shownOrderOf(view, state.postedOrder);
}

// The usage panel has two sections. The first is the origin's count since its start, one row per
// role, provider and model, with the legend that says what each provider name is. Alt also shows
// the history of this log, which the page folds from the model calls in the log. The panel is
// hidden until there is something to show. At rest the first section shows its first few rows;
// Alt shows every origin row, the provider legend and the newest history rows the view carries.
// A page with no origin rows (a replay, a fixture) shows the history at rest instead of an empty
// table, so the attribution is still on screen.
export function paintUsage() {
  const served = state.serverUsage;
  const history = state.view === null ? [] : state.view.usage;
  const { live, offline, total } = splitUsage(served?.rows ?? []);
  const rows = [...live, ...offline];
  usagePanel.hidden = rows.length === 0 && history.length === 0 && !served?.error;
  const shown = state.altDown ? rows : rows.slice(0, USAGE_REST_ROWS);
  metaBody.replaceChildren(...shown.map(metaRow));
  metaNote.textContent = metaNoteText(served, live.length, offline.length, total);
  metaSection.hidden = rows.length === 0 && !served?.error && history.length > 0;
  metaLegend.hidden = !state.altDown || rows.length === 0;
  historyEl.hidden = history.length === 0 || (!state.altDown && !metaSection.hidden);
  const earlier = state.view === null ? 0 : state.view.usage_earlier;
  historyBody.replaceChildren(...history.map(historyRow), ...(earlier > 0 ? [earlierRow(earlier)] : []));
}

// The view carries only the newest few history rows; one line counts the older ones.
function earlierRow(count) {
  const tr = document.createElement("tr");
  const td = document.createElement("td");
  td.colSpan = 5;
  td.className = "earlier";
  td.textContent = `+${count} earlier`;
  tr.append(td);
  return tr;
}

function metaNoteText(served, liveRows, offlineRows, total) {
  if (served === null) return "";
  if (served.error !== null) return `usage not read from the server: ${served.error}`;
  if (liveRows + offlineRows === 0) return "no model calls since server start";
  const live = `live total: ${total.calls} calls, ${total.input.toLocaleString("en-US")} in, ${total.output.toLocaleString("en-US")} out`;
  return offlineRows === 0 ? live : `${live}. Offline stand-in: not a model call, not counted.`;
}

// An offline row is the stand-in's; its provider cell says so, and the note says what it is.
function metaRow(row) {
  const offline = row.provider === OFFLINE_PROVIDER;
  const provider = offline ? "offline stand-in" : row.provider;
  const values = [roleLabel(row.role), provider, row.model, row.calls, row.prompt_tokens, row.completion_tokens];
  const tr = tableRow(values, 3);
  if (offline) tr.classList.add("standin");
  return tr;
}

function historyRow(row) {
  return tableRow([row.role, row.model, row.calls, row.input, row.output], 2);
}

// A table row with one cell per value. Values from `firstNumeric` on are right-aligned.
function tableRow(values, firstNumeric) {
  const tr = document.createElement("tr");
  values.forEach((value, i) => {
    const td = document.createElement("td");
    td.textContent = typeof value === "number" ? value.toLocaleString("en-US") : String(value);
    if (i >= firstNumeric) td.className = "num";
    tr.append(td);
  });
  return tr;
}
