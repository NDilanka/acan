// The projects map's pure parts: the plant boxes the wasm side reads out, the fit of the world into
// the map, the footprints, the view's ground, the keys and the words. It has no imports and
// touches no DOM, so Node can test it. minimap.js binds it to the page.

// Whether the map is folded to its toggle, in local storage: "collapsed", or absent for open.
export const MINIMAP_KEY = "acan.minimap";

// The room the fit keeps inside the map's edges, in CSS px.
export const MAP_PAD_PX = 8;

// The least world width the map spans, in plan units, so a lone small plant is not drawn huge.
export const MIN_SPAN = 400;

// A footprint is never drawn smaller than this on either side, in CSS px.
export const MIN_FOOT_PX = 6;

// A press that moves less than this is a click.
export const CLICK_PX = 4;

// The fit is kept while the plants fill at least this share of what a fresh fit would fill.
export const KEEP_SHARE = 0.6;

// The boxes of `canvas_boxes()`, five floats per project, paired with the ids of `canvas_order()`
// as `{ id, box, built }`. A box with a NaN (a project with no plant) or with no id is left out,
// and `built` is kept between 0 and 1.
export function readBoxes(floats, order) {
  const out = [];
  const count = Math.floor((floats?.length ?? 0) / 5);
  for (let i = 0; i < count; i += 1) {
    const id = order?.[i];
    if (typeof id !== "string" || id === "") continue;
    const k = i * 5;
    const box = [floats[k], floats[k + 1], floats[k + 2], floats[k + 3]];
    if (!box.every(Number.isFinite) || box[2] < box[0] || box[3] < box[1]) continue;
    const raw = floats[k + 4];
    const built = Number.isFinite(raw) ? Math.min(1, Math.max(0, raw)) : 1;
    out.push({ id, box, built });
  }
  return out;
}

// The map shows on a canvas of two plants or more.
export function shouldShow(entries) {
  return entries.length >= 2;
}

// The box round every entry's box, or null for none.
export function union(entries) {
  if (entries.length === 0) return null;
  const out = [Infinity, Infinity, -Infinity, -Infinity];
  for (const { box } of entries) {
    out[0] = Math.min(out[0], box[0]);
    out[1] = Math.min(out[1], box[1]);
    out[2] = Math.max(out[2], box[2]);
    out[3] = Math.max(out[3], box[3]);
  }
  return out;
}

// The transform that centres the world box `box` in a map `w` by `h`, inside the padding:
// map px = world * s + t. The view's footprint is never part of the fit, so a pan does not refit.
export function fitWorld(box, [w, h]) {
  if (box === null) return null;
  const room = [Math.max(1, w - 2 * MAP_PAD_PX), Math.max(1, h - 2 * MAP_PAD_PX)];
  const s = Math.min(room[0] / Math.max(box[2] - box[0], 1e-6), room[1] / Math.max(box[3] - box[1], 1e-6), room[0] / MIN_SPAN);
  return { s, tx: w / 2 - s * (box[0] + box[2]) / 2, ty: h / 2 - s * (box[1] + box[3]) / 2 };
}

// Whether transform `t` still holds the world box `box`: it lies inside the padded map and fills
// at least KEEP_SHARE of what a fresh fit fills on its binding axis. A drag of one plant inside
// the map then leaves the others where they are drawn.
export function keepFit(t, box, size) {
  if (!t || box === null) return false;
  const [w, h] = size;
  const [left, top] = toMap(t, [box[0], box[1]]);
  const [right, bottom] = toMap(t, [box[2], box[3]]);
  const slack = 0.01;
  if (left < MAP_PAD_PX - slack || top < MAP_PAD_PX - slack) return false;
  if (right > w - MAP_PAD_PX + slack || bottom > h - MAP_PAD_PX + slack) return false;
  const fresh = fitWorld(box, size);
  return t.s >= KEEP_SHARE * fresh.s && t.s <= fresh.s * (1 + 1e-6);
}

export function toMap(t, [x, y]) {
  return [x * t.s + t.tx, y * t.s + t.ty];
}

export function toWorld(t, [x, y]) {
  return [(x - t.tx) / t.s, (y - t.ty) / t.s];
}

// "built" when every machine is on the map, "unmapped" before any is, "filling" between.
function lookOf(built) {
  if (built >= 1) return "built";
  return built <= 0 ? "unmapped" : "filling";
}

// One side of a footprint, at least MIN_FOOT_PX long and centred on the box when it is shorter.
function side(a, b) {
  const lo = Math.min(a, b);
  const hi = Math.max(a, b);
  if (hi - lo >= MIN_FOOT_PX) return [lo, hi - lo];
  return [(lo + hi) / 2 - MIN_FOOT_PX / 2, MIN_FOOT_PX];
}

// Each entry's box on the map, in CSS px, with its built share and how it looks.
export function footprints(entries, t) {
  return entries.map(({ id, box, built }) => {
    const [x0, y0] = toMap(t, [box[0], box[1]]);
    const [x1, y1] = toMap(t, [box[2], box[3]]);
    const [left, width] = side(x0, x1);
    const [top, height] = side(y0, y1);
    return { id, left, top, width, height, built, look: lookOf(built) };
  });
}

// The view of `minimap_view()` on the map: its four corners in screen order, whether its top
// edge is cut, and the pivot. Null for an empty or broken read.
export function viewShape(floats, t) {
  if (!floats || floats.length < 11 || !t) return null;
  const corners = [];
  for (let k = 0; k < 8; k += 2) {
    if (!Number.isFinite(floats[k]) || !Number.isFinite(floats[k + 1])) return null;
    corners.push(toMap(t, [floats[k], floats[k + 1]]));
  }
  return { points: corners, cut: floats[8] >= 0.5, pivot: toMap(t, [floats[9], floats[10]]) };
}

function centroid(points) {
  const sum = points.reduce((acc, p) => [acc[0] + p[0], acc[1] + p[1]], [0, 0]);
  return [sum[0] / points.length, sum[1] / points.length];
}

// Whether `p` lies in the convex polygon `points`, its edges included, in either winding.
export function insideQuad(points, p) {
  if (points.length < 3) return false;
  let sign = 0;
  for (let i = 0; i < points.length; i += 1) {
    const a = points[i];
    const b = points[(i + 1) % points.length];
    const cross = (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
    if (Math.abs(cross) < 1e-9) continue;
    if (sign === 0) sign = Math.sign(cross);
    else if (Math.sign(cross) !== sign) return false;
  }
  return true;
}

// The tick that shows where the screen's top is: `len` px from the middle of the top edge,
// square to it and away from the polygon's middle.
export function upTick(points, len) {
  const [tl, tr] = points;
  const from = [(tl[0] + tr[0]) / 2, (tl[1] + tr[1]) / 2];
  const middle = centroid(points);
  const out = [from[0] - middle[0], from[1] - middle[1]];
  let n = [tr[1] - tl[1], tl[0] - tr[0]];
  if (Math.hypot(n[0], n[1]) < 1e-9) n = out;
  if (n[0] * out[0] + n[1] * out[1] < 0) n = [-n[0], -n[1]];
  const length = Math.hypot(n[0], n[1]) || 1;
  return { from, to: [from[0] + (n[0] / length) * len, from[1] + (n[1] / length) * len] };
}

function project(points, axis) {
  let lo = Infinity;
  let hi = -Infinity;
  for (const p of points) {
    const d = p[0] * axis[0] + p[1] * axis[1];
    lo = Math.min(lo, d);
    hi = Math.max(hi, d);
  }
  return [lo, hi];
}

// Whether the convex polygon and the map box share any point, by separating axes.
function meetsBox(points, w, h) {
  const box = [[0, 0], [w, 0], [w, h], [0, h]];
  const axes = [[1, 0], [0, 1]];
  points.forEach((a, i) => {
    const b = points[(i + 1) % points.length];
    axes.push([b[1] - a[1], a[0] - b[0]]);
  });
  return axes.every((axis) => {
    const [p0, p1] = project(points, axis);
    const [b0, b1] = project(box, axis);
    return p0 <= b1 && b0 <= p1;
  });
}

// Where to point at a view that lies wholly off the map: the border point, `inset` px in, nearest
// the view's pivot, and the angle in degrees from it towards the view (0 points right, 90 down).
// Null while any of the view is on the map.
export function offMap(shape, [w, h], inset = 0) {
  if (shape === null || meetsBox(shape.points, w, h)) return null;
  const pivot = shape.pivot;
  const outside = Number.isFinite(pivot?.[0]) && Number.isFinite(pivot?.[1])
    && (pivot[0] < 0 || pivot[1] < 0 || pivot[0] > w || pivot[1] > h);
  const aim = outside ? pivot : centroid(shape.points);
  const x = Math.min(w - inset, Math.max(inset, aim[0]));
  const y = Math.min(h - inset, Math.max(inset, aim[1]));
  return { x, y, angle: (Math.atan2(aim[1] - y, aim[0] - x) * 180) / Math.PI };
}

// The id of the box that holds the point, else of the closest one, the first on a tie; null for
// none. The wasm side's `canvas::nearest` picks the same way.
export function nearestId(entries, p) {
  let best = null;
  let distance = Infinity;
  for (const { id, box } of entries) {
    const dx = Math.max(box[0] - p[0], p[0] - box[2], 0);
    const dy = Math.max(box[1] - p[1], p[1] - box[3], 0);
    const d = Math.hypot(dx, dy);
    if (d < distance) {
      best = id;
      distance = d;
    }
  }
  return best;
}

// The ids of footprints in the order the arrow keys walk them: left to right by centre, then top
// to bottom.
export function focusOrder(rects) {
  const cx = (r) => r.left + r.width / 2;
  const cy = (r) => r.top + r.height / 2;
  return [...rects].sort((a, b) => cx(a) - cx(b) || cy(a) - cy(b)).map((r) => r.id);
}

// The index the key moves focus to among `count` items from `index` (-1 for none): the arrows
// wrap, Home and End go to the ends. Null for any other key.
export function rovingNext(count, index, key) {
  if (count <= 0) return null;
  switch (key) {
    case "ArrowRight":
    case "ArrowDown":
      return (index + 1 + count) % count;
    case "ArrowLeft":
    case "ArrowUp":
      return index < 0 ? count - 1 : (index - 1 + count) % count;
    case "Home":
      return 0;
    case "End":
      return count - 1;
    default:
      return null;
  }
}

// A plant's words: `text` for the map's head, such as "api · mapping 40%", and `aria` for its
// button, such as "api, on screen, mapping 40%".
export function labelOf({ name, here = false, built = 1, degraded = false }) {
  let status = "";
  if (degraded) status = "degraded";
  else if (built <= 0) status = "not mapped yet";
  else if (built < 1) status = `mapping ${Math.min(99, Math.max(1, Math.floor(built * 100)))}%`;
  return {
    text: status === "" ? name : `${name} · ${status}`,
    aria: [name, here ? "on screen" : "", status].filter((part) => part !== "").join(", "),
  };
}

// Whether the stored value folds the map. Anything but "collapsed" leaves it open.
export function collapsedFrom(text) {
  return text === "collapsed";
}

// An SVG points attribute, each value rounded to half a pixel, so a move under a quarter pixel
// writes the same string.
export function pointsAttr(points) {
  const round = (v) => {
    const r = Math.round(v * 2) / 2;
    return Object.is(r, -0) ? 0 : r;
  };
  return points.map(([x, y]) => `${round(x)},${round(y)}`).join(" ");
}

// Whether two float lists match within `eps`, NaN matching NaN.
export function sameFloats(a, b, eps = 1e-3) {
  if (!a || !b || a.length !== b.length) return false;
  for (let i = 0; i < a.length; i += 1) {
    if (Number.isNaN(a[i]) && Number.isNaN(b[i])) continue;
    if (!(Math.abs(a[i] - b[i]) <= eps)) return false;
  }
  return true;
}
