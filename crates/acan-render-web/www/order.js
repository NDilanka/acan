// The order a human posts on a machine: the command the compose panel sends, the sentence a
// refusal reads as, what the machine holds now, and where a machine sits on screen. This module
// has no imports and touches no DOM, so Node can test it. `project(x, y)` is the wasm side's
// `world_to_screen`: a ground point in world units to CSS pixels on the canvas.

export const EMPTY_LINE = "Write one sentence first. Nothing was sent.";

// A click this close to a machine's name picks the machine, for the faces a tall machine raises
// above its footprint.
export const LABEL_PICK_PX = 22;

// The compose panel sits this far to the side of its machine, and this far inside the canvas.
const SPOT_GAP_PX = 22;
const SPOT_MARGIN_PX = 12;

// The PostOrder body for one sentence on `machine`, or the line that says why none is sent. The
// sentence is sent on one line with its spaces collapsed. An order carries no witnesses: the
// scope-first lane drafts what the order may change and must keep, and a human seals it.
export function orderCommand(machine, text) {
  const sentence = String(text ?? "").replace(/\s+/g, " ").trim();
  if (sentence === "") return { ok: false, line: EMPTY_LINE };
  return { ok: true, command: { cmd: "post_order", machine, sentence, witnesses: [] } };
}

// The id of the order an accepted PostOrder minted, from the events the route answered, or null.
export function postedOrder(events) {
  if (!Array.isArray(events)) return null;
  const posted = events.find((stamped) => stamped?.event?.type === "order_posted");
  return posted === undefined ? null : posted.event.order;
}

// The plain sentence a refused PostOrder reads as. A 409 carries the origin's Rejection, which
// names its `reason`. The page never shows the JSON. Each sentence says nothing was posted, which
// holds because a refused command appends nothing.
export function orderRefusal(status, text) {
  const body = parseBody(text);
  if (status === 401) return "The owner token was not accepted, so nothing was posted. Sign in again.";
  if (status === 403) return "Only a member can post an order, and this token is not one. Nothing was posted.";
  if (status === 400 || status === 415 || status === 422) {
    return "The server could not read this order, so nothing was posted.";
  }
  if (status >= 500) return "The server failed to take the order. Try again.";
  switch (body?.reason) {
    case "forbidden":
      return "You are not a member of this world, so nothing was posted.";
    case "invalid":
      return `The server did not take it: ${sentence(body.why)} Nothing was posted.`;
    case "wrong_state":
      return `The ${body.what ?? "order"} is ${body.found ?? "in another state"}, so nothing was posted.`;
    case "unknown":
      return `The server does not know this ${body.what ?? "machine"}, so nothing was posted.`;
    default:
      return "The server refused the order. Nothing was posted.";
  }
}

// What the machine holds now, in one line: its orders still in flight and the agents on it.
export function machineContext(view, name) {
  const orders = view.orders.filter((order) => order.machine === name
    && order.phase !== "landed" && order.phase !== "cancelled");
  const agents = view.agents.filter((agent) => agent.machine === name);
  const parts = [];
  if (orders.length > 0) {
    parts.push(orders.map((order) => `o-${order.id} ${order.phase.replace(/_/g, " ")}`).join(", "));
  }
  if (agents.length > 0) {
    parts.push(`${agents.length} ${agents.length === 1 ? "agent" : "agents"} here`);
  }
  return parts.length === 0 ? "Nothing is in flight here." : `Here now: ${parts.join(" · ")}.`;
}

// The order the slip shows the scope of: the order this page posted
// last, while it is in flight, so a new order is named even while an older one is open. Else the
// first order in flight, else the newest. `posted` is the id this page posted last, or null.
export function shownOrderOf(view, posted = null) {
  const inFlight = (order) => order.phase !== "cancelled" && order.phase !== "landed";
  const mine = posted === null ? undefined : view.orders.find((order) => order.id === posted && inFlight(order));
  return mine ?? view.orders.find(inFlight) ?? view.orders.at(-1);
}

// The machine's footprint on screen, as its four ground corners.
export function machineQuad(machine, project) {
  const [x, y] = machine.min;
  const [w, d] = machine.size;
  return [[x, y], [x + w, y], [x + w, y + d], [x, y + d]].map(([a, b]) => {
    const p = project(a, b);
    return [p[0], p[1]];
  });
}

// The machine's centre on screen.
export function machineCentre(machine, project) {
  const p = project(machine.min[0] + machine.size[0] / 2, machine.min[1] + machine.size[1] / 2);
  return { x: p[0], y: p[1] };
}

// Whether (x, y) lies inside the polygon, by the even-odd rule.
export function insideQuad(quad, x, y) {
  let inside = false;
  for (let i = 0, j = quad.length - 1; i < quad.length; j = i, i += 1) {
    const [xi, yi] = quad[i];
    const [xj, yj] = quad[j];
    if ((yi > y) !== (yj > y) && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi) inside = !inside;
  }
  return inside;
}

function quadArea(quad) {
  let sum = 0;
  for (let i = 0, j = quad.length - 1; i < quad.length; j = i, i += 1) {
    sum += quad[j][0] * quad[i][1] - quad[i][0] * quad[j][1];
  }
  return Math.abs(sum) / 2;
}

// How many CSS pixels one world unit of height spans over the ground point (x, y). The camera
// never rolls, so a vertical edge stays vertical on screen. The ground's screen x changes at the
// full scale and its screen y at the scale shortened by the tilt, and the height takes the rest.
export function riseScale(project, x, y) {
  const o = project(x, y);
  const a = project(x + 1, y);
  const b = project(x, y + 1);
  const full = Math.hypot(a[0] - o[0], b[0] - o[0]);
  const tilted = Math.hypot(a[1] - o[1], b[1] - o[1]);
  return Math.sqrt(Math.max(0, full * full - tilted * tilted));
}

// Without the model heights from the wasm side, a machine is taken to rise this share of its
// footprint's shorter side, which the demo plant's models come close to.
const RISE_PER_SIDE = 0.8;

export function guessTop(machine) {
  return Math.min(machine.size[0], machine.size[1]) * RISE_PER_SIDE;
}

// The machine's outline on screen as a box `top` world units tall: the hull of its footprint and
// the footprint raised to its top.
export function raisedOutline(machine, project, top) {
  const quad = machineQuad(machine, project);
  const rise = top * riseScale(project, machine.min[0] + machine.size[0] / 2, machine.min[1] + machine.size[1] / 2);
  if (!(rise > 0)) return quad;
  return hull([...quad, ...quad.map(([px, py]) => [px, py - rise])]);
}

// The convex hull of the points, counter-clockwise on screen, by the monotone chain.
export function hull(points) {
  const sorted = [...points].sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  if (sorted.length < 3) return sorted;
  const cross = (o, a, b) => (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
  const half = (list) => {
    const out = [];
    for (const p of list) {
      while (out.length >= 2 && cross(out.at(-2), out.at(-1), p) <= 0) out.pop();
      out.push(p);
    }
    out.pop();
    return out;
  };
  return [...half(sorted), ...half([...sorted].reverse())];
}

// The name of the machine under the CSS point, or null. Each machine is judged by its raised
// outline, so a click on a tall face picks it. `topOf(machine)` gives a machine's height in world
// units, else `guessTop` does. When outlines overlap, a machine that stands on another's
// footprint wins over that one, and otherwise the frontmost wins: the one whose footprint reaches
// lowest on screen, as the nearer of two machines does. Else a shown name within LABEL_PICK_PX
// picks its machine. `labels` are `{ name, x, y }`, the centre of each name.
export function machineAt(machines, project, x, y, labels = [], topOf = guessTop) {
  const hits = [];
  for (const machine of machines) {
    const outline = raisedOutline(machine, project, topOf(machine));
    if (!insideQuad(outline, x, y)) continue;
    const quad = machineQuad(machine, project);
    hits.push({ name: machine.name, quad, front: Math.max(...quad.map((p) => p[1])), area: quadArea(quad) });
  }
  const onTop = hits.filter((hit) => !hits.some((other) => other !== hit
    && other.area < hit.area && other.quad.every(([px, py]) => insideQuad(hit.quad, px, py))));
  onTop.sort((a, b) => b.front - a.front || a.area - b.area);
  if (onTop.length > 0) return onTop[0].name;
  let found = null;
  let best = LABEL_PICK_PX;
  for (const label of labels) {
    const distance = Math.hypot(label.x - x, label.y - y);
    if (distance < best) {
      best = distance;
      found = label.name;
    }
  }
  return found;
}

// The machine whose centre is nearest the middle of the canvas and on it, or null: the one the
// camera looks at, which "order here" means when nothing is picked.
export function centreMachine(machines, project, width, height) {
  let found = null;
  let best = Infinity;
  for (const machine of machines) {
    const { x, y } = machineCentre(machine, project);
    if (x < 0 || y < 0 || x > width || y > height) continue;
    const distance = Math.hypot(x - width / 2, y - height / 2);
    if (distance < best) {
      best = distance;
      found = machine.name;
    }
  }
  return found;
}

// Where a panel of size `w` by `h` sits beside the point `at`: to its right when it fits, else to
// its left, centred on it, and always inside the band from `from` to `width` across the canvas.
export function composeSpot(at, w, h, width, height, from = 0) {
  let left = at.x + SPOT_GAP_PX;
  let side = "right";
  if (left + w > width - SPOT_MARGIN_PX) {
    left = at.x - SPOT_GAP_PX - w;
    side = "left";
  }
  left = clamp(left, from + SPOT_MARGIN_PX, width - w - SPOT_MARGIN_PX);
  const top = clamp(at.y - h / 2, SPOT_MARGIN_PX, height - h - SPOT_MARGIN_PX);
  return { left, top, side };
}

function clamp(value, low, high) {
  return high < low ? low : Math.min(Math.max(value, low), high);
}

function parseBody(text) {
  try {
    const body = JSON.parse(text);
    return body !== null && typeof body === "object" ? body : null;
  } catch {
    return null;
  }
}

function sentence(text) {
  const plain = typeof text === "string" && text.length > 0 ? text : "the order is not valid";
  return /[.!?]$/.test(plain) ? plain : `${plain}.`;
}
