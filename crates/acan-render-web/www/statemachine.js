// The state machines in the map: where their dials, stations and belts stand on the ground, which
// transition a touch names, and which belts are lit at a given time. A state machine is one entry
// of the plant payload's `state_machines`, and a touch is a `transition_touched` frame of the
// stream. This module touches no DOM, so Node can test it. The painter is `statemachine-paint.js`.

// A machine with at most this many states lays its stations in a row; more sit on a ring.
export const ROW_MAX = 6;
// At most this many dials stand on one building's roof; the rest become a "+N" chip.
export const DIAL_MAX = 3;
// The screen gap between dials on one roof, in CSS px.
const DIAL_GAP_PX = 17;
// How long a held belt stays lit with no landing, and how long a landed belt takes to fade.
export const HELD_MS = 15 * 60 * 1000;
export const FADE_MS = 4000;
// Most light records kept. A page holds a few claims at a time, so this is never reached by one.
const RECORDS_MAX = 200;
// Points sampled along a belt.
const SAMPLES = 10;

// The machines of a plant payload's `state_machines`, as the spec lists them: a machine has at least
// two states and one transition, and a payload without the field has none. Other entries are dropped.
export function readStateMachines(text) {
  // Most plants carry no machines, so the text is searched before it is parsed.
  if (!text.includes('"state_machines"')) return [];
  let list;
  try {
    list = JSON.parse(text)?.state_machines;
  } catch {
    return [];
  }
  if (!Array.isArray(list)) return [];
  return list.filter(
    (machine) => typeof machine?.module === "string"
      && Array.isArray(machine.states) && machine.states.length >= 2
      && Array.isArray(machine.transitions) && machine.transitions.length >= 1,
  );
}

// The frame when `text` is a transition touch, else null. Most stream text is an event, so the
// text is checked before it is parsed.
export function touchOf(text) {
  if (!text.includes('"transition_touched"')) return null;
  try {
    const frame = JSON.parse(text);
    return frame?.type === "transition_touched" ? frame : null;
  } catch {
    return null;
  }
}

// The plan size of a station tile: a fifth of the building's shorter side, and at least one unit.
export function tileSize(footprint) {
  return Math.max(1, Math.min(footprint.size[0], footprint.size[1]) * 0.2);
}

// Where each state's station stands on the ground, in plan units, in the order of `states`. At most
// ROW_MAX states sit in a row in front of the building, centred on it. More sit on a ring round it.
// `footprint` is a building's `{ min, size }` from the view.
export function stationLayout(states, footprint) {
  const [x, y] = footprint.min;
  const [w, d] = footprint.size;
  const tile = tileSize(footprint);
  const n = states.length;
  if (n <= ROW_MAX) {
    const gap = tile * 1.6;
    const left = x + w / 2 - ((n - 1) * gap) / 2;
    const row = y + d + tile * 1.2;
    return states.map((state, i) => ({ name: state.name, x: left + i * gap, y: row }));
  }
  const cx = x + w / 2;
  const cy = y + d / 2;
  const rx = w / 2 + tile * 1.6;
  const ry = d / 2 + tile * 1.6;
  return states.map((state, i) => {
    const angle = -Math.PI / 2 + (2 * Math.PI * i) / n;
    return { name: state.name, x: cx + rx * Math.cos(angle), y: cy + ry * Math.sin(angle) };
  });
}

// Points along a curve from `a` to `b`, both ends included. The control point sits to one side of
// the chord, so a belt and its reverse bow apart and do not lie on each other.
export function curvePoints(a, b, bow = 0.2) {
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  const length = Math.hypot(dx, dy) || 1;
  const cx = (a.x + b.x) / 2 - (dy / length) * length * bow;
  const cy = (a.y + b.y) / 2 + (dx / length) * length * bow;
  const points = [];
  for (let i = 0; i <= SAMPLES; i += 1) {
    const t = i / SAMPLES;
    const u = 1 - t;
    points.push({
      x: u * u * a.x + 2 * u * t * cx + t * t * b.x,
      y: u * u * a.y + 2 * u * t * cy + t * t * b.y,
    });
  }
  return points;
}

// A transition from a state to itself: a small loop that starts and ends at the station, on the
// side away from the building.
export function loopPoints(station, tile) {
  const r = tile * 0.5;
  const cx = station.x;
  const cy = station.y + r;
  const points = [];
  for (let i = 0; i <= SAMPLES; i += 1) {
    const angle = (2 * Math.PI * i) / SAMPLES;
    points.push({ x: cx + r * Math.sin(angle), y: cy - r * Math.cos(angle) });
  }
  return points;
}

// The stub of a transition from "*", whose source the scan cannot name: a short straight run that
// enters the target station from the side the building lies away from.
export function stubPoints(station, centre, tile) {
  const dx = station.x - centre.x;
  const dy = station.y - centre.y;
  const length = Math.hypot(dx, dy) || 1;
  const start = { x: station.x + (dx / length) * tile, y: station.y + (dy / length) * tile };
  const points = [];
  for (let i = 0; i <= 4; i += 1) {
    const t = i / 4;
    points.push({ x: start.x + (station.x - start.x) * t, y: start.y + (station.y - start.y) * t });
  }
  return points;
}

// The stations and belts of one state machine on the ground. Each transition with both of its
// states known becomes a belt, in the order of `transitions`; a transition from "*" becomes a stub.
// A transition that names a state the machine does not list is left out and counted.
export function beltsOf(machine, footprint) {
  const stations = stationLayout(machine.states, footprint);
  const byName = new Map(stations.map((station) => [station.name, station]));
  const centre = { x: footprint.min[0] + footprint.size[0] / 2, y: footprint.min[1] + footprint.size[1] / 2 };
  const tile = tileSize(footprint);
  const low = machine.confidence === "low";
  const belts = [];
  let left = 0;
  machine.transitions.forEach((transition, index) => {
    const to = byName.get(transition.to);
    if (to === undefined) {
      left += 1;
      return;
    }
    const belt = { index, from: transition.from, to: transition.to, via: transition.via ?? null, low };
    if (transition.from === "*") {
      belts.push({ ...belt, stub: true, points: stubPoints(to, centre, tile) });
      return;
    }
    const from = byName.get(transition.from);
    if (from === undefined) {
      left += 1;
      return;
    }
    const points = from === to ? loopPoints(to, tile) : curvePoints(from, to);
    belts.push({ ...belt, stub: false, points });
  });
  return { stations, belts, left };
}

// The dial's ticks, one per state, spread over a 270-degree arc from the lower left. `lit` is the
// index of the tick that is lit, or -1 for none. Angles are degrees clockwise from straight up.
export function dialTicks(count, lit) {
  const ticks = [];
  for (let i = 0; i < count; i += 1) {
    const angle = count === 1 ? 0 : -135 + (270 * i) / (count - 1);
    ticks.push({ angle, lit: i === lit });
  }
  return ticks;
}

// The dials one building's roof carries: the first DIAL_MAX machines, and how many more there are.
export function stackPlan(machines) {
  return { shown: machines.slice(0, DIAL_MAX), more: Math.max(0, machines.length - DIAL_MAX) };
}

// The index of the transition a touch names: the one in the same file on the same lines, else the
// one with the same from and to. -1 when the machine has neither.
export function transitionIndex(machine, touch) {
  const exact = machine.transitions.findIndex((t) => t.file === touch.file
    && t.line === touch.line && t.end_line === touch.end_line);
  if (exact >= 0) return exact;
  return machine.transitions.findIndex((t) => t.from === touch.from && t.to === touch.to);
}

// The page's state machines and the light its belts show. The light is kept as records, one per
// touch: a claim or a build holds its belt, and a landing starts the belt's fade. Only the latest
// record of a belt decides its light. Live touches come from the stream, and a replay derives the
// same records from the claims and landings in its log, since touches are not in the log.
let machines = [];
let records = [];
// The recorded time of the last log entry a replay folded, or null on a live page. Recorded times
// are epoch milliseconds, as `Date.now()` is, so the light runs on one clock either way.
let replayAt = null;

// Sets the page's machines. The light is kept: records name their machine by id, so a machine the
// new list lacks simply has no belt to light.
export function setStateMachines(list) {
  machines = list;
}

export function allStateMachines() {
  return machines;
}

export function stateMachinesOf(building) {
  return machines.filter((machine) => machine.module === building);
}

// The time the light is drawn at: the replay's recorded time, or the wall clock on a live page.
export function lightNow() {
  return replayAt ?? Date.now();
}

// Forgets the light and keeps the machines, as a replay does when it starts again.
export function resetLight() {
  records = [];
  replayAt = null;
}

function keep(record) {
  records.push(record);
  if (records.length > RECORDS_MAX) records = records.slice(-RECORDS_MAX);
}

// Notes one streamed touch at time `now`. Returns false for a touch of a machine the page does not
// hold, which is then not lit. The touch is live only, so it is not folded into the log.
export function noteTouch(frame, now) {
  const machine = machines.find((m) => m.id === frame.machine);
  if (machine === undefined) return false;
  const index = transitionIndex(machine, frame);
  if (index < 0) return false;
  keep({ machine: machine.id, index, task: frame.task, stage: frame.stage, at: now });
  return true;
}

// Notes a log entry for a replay: a claim holds the belts of the files it declares, and a landing
// fades the belts that task held. Other entries do nothing.
export function noteLogEntry(entry) {
  const event = entry?.event;
  if (Number.isFinite(entry?.at)) replayAt = entry.at;
  if (event?.type === "declared") {
    const files = event.decl?.footprint?.files ?? [];
    for (const machine of machines) {
      machine.transitions.forEach((transition, index) => {
        if (files.includes(transition.file)) {
          keep({ machine: machine.id, index, task: event.decl.task, stage: "declared", at: entry.at });
        }
      });
    }
  } else if (event?.type === "landed") {
    const held = records.filter((r) => r.task === event.task && r.stage !== "landed");
    const seen = new Set();
    for (const r of held) {
      const key = `${r.machine}#${r.index}`;
      if (seen.has(key)) continue;
      seen.add(key);
      keep({ machine: r.machine, index: r.index, task: event.task, stage: "landed", at: entry.at });
    }
  }
}

// The light each belt shows at `now`, keyed by `${machine id}#${transition index}`:
// `{ kind: "held", task }` while a claim or a build holds it, and `{ kind: "fade", alpha }` for a
// landed belt fading out. A belt with no entry is paper.
export function lightAt(now) {
  const latest = new Map();
  for (const r of records) latest.set(`${r.machine}#${r.index}`, r);
  const light = new Map();
  for (const [key, r] of latest) {
    const age = now - r.at;
    if (age < 0) continue;
    if (r.stage === "landed") {
      if (age < FADE_MS) light.set(key, { kind: "fade", alpha: 1 - age / FADE_MS });
    } else if (age < HELD_MS) {
      light.set(key, { kind: "held", task: r.task });
    }
  }
  return light;
}

// The index in `states` of the state the machine's latest touch lands in, the `to` of the
// transition it names. -1 when no touch of the machine is kept.
export function lastTouchedState(machine) {
  for (let i = records.length - 1; i >= 0; i -= 1) {
    const r = records[i];
    if (r.machine !== machine.id) continue;
    const to = machine.transitions[r.index]?.to;
    return machine.states.findIndex((state) => state.name === to);
  }
  return -1;
}

// Each further machine of a building stands this many tile sizes further out in front of it.
const STACK_TILES = 2.5;

// The marks one frame draws. A dial stands on each shown machine's building roof, and at near
// detail the stations and belts of every machine stand on the ground in front of the building.
// The callbacks come from the page: `project(x, y)` is the screen point of a ground point,
// `riseAt(x, y)` the screen px per world unit of height there, and `topOf(building)` the roof's
// height in world units. `buildings` are the view's machines. Returns the shapes in screen px,
// and `fading`, whether any belt is still fading.
export function sceneOf({ buildings, machines, project, riseAt, topOf, near, now }) {
  const byModule = new Map();
  for (const machine of machines) {
    const list = byModule.get(machine.module);
    if (list === undefined) byModule.set(machine.module, [machine]);
    else list.push(machine);
  }
  const light = lightAt(now);
  const scene = { dials: [], more: [], stations: [], belts: [], fading: false };
  for (const building of buildings) {
    const list = byModule.get(building.name);
    if (list === undefined) continue;
    const { shown, more } = stackPlan(list);
    const [x, y] = building.min;
    const [w, d] = building.size;
    const cx = x + w / 2;
    const cy = y + d / 2;
    const ground = project(cx, cy);
    const hub = [ground[0], ground[1] - topOf(building) * riseAt(cx, cy)];
    shown.forEach((machine, i) => {
      const dx = (i - (shown.length - 1) / 2) * DIAL_GAP_PX;
      scene.dials.push({ x: hub[0] + dx, y: hub[1], ticks: dialTicks(machine.states.length, lastTouchedState(machine)) });
    });
    if (more > 0) scene.more.push({ x: hub[0] + ((shown.length + 1) / 2) * DIAL_GAP_PX, y: hub[1], text: `+${more}` });
    if (!near) continue;
    const tile = tileSize({ min: [x, y], size: [w, d] });
    list.forEach((machine, k) => {
      const layout = beltsOf(machine, { min: [x, y + k * tile * STACK_TILES], size: [w, d] });
      for (const station of layout.stations) {
        const p = project(station.x, station.y);
        scene.stations.push({ x: p[0], y: p[1], name: station.name });
      }
      for (const belt of layout.belts) {
        const lit = light.get(`${machine.id}#${belt.index}`) ?? null;
        const points = belt.points.map((point) => {
          const p = project(point.x, point.y);
          return [p[0], p[1]];
        });
        scene.belts.push({ low: belt.low, stub: belt.stub, points, lit });
      }
    });
  }
  scene.fading = [...light.values()].some((entry) => entry.kind === "fade");
  return scene;
}
