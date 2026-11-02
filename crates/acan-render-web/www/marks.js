// The HTML marks over the canvas: agent flags with their witness chips, the leaders to each
// drone, the machine labels and the clash pins. Positions come from the wasm side; this file
// only places the elements.
import { openCard, paintCard } from "./cards.js";
import { canvas, flagsEl, labelsEl, leadersEl, state } from "./state.js";
import { boxAt, clearOf, inView, placeOverlays } from "./place.js";

const ID_PREFIX = { order: "o", clash: "c", question: "q", task: "t" };

const LABEL_OFFSET_PX = 6;

const LABEL_PAD_PX = 4;

const PIN_PICK_PX = 18;

// The gap between the flags of one machine, stacked in a column above its bots.
const FLAG_ROW_GAP_PX = 4;

const SVG_NS = "http://www.w3.org/2000/svg";

// The riding agent's own flag is hidden: the chip replaces it.
function isRiderFlag(flag) {
  return state.ridden !== "" && flag !== undefined && flag.kind === "agent"
    && flag.text.startsWith(`${state.ridden} \u00B7 `);
}

// Labels come in priority order, larger and nearer first. A label that would overlap one
// already placed stays hidden, so the plant never shows a pile of names. The drones and the flags
// that stay put are placed first, so no name lands on them. The names come next, and the plot
// flags last: a plot flag moves clear of a name, so a name is never hidden by one.
export function placeLabels() {
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  state.labelGeneration += 1;
  const placedBoxes = [];
  const plots = placeFlags(placedBoxes);
  const quads = state.app.label_layout(state.altDown);
  const shown = [];
  for (let k = 0; k < quads.length; k += 4) {
    const index = quads[k];
    const x = quads[k + 1];
    const y = quads[k + 2];
    const opacity = quads[k + 3];
    let el = state.labels.get(index);
    if (el === undefined) {
      el = makeLabel(index);
      state.labels.set(index, el);
      labelsEl.appendChild(el);
    }
    el.generation = state.labelGeneration;
    if (el.size === undefined || el.sizedAlt !== state.altDown) {
      el.size = [el.offsetWidth, el.offsetHeight];
      el.sizedAlt = state.altDown;
    }
    const [w, h] = el.size;
    shown.push({ el, x, y, opacity, w, h });
  }
  const { labelShown, plotTops } = placeOverlays({
    placed: placedBoxes,
    labels: shown.map(({ x, y, w, h }) => ({
      box: boxAt(x - w / 2, y + LABEL_OFFSET_PX, w, h, LABEL_PAD_PX),
    })),
    plots,
    width,
    height,
    pad: LABEL_PAD_PX,
  });
  shown.forEach(({ el, x, y, opacity }, i) => {
    el.style.visibility = labelShown[i] ? "visible" : "hidden";
    el.style.opacity = String(opacity);
    el.style.transform = `translate(${x}px, ${y}px) translate(-50%, ${LABEL_OFFSET_PX}px)`;
  });
  plots.forEach((plot, i) => showFlag(plot.el, plot.left, plotTops[i]));
  for (const [index, el] of state.labels) {
    if (el.generation !== state.labelGeneration) {
      el.remove();
      state.labels.delete(index);
    }
  }
}

function makeLabel(index) {
  const el = document.createElement("div");
  el.className = "label";
  el.textContent = state.app.machine_name(index);
  const files = state.app.machine_files(index);
  const count = document.createElement("span");
  count.className = "count";
  count.textContent = `${files} ${files === 1 ? "file" : "files"}`;
  el.appendChild(count);
  return el;
}

export function label(kind, id) {
  return `${ID_PREFIX[kind]}-${id}`;
}

// The marks come from the wasm side: each agent with its activity, the active order, each open
// clash and each question. They are rebuilt when the view changes, and placed every frame.
export function paintMarks() {
  state.markState = JSON.parse(state.app.marks_json());
  flagsEl.replaceChildren();
  state.flagEls = state.markState.flags.map((flag) => {
    const el = flagElement(flag);
    flagsEl.append(el);
    return el;
  });
  const unplaced = state.markState.unplaced;
  if (unplaced.length > 0) {
    const note = document.createElement("div");
    note.className = "flag unplaced";
    note.textContent = `scope names ${unplaced.length} machine${unplaced.length === 1 ? "" : "s"} this plant lacks: ${unplaced.join(", ")}`;
    flagsEl.append(note);
  }
  paintCard();
}

function flagElement(flag) {
  const el = document.createElement("div");
  el.className = `flag ${flag.kind}`;
  el.classList.toggle("leaving", flag.leaving);
  if (flag.hue) {
    const dot = document.createElement("span");
    dot.className = "dot";
    dot.style.background = flag.hue;
    el.append(dot);
  }
  if (flag.kind === "landed") {
    const dot = document.createElement("span");
    dot.className = "dot land";
    el.append(dot);
    el.classList.toggle("quiet", flag.text === "");
  }
  if (flag.text !== "") {
    const text = document.createElement("span");
    text.textContent = flag.text;
    el.append(text);
  }
  if (flag.check) el.append(checkChip(flag.check));
  if (flag.kind === "plot") el.classList.add(flag.text.includes(" confirmed ") ? "confirmed" : "draft");
  if (flag.scope) {
    const scope = document.createElement("span");
    scope.className = "scope";
    scope.textContent = state.altDown ? flag.scope : "in confirmed scope";
    el.title = flag.scope;
    el.append(scope);
  }
  if (state.altDown && flag.detail.length > 0) {
    const lines = document.createElement("span");
    lines.className = "detail";
    for (const line of flag.detail) {
      const row = document.createElement("span");
      row.textContent = line;
      lines.append(row);
    }
    el.append(lines);
  }
  if (flag.waiting) {
    const amber = document.createElement("span");
    amber.className = "dot wait";
    el.append(amber);
  }
  if (flag.clash !== null) {
    el.addEventListener("click", () => openCard(flag.clash));
  }
  return el;
}

// A witness chip: a grey ring while the check is pending, a dark tick when it passed, a red dot
// when it failed. Alt adds its words, such as `w3 · passed`.
function checkChip(check) {
  const chip = document.createElement("span");
  chip.className = `chip ${check.state}`;
  chip.title = `${check.name} · ${check.state}`;
  chip.append(document.createElement("i"));
  if (state.altDown) {
    const words = document.createElement("span");
    words.textContent = `${check.name} · ${check.state}`;
    chip.append(words);
  }
  return chip;
}

// Flags are placed before the names and are never hidden for overlap; a flag is hidden only when
// it is off the canvas. An agent's flags stack in a column above their machine's drones, each with
// a leader line to its drone's crown, and a flag never sits on a drone because the column starts
// above the crowns. A clash label starts to the right of its diamond, and the order flag sits
// above its machine. A flag that would cover a flag placed before it moves up clear of it. Each
// flag keeps its place as the drones and diamonds move. The plot flags are the exception: they are
// returned, not placed here, and placed after the names, so they move clear of a name.
function placeFlags(placedBoxes) {
  const plots = [];
  // The drones come first, so no flag or name is placed over one.
  const discs = state.app.drone_layout();
  for (let k = 0; k < discs.length; k += 3) {
    const [x, y, r] = [discs[k], discs[k + 1], discs[k + 2]];
    placedBoxes.push({ left: x - r, right: x + r, top: y - r, bottom: y + r });
  }
  const quads = state.app.flag_layout();
  const columns = new Map();
  for (let k = 0; k < quads.length; k += 4) {
    const el = state.flagEls[quads[k]];
    if (el === undefined) continue;
    el.hidden = isRiderFlag(state.markState.flags[quads[k]]);
    if (el.hidden) continue;
    const machine = quads[k + 1];
    const x = quads[k + 2];
    const y = quads[k + 3];
    if (el.size === undefined) el.size = [el.offsetWidth, el.offsetHeight];
    if (machine < 0 && el.classList.contains("plot")) {
      const [w, h] = el.size;
      plots.push({ el, left: x - w / 2, top: y - LABEL_OFFSET_PX - h, w, h });
      continue;
    }
    if (machine >= 0) {
      if (!columns.has(machine)) columns.set(machine, []);
      columns.get(machine).push({ el, x, y });
      continue;
    }
    const [w, h] = el.size;
    const beside = el.classList.contains("clash") || el.classList.contains("landed");
    const left = beside ? x : x - w / 2;
    const top = clearOf(
      left,
      beside ? y - h / 2 : y - LABEL_OFFSET_PX - h,
      w,
      h,
      placedBoxes,
      LABEL_PAD_PX,
    );
    placeFlag(el, left, top, placedBoxes);
  }
  let leaders = 0;
  for (const column of columns.values()) {
    const centre = column.reduce((sum, item) => sum + item.x, 0) / column.length;
    let bottom = Math.min(...column.map((item) => item.y)) - LABEL_OFFSET_PX;
    for (const item of column) {
      const [w, h] = item.el.size;
      const top = clearOf(centre - w / 2, bottom - h, w, h, placedBoxes, LABEL_PAD_PX);
      placeFlag(item.el, centre - w / 2, top, placedBoxes);
      drawLeader(leaders, centre, top + h, item.x, item.y);
      leaders += 1;
      bottom = top - FLAG_ROW_GAP_PX;
    }
  }
  for (let i = leaders; i < state.leaderEls.length; i += 1) {
    state.leaderEls[i].style.display = "none";
  }
  return plots;
}

function placeFlag(el, left, top, placedBoxes) {
  const [w, h] = el.size;
  placedBoxes.push(boxAt(left, top, w, h, LABEL_PAD_PX));
  showFlag(el, left, top);
}

// Shows a flag at (left, top), unless it, with its padding, is off the canvas.
function showFlag(el, left, top) {
  const [w, h] = el.size;
  const inside = inView(boxAt(left, top, w, h, LABEL_PAD_PX), canvas.clientWidth, canvas.clientHeight);
  el.style.visibility = inside ? "visible" : "hidden";
  el.style.transform = `translate(${left}px, ${top}px)`;
}

// A thin line from a flag's bottom edge to its bot's crown. The leaders sit under the flags, so
// a flag hides the start of its own line.
function drawLeader(index, x1, y1, x2, y2) {
  let line = state.leaderEls[index];
  if (line === undefined) {
    line = document.createElementNS(SVG_NS, "line");
    leadersEl.append(line);
    state.leaderEls[index] = line;
  }
  line.setAttribute("x1", String(x1));
  line.setAttribute("y1", String(y1));
  line.setAttribute("x2", String(x2));
  line.setAttribute("y2", String(y2));
  line.style.display = "";
}

// The pin whose tip lies within PIN_PICK_PX of the CSS point, nearest first, or null.
export function pinAt(x, y) {
  const quads = state.app.pin_layout();
  let found = null;
  let best = PIN_PICK_PX;
  for (let k = 0; k < quads.length; k += 3) {
    const distance = Math.hypot(quads[k + 1] - x, quads[k + 2] - y);
    if (distance < best) {
      best = distance;
      found = quads[k];
    }
  }
  return found;
}
