// The vision pins: one pin at each task's machine whose latest gate run has a vision record, and
// the before and after viewer a pin opens beside its machine. What a record means is decided in
// vision.js. The records come from GET /api/tasks/{task}/vision and the screenshots from its PNG
// routes, fetched with the token, since an <img> cannot send one. These calls do not use
// stream.js's `get`, because a missing record (404) is the common answer, not a fault.
import { closeQuestionCard } from "./cards.js";
import { closeCompose } from "./compose.js";
import { requestRender } from "./main.js";
import { apiPath, target } from "./projects.js";
import { $, canvas, freeBand, params, state } from "./state.js";
import {
  ADVISORY,
  checkView,
  compareLayout,
  escapeClosesViewer,
  viewerPlacement,
  hasPin,
  imagePaths,
  pinLabel,
  secondLook,
  summarize,
  visionTasks,
} from "./vision.js";
import { capWords } from "./words.js";

// A record is pending while the witness runs, and the view carries no event when it ends, so a
// pending record is asked for again every POLL_MS, at most POLL_TRIES times.
const POLL_MS = 4000;
const POLL_TRIES = 45;

const PIN_GAP_PX = 4;
const PIN_OFFSET_PX = 6;
const VIEWER_GAP_PX = 14;
const EDGE_PX = 12;

const viewer = $("vision");
const layer = document.createElement("div");
layer.id = "vision-pins";

// task -> { key, machine, order, record }
const records = new Map();
const inFlight = new Set();
const pins = new Map();
// The open viewer: its task, the check it shows, how it compares, and the object URLs it holds.
let open = null;
// `?vision=fixture` shows a fixture record, marked FIXTURE, with no server.
let fixture = null;

function authed(path) {
  return fetch(path, { headers: { Authorization: `Bearer ${state.token}` } });
}

// Starts the pins: the layer and the fixture when the page asks for one. It
// never throws, so a broken fixture cannot stop the rest of the page from starting.
export async function installVision() {
  document.body.insertBefore(layer, viewer);
  // An image arriving or a page turning changes the viewer's height, so it is placed again.
  new ResizeObserver(() => placeViewer()).observe(viewer);
  if (params.get("vision") !== "fixture") return;
  try {
    await loadFixture();
  } catch (err) {
    console.warn(`the vision fixture was not shown: ${err?.message ?? err}`);
  }
}

export function viewerOpen() {
  return open !== null;
}

// Whether a window Esc closes the open viewer. main.js asks, since its key table owns Esc and
// closes the topmost panel first.
export function viewerTakesEscape(event) {
  const target = event.target instanceof Element ? event.target : null;
  const typing = target !== null && (target.matches("input, textarea, select") || target.isContentEditable);
  return escapeClosesViewer({
    open: open !== null,
    dialogOpen: document.querySelector("dialog[open]") !== null,
    typing,
    insideViewer: target !== null && viewer.contains(target),
  });
}

async function loadFixture() {
  const response = await fetch("fixtures/vision-witness.json");
  if (!response.ok) throw new Error(`fixture HTTP ${response.status}`);
  const loaded = await response.json();
  // The fixture plant has no log, so it has no view, and the view is what gives each machine its
  // footprint. The fixture carries the shortest log a world can have, its opening event, which
  // folds into a view of the plant's machines with no orders.
  if (state.view === null && params.get("plant") === "fixture") state.app.load_log(JSON.stringify(loaded.log));
  fixture = loaded;
  syncVision();
  requestRender();
}

// Asks for the record of each task whose order's receipt count moved. Called when the view changes.
export function syncVision() {
  if (state.view === null) return;
  if (fixture !== null) {
    syncFixture();
    return;
  }
  if (!state.token) return;
  for (const entry of visionTasks(state.view)) {
    const held = records.get(entry.task);
    if (held) held.machine = entry.machine;
    if (held?.key === entry.key || inFlight.has(entry.task)) continue;
    fetchRecord(entry, 0);
  }
  paintVisionPins();
}

function syncFixture() {
  const task = state.view.tasks.find((item) => typeof item.machine === "string");
  const machine = task?.machine ?? fixture.machine ?? state.view.machines[0]?.name;
  if (machine === undefined) return;
  records.set(fixture.record.task, { key: 0, machine, order: "fixture", record: fixture.record });
  paintVisionPins();
}

async function fetchRecord(entry, tries) {
  inFlight.add(entry.task);
  let record = null;
  try {
    const res = await authed(apiPath(target(), `/api/tasks/${encodeURIComponent(entry.task)}/vision`));
    if (res.ok) record = await res.json();
    else if (res.status !== 404) console.error(`the vision record of ${entry.task} answered ${res.status}`);
  } catch (err) {
    console.error(`the vision record of ${entry.task} was not read`, err);
  } finally {
    inFlight.delete(entry.task);
  }
  const held = records.get(entry.task);
  records.set(entry.task, { key: entry.key, machine: held?.machine ?? entry.machine, order: entry.order, record });
  if (record?.status === "pending" && tries < POLL_TRIES) {
    setTimeout(() => {
      const now = records.get(entry.task);
      if (now && now.key === entry.key && !inFlight.has(entry.task)) fetchRecord({ ...entry, machine: now.machine }, tries + 1);
    }, POLL_MS);
  }
  paintVisionPins();
  if (open?.task === entry.task) paintViewer();
  requestRender();
}

// One pin per record that has one, in task order. A pin's words are its verdict; the tone never
// reads as a pass.
export function paintVisionPins() {
  const shown = new Set();
  for (const [task, held] of records) {
    if (!hasPin(held.record)) continue;
    shown.add(task);
    let pin = pins.get(task);
    if (pin === undefined) {
      pin = document.createElement("button");
      pin.type = "button";
      pin.className = "vpin";
      pin.addEventListener("click", () => openViewer(task));
      pins.set(task, pin);
      layer.append(pin);
    }
    const summary = summarize(held.record);
    pin.className = `vpin tone-${summary.tone}`;
    pin.classList.toggle("open", open?.task === task);
    pin.setAttribute("aria-label", pinLabel(task, held.machine, summary));
    pin.title = ADVISORY;
    const glyph = document.createElement("i");
    glyph.setAttribute("aria-hidden", "true");
    const name = document.createElement("span");
    name.textContent = "vision";
    const word = document.createElement("b");
    word.textContent = summary.word;
    pin.replaceChildren(glyph, name, word);
    pin.size = undefined;
  }
  for (const [task, pin] of pins) {
    if (shown.has(task)) continue;
    pin.remove();
    pins.delete(task);
  }
}

function footprints() {
  const byName = new Map();
  for (const machine of state.view?.machines ?? []) byName.set(machine.name, machine);
  return byName;
}

// The machine's corner furthest right on screen, so the pin sits beside the machine and clear of
// its name, which hangs under the front edge.
function anchorOf(machine) {
  const [x0, y0] = machine.min;
  const [x1, y1] = [x0 + machine.size[0], y0 + machine.size[1]];
  let best = null;
  for (const [x, y] of [[x0, y0], [x1, y0], [x0, y1], [x1, y1]]) {
    const [sx, sy] = state.app.world_to_screen(x, y);
    if (best === null || sx > best[0]) best = [sx, sy];
  }
  return best;
}

// Places each pin beside its machine, stacked when one machine has several, and the open viewer
// beside its pin. Called every frame.
export function placeVisionPins() {
  if (pins.size === 0 && open === null) return;
  const byName = footprints();
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  const stacked = new Map();
  for (const [task, pin] of pins) {
    const held = records.get(task);
    const machine = byName.get(held?.machine);
    if (!machine) {
      pin.style.visibility = "hidden";
      continue;
    }
    if (pin.size === undefined) pin.size = [pin.offsetWidth, pin.offsetHeight];
    const [w, h] = pin.size;
    const row = stacked.get(machine.name) ?? 0;
    stacked.set(machine.name, row + 1);
    const [x, y] = anchorOf(machine);
    const left = x + PIN_OFFSET_PX;
    const top = y - h / 2 + row * (h + PIN_GAP_PX);
    const inside = left >= 0 && top >= 0 && left + w <= width && top + h <= height;
    pin.style.visibility = inside ? "visible" : "hidden";
    pin.style.transform = `translate(${left}px, ${top}px)`;
    pin.at = inside ? { left, top, w, h } : null;
  }
  placeViewer();
}

// The viewer opens beside its pin, on the side with room, and stays inside the window. With no
// pin on screen it keeps the place it had, or docks at the top right before it has one.
function placeViewer() {
  if (open === null) return;
  const at = pins.get(open.task)?.at ?? null;
  const band = freeBand();
  const spot = viewerPlacement({
    pin: at,
    size: [viewer.offsetWidth, viewer.offsetHeight],
    viewport: [band.right, canvas.clientHeight],
    from: band.left,
    last: open.placed,
    gap: VIEWER_GAP_PX,
    edge: EDGE_PX,
  });
  viewer.classList.toggle("docked", spot === null);
  if (spot === null) return;
  viewer.style.left = `${spot.left}px`;
  viewer.style.top = `${spot.top}px`;
  open.placed = spot;
}

function openViewer(task) {
  if (open?.task === task) {
    closeViewer();
    return;
  }
  // One panel opens beside the map at a time.
  closeCompose();
  closeQuestionCard();
  releaseImages();
  open = { task, index: 0, mode: "side", swipe: 50, urls: new Map(), placed: null, opener: pins.get(task) ?? null };
  viewer.hidden = false;
  paintViewer();
  paintVisionPins();
  requestRender();
  viewer.querySelector("h2")?.focus();
}

export function closeViewer() {
  if (open === null) return;
  const opener = open.opener;
  releaseImages();
  open = null;
  viewer.hidden = true;
  paintVisionPins();
  if (opener?.isConnected) opener.focus();
}

function releaseImages() {
  if (open === null) return;
  for (const url of open.urls.values()) {
    if (url) URL.revokeObjectURL(url);
  }
  open.urls.clear();
}

function element(tag, text, className) {
  const el = document.createElement(tag);
  if (text !== undefined && text !== null) el.textContent = text;
  if (className) el.className = className;
  return el;
}

// A reason cut to CARD_WORDS words, with the full text on hover and in its label, and in full while
// Alt is held.
function reasonElement(tag, text, className) {
  const shown = capWords(text);
  const el = element(tag, state.altDown ? text : shown, className);
  if (shown !== text) {
    el.title = text;
    el.setAttribute("aria-label", text);
  }
  return el;
}

function button(text, onClick, pressed) {
  const el = element("button", text);
  el.type = "button";
  el.dataset.focus = text;
  el.addEventListener("click", onClick);
  if (pressed !== undefined) el.setAttribute("aria-pressed", String(pressed));
  return el;
}

// Paints the viewer for the record it holds: the advisory line, the verdict over all pages, then
// one page at a time with its two screenshots, its verdict and reason, and the second look.
export function paintViewer() {
  if (open === null) return;
  const held = records.get(open.task);
  const record = held?.record ?? null;
  const summary = summarize(record);
  const head = element("div", undefined, "vhead");
  const title = element("h2", `Vision · ${open.task} · ${held?.machine ?? ""}`);
  title.tabIndex = -1;
  if (fixture !== null) title.append(element("span", "FIXTURE", "fixture"));
  head.append(title, button("close", closeViewer));
  const advisory = element("p", ADVISORY, "advisory");
  const verdict = element("p", undefined, `vsum tone-${summary.tone}`);
  verdict.append(element("i"), element("b", summary.word), element("span", summary.counts ? ` · ${summary.counts}` : ""));
  const parts = [head, advisory, verdict];
  const checks = Array.isArray(record?.checks) ? record.checks : [];
  if (checks.length === 0) {
    parts.push(element("p", summary.line, "vline"));
  } else {
    open.index = Math.min(open.index, checks.length - 1);
    parts.push(...checkParts(record, checks));
  }
  const by = [record?.model, record?.provider, record?.gate_run ? `gate run ${record.gate_run}` : null].filter(Boolean);
  if (by.length > 0) parts.push(element("p", `first look: ${by.join(" · ")}`, "foot"));
  // The viewer is rebuilt on each change, so focus goes back to the control that had it.
  const focused = viewer.contains(document.activeElement) ? document.activeElement.dataset.focus : undefined;
  viewer.replaceChildren(...parts);
  const again = focused && [...viewer.querySelectorAll("[data-focus]")].find((el) => el.dataset.focus === focused);
  if (again) (again.disabled ? title : again).focus();
  placeViewer();
}

function checkParts(record, checks) {
  const check = checks[open.index];
  const view = checkView(check);
  const parts = [];
  if (checks.length > 1) {
    const nav = element("div", undefined, "vnav");
    const prev = button("‹", () => { open.index -= 1; paintViewer(); });
    prev.setAttribute("aria-label", "previous page");
    prev.disabled = open.index === 0;
    const next = button("›", () => { open.index += 1; paintViewer(); });
    next.setAttribute("aria-label", "next page");
    next.disabled = open.index >= checks.length - 1;
    nav.append(prev, element("span", `page ${open.index + 1} of ${checks.length}`), next);
    parts.push(nav);
  }
  parts.push(element("p", `${view.route} · ${view.compared}`, "vroute"));
  parts.push(compare(record.task ?? open.task, check));
  const line = element("p", undefined, `vverdict tone-${view.tone}`);
  line.append(element("i"), element("b", view.word));
  if (view.reason) line.append(reasonElement("span", view.reason));
  parts.push(line);
  const second = secondLook(check);
  if (second !== null) {
    const section = element("section", undefined, `vsecond tone-${second.tone}`);
    section.append(element("h3", "Muse Wonder's second look"), element("p", second.line));
    if (second.reason) section.append(reasonElement("p", second.reason, "vreason"));
    if (second.model) section.append(element("p", second.model, "muted"));
    parts.push(section);
  }
  if (view.mustKeep.length > 0) {
    const keeps = element("details", undefined, "vkeeps");
    keeps.append(element("summary", `checked against ${view.mustKeep.length} must-keep ${view.mustKeep.length === 1 ? "item" : "items"}`));
    const list = element("ul");
    list.append(...view.mustKeep.map((text) => element("li", text)));
    keeps.append(list);
    parts.push(keeps);
  }
  return parts;
}

// The before and after pages, side by side or as a swipe, with the toggle that switches them.
function compare(task, check) {
  const paths = fixture !== null ? fixturePaths(check) : imagePaths(task, check);
  const layout = compareLayout(open.mode, paths);
  if (layout.empty) return element("p", layout.message, "vmissing");
  const box = element("div", undefined, `vcompare mode-${layout.mode}`);
  box.style.setProperty("--swipe", `${open.swipe}%`);
  box.append(figure("before", paths.before, check, "No before page: this check looked at the after page only."));
  box.append(figure("after", paths.after, check, "No after page was captured."));
  const modes = element("div", undefined, "vmodes");
  const group = element("span", undefined, "modes");
  group.setAttribute("role", "group");
  group.setAttribute("aria-label", "compare");
  const side = button("side by side", () => { open.mode = "side"; paintViewer(); }, layout.mode === "side");
  const swipe = button("swipe", () => { open.mode = "swipe"; paintViewer(); }, layout.mode === "swipe");
  swipe.disabled = !layout.swipeable;
  if (!layout.swipeable) swipe.title = "Swipe needs both a before and an after page.";
  group.append(side, swipe);
  modes.append(group);
  if (layout.mode === "swipe") {
    const range = element("input");
    range.type = "range";
    range.min = "0";
    range.max = "100";
    range.value = String(open.swipe);
    range.setAttribute("aria-label", "Swipe between the before and after pages");
    range.addEventListener("input", () => {
      open.swipe = Number(range.value);
      box.style.setProperty("--swipe", `${open.swipe}%`);
    });
    modes.append(range);
  }
  const wrap = element("div");
  wrap.append(box, modes);
  return wrap;
}

function figure(side, path, check, missing) {
  const fig = element("figure", undefined, `vside ${side}`);
  const caption = element("figcaption", side);
  if (path === null) {
    fig.append(element("p", missing, "vmissing"), caption);
    return fig;
  }
  const img = element("img");
  img.alt = `${side} page of ${check.route}`;
  if (check.width && check.height) {
    img.width = check.width;
    img.height = check.height;
  }
  fig.append(img, caption);
  loadImage(path, img, fig);
  return fig;
}

// Fetches one screenshot with the token, once per open viewer, and shows it as an object URL.
async function loadImage(path, img, fig) {
  const held = open;
  if (held.urls.has(path)) {
    const url = held.urls.get(path);
    if (url) img.src = url;
    else fig.replaceChildren(element("p", "This page's image could not be read.", "vmissing"), fig.lastChild);
    return;
  }
  held.urls.set(path, null);
  let url = null;
  try {
    const res = fixture !== null ? await fetch(path) : await authed(path);
    if (res.ok) url = URL.createObjectURL(await res.blob());
  } catch (err) {
    console.error(`the image ${path} was not read`, err);
  }
  if (open !== held) {
    if (url) URL.revokeObjectURL(url);
    return;
  }
  held.urls.set(path, url);
  if (url) img.src = url;
  else fig.replaceChildren(element("p", "This page's image could not be read.", "vmissing"), fig.lastChild);
}

function fixturePaths(check) {
  return {
    before: check.baseline_image_sha256 ? `fixtures/vision/${check.index}-before.svg` : null,
    after: check.candidate_image_sha256 ? `fixtures/vision/${check.index}-after.svg` : null,
  };
}
