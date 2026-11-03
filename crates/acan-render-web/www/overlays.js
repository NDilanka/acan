// The project-tagged overlay layouts the wasm side gives for a canvas of several projects, read
// with no DOM, so Node can test them. Each flat layout starts every row with the project's place
// in `canvas_order`; parked.js and multi.js place and pick what these return.

// Splits a flat layout of rows `stride` long, each led by a project index, into a Map from that
// index to its rows without it. Rows of project `skip` are left out: the project on screen draws
// its own marks.
export function splitByProject(flat, stride, skip = -1) {
  const out = new Map();
  for (let k = 0; k + stride <= flat.length; k += stride) {
    const project = flat[k];
    if (project === skip) continue;
    if (!out.has(project)) out.set(project, []);
    out.get(project).push(Array.from(flat.slice(k + 1, k + stride)));
  }
  return out;
}

// The parked projects' marks as `all_marks_json` lists them, without the project on screen.
export function parkedEntries(entries, active) {
  return entries.filter((entry) => entry.id !== active);
}

// The clash pin of a parked project whose tip lies within `pick` CSS px of (x, y), nearest first,
// as `{ project, clash }` with the project's index, or null. `quads` is `all_pin_layout`.
export function pinNear(quads, x, y, skip, pick) {
  let found = null;
  let best = pick;
  for (let k = 0; k + 4 <= quads.length; k += 4) {
    if (quads[k] === skip) continue;
    const distance = Math.hypot(quads[k + 2] - x, quads[k + 3] - y);
    if (distance < best) {
      best = distance;
      found = { project: quads[k], clash: quads[k + 1] };
    }
  }
  return found;
}

// What a click at a point does on a parked plant, or null when it hits nothing there and the
// project on screen takes the click as it always did. A pin wins over a drone, as it does on the
// project on screen. `pin` is `pinNear`'s answer and `drone` is `all_drone_at`'s, parsed, or null;
// `order` is `canvas_order`. Returns `{ kind: "pin", id, clash }` or `{ kind: "drone", id, agent }`.
export function parkedClick(pin, drone, order, active) {
  if (pin !== null && order[pin.project] !== undefined && order[pin.project] !== active) {
    return { kind: "pin", id: order[pin.project], clash: pin.clash };
  }
  if (drone !== null && drone.id !== active) return { kind: "drone", id: drone.id, agent: drone.agent };
  return null;
}

// A project's flag anchors, `[flag index, machine or -1, x, y]` rows, as the agent columns keyed
// by machine and the flags that stand alone.
export function flagGroups(rows) {
  const columns = new Map();
  const alone = [];
  for (const [index, machine, x, y] of rows) {
    if (machine >= 0) {
      if (!columns.has(machine)) columns.set(machine, []);
      columns.get(machine).push({ index, x, y });
    } else {
      alone.push({ index, x, y });
    }
  }
  return { columns, alone };
}

// The flag kinds a parked plant shows. A clash, an order, a plot and a tour stage wait on the
// project being on screen: their words open on a hover or with the card the project on screen
// owns, so a parked plant shows its agents, its landed and away dots and its questions only.
export function parkedKind(kind) {
  return kind === "agent" || kind === "landed" || kind === "away" || kind === "question";
}

// The leader from a parked agent flag to its drone's crown at `anchor`, in the on-screen
// project's rule: a lone flag that kept its wanted spot points with its own pointer and needs
// none; any other draws one from its bottom edge, `inset` in from its left when lone, else from
// its middle. `spot` is `{ left, top, w, h }`. Returns `{ x1, y1, x2, y2 }` or null.
export function flagLeader(spot, wantLeft, wantTop, anchor, single, inset) {
  if (single && spot.left === wantLeft && spot.top === wantTop) return null;
  return { x1: spot.left + (single ? inset : spot.w / 2), y1: spot.top + spot.h, x2: anchor.x, y2: anchor.y };
}

function overlapsBox(a, b) {
  return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
}

// Where each project's plate shows. `plates` are `{ id, x, y, w, h }` in the order they claim
// room, the project on screen first: each wants its top centre at (x, y). A plate that would
// cover a box in `blocked` (the names and flags already placed) or a plate placed before it moves
// down past it, so plates of plants that sit close on screen stack instead of piling up, and
// none covers a machine's name. Returns a Map from id to `{ left, top }`.
export function plateSpots(plates, blocked, pad) {
  const placed = [...blocked];
  const out = new Map();
  for (const { id, x, y, w, h } of plates) {
    const left = x - w / 2;
    let top = y;
    let moved = true;
    while (moved) {
      moved = false;
      for (const box of placed) {
        const mine = { left: left - pad, right: left + w + pad, top: top - pad, bottom: top + h + pad };
        if (overlapsBox(mine, box)) {
          top = box.bottom + pad;
          moved = true;
        }
      }
    }
    placed.push({ left, right: left + w, top, bottom: top + h });
    out.set(id, { left, top });
  }
  return out;
}
