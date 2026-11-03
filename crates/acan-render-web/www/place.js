// The overlap rules for the HTML marks, with no DOM, so Node can test them. marks.js measures the
// elements and applies what these return.

// Whether two boxes `{ left, right, top, bottom }` share any area.
export function overlaps(a, b) {
  return a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom;
}

// The box an element of size w by h takes at (left, top), grown by `pad` on every side.
export function boxAt(left, top, w, h, pad) {
  return { left: left - pad, right: left + w + pad, top: top - pad, bottom: top + h + pad };
}

// Whether a box is at least partly inside a viewport of the given size.
export function inView(box, width, height) {
  return box.right >= 0 && box.left <= width && box.bottom >= 0 && box.top <= height;
}

// The top at which a box of size w by h, starting at `top`, clears every box in `placed`. It
// moves up past each box it runs into, so it ends clear of all of them. It only ever moves up.
export function clearOf(left, top, w, h, placed, pad) {
  let y = top;
  let moved = true;
  while (moved) {
    moved = false;
    for (const box of placed) {
      if (overlaps(boxAt(left, y, w, h, pad), box)) {
        y = box.top - pad - h;
        moved = true;
      }
    }
  }
  return y;
}

// Where a flag of size w by h, wanted at (left, top), shows clear of every box in `placed`. It
// moves up as `clearOf` does. When that runs it off the top of the view, as a wide pill does under
// a panel that reaches the top edge, it slides sideways instead, to the nearest left at which
// moving up keeps it in view. Returns `{ left, top }`, the upward spot when no left does better.
export function flagSpot(left, top, w, h, placed, pad, width) {
  const up = clearOf(left, top, w, h, placed, pad);
  if (up >= pad) return { left, top: up };
  const lefts = [pad, width - w - pad];
  for (const box of placed) lefts.push(box.left - pad - w, box.right + pad);
  let found = null;
  for (const x of lefts) {
    if (x < pad || x + w > width - pad) continue;
    const y = clearOf(x, top, w, h, placed, pad);
    if (y < pad) continue;
    const cost = Math.abs(x - left) + (top - y);
    if (found === null || cost < found.cost) found = { left: x, top: y, cost };
  }
  return found === null ? { left, top: up } : { left: found.left, top: found.top };
}
// The top nearest `top` at which a box of size w by h, at `left`, lies wholly in the view and
// clears every box in `placed`. It may move up or down, but not below `maxTop`. The free tops end
// at the edges of the boxes, so the nearest one is `top` itself or just above or below a box. A
// tie goes up. Returns null when no such top exists.
export function nearestClear(left, top, w, h, placed, pad, width, height, maxTop = Infinity) {
  const candidates = [top];
  for (const box of placed) candidates.push(box.top - pad - h, box.bottom + pad);
  let found = null;
  for (const y of candidates) {
    const wholly = left >= 0 && left + w <= width && y >= 0 && y + h <= height;
    if (!wholly || y > maxTop || placed.some((other) => overlaps(boxAt(left, y, w, h, pad), other))) {
      continue;
    }
    const distance = Math.abs(y - top);
    if (found === null || distance < found.distance || (distance === found.distance && y < found.y)) {
      found = { y, distance };
    }
  }
  return found === null ? null : found.y;
}

// The spots for the names the wasm side left out. It drops a name whose spots under and above
// its machine are both covered by other machines, but every machine on screen keeps its name, so
// such a machine gets a spot under its footprint's front corner, where the wasm side hangs a name.
// `machines` are `{ index, quad }`, each footprint as four screen corners, and `named` the
// indexes the wasm side placed. A footprint narrower than `minPx` on screen, or with its spot out
// of view, gets none. The page places these last, so they never push out a name the wasm side
// placed.
export function unnamedSpots(machines, named, width, height, minPx) {
  const spots = [];
  for (const { index, quad } of machines) {
    if (named.has(index)) continue;
    const xs = quad.map((p) => p[0]);
    if (Math.max(...xs) - Math.min(...xs) < minPx) continue;
    const x = xs.reduce((sum, v) => sum + v, 0) / xs.length;
    const y = Math.max(...quad.map((p) => p[1]));
    if (x < 0 || x > width || y < 0 || y > height) continue;
    spots.push({ index, x, y });
  }
  return spots;
}

// The leader a plot flag draws once it has moved off its natural top: a line from the edge of its
// box that faces the anchor (x, y), the machine's top point, to that anchor. Null when it stays.
export function leaderOf(plot, top) {
  if (top === plot.top) return null;
  const x = Math.min(Math.max(plot.x, plot.left), plot.left + plot.w);
  const y1 = plot.y < top ? top : top + plot.h;
  return { x1: x, y1, x2: plot.x, y2: plot.y };
}

// Decides what shows, in one pass. `placed` holds the boxes already placed: the page's fixed
// panels, the drones, and the flags that stay where they are. `labels` are `{ box }` in priority
// order. A label shows when it is in view and clear of everything placed before it, and a shown
// label is placed in turn. `plots` are `{ left, top, w, h, x, y }` at their natural spots, where
// (x, y) is the anchor the flag points to. They are placed after the labels, so a plot flag moves
// clear of every label, panel and flag, and never hides one. Returns `labelShown`, one boolean per
// label, `plotTops`, one top per plot, and `plotLeaders`, a leader per plot that moved, else null.
export function placeOverlays({ placed, labels, plots, width, height, pad }) {
  const boxes = [...placed];
  const labelShown = labels.map(({ box }) => {
    const clear = inView(box, width, height) && boxes.every((other) => !overlaps(box, other));
    if (clear) boxes.push(box);
    return clear;
  });
  const plotTops = [];
  const plotLeaders = [];
  for (const plot of plots) {
    const { left, top, w, h } = plot;
    // A flag stays above its anchor when it can, so its leader runs up to the machine and is not
    // hidden under the flag. Only when no clear spot is above does it take the nearest one below.
    const y = nearestClear(left, top, w, h, boxes, pad, width, height, plot.y - h)
      ?? nearestClear(left, top, w, h, boxes, pad, width, height)
      ?? clearOf(left, top, w, h, boxes, pad);
    boxes.push(boxAt(left, y, w, h, pad));
    plotTops.push(y);
    plotLeaders.push(leaderOf(plot, y));
  }
  return { labelShown, plotTops, plotLeaders };
}

// The most agent flags one machine's column shows. The rest fold into one `+N` flag.
export const FLAGS_PER_COLUMN = 2;

// How a column ranks its agents' flags, nearest the drones first: one that waits on the user,
// then a stale one, then a working one, then an idle one.
const FLAG_RANK = { waiting: 0, stale: 1, working: 2, idle: 3 };

// Whether an agent's flag shows: always for a working, waiting or stale drone, and for an idle one
// only while it is hovered or Alt is down.
export function agentFlagShown(stateWord, alt, hovered) {
  return stateWord !== "idle" || alt || hovered;
}

// The flags a machine's column shows, ranked, and how many fold into its `+N`. Each item carries
// its drone's `state` word, and `hovered` when the pointer is over its drone, which brings it to
// the front of the column. Items of one rank keep their order.
export function foldColumn(items, max = FLAGS_PER_COLUMN) {
  const rank = (item) => (item.hovered ? -1 : FLAG_RANK[item.state] ?? Object.keys(FLAG_RANK).length);
  const ranked = items.map((item, i) => ({ item, i }))
    .sort((a, b) => rank(a.item) - rank(b.item) || a.i - b.i)
    .map(({ item }) => item);
  return { shown: ranked.slice(0, max), folded: Math.max(0, ranked.length - max) };
}

// Whether a flag of `kind` starts beside its anchor, centred on its y, rather than centred above
// it: a clash label right of its diamond, a landed dot right of its machine's point, and an away
// dot with its ring on the plinth's corner.
export function standsBeside(kind) {
  return kind === "clash" || kind === "landed" || kind === "away";
}

// A clash badge's diameter in CSS px, as the wasm side sizes it (`marks::BADGE_PX`).
export const BADGE_PX = 16;

// The boxes of a clash's two badges, centred at (leftX, y) and (rightX, y).
export function badgeBoxes(leftX, rightX, y, size = BADGE_PX) {
  const half = size / 2;
  return [leftX, rightX].map((x) => ({ left: x - half, right: x + half, top: y - half, bottom: y + half }));
}

// The key under which the page keeps when the reader last saw it live.
export const LAST_SEEN_KEY = "acan.lastSeen";

// How long the reader must be away before the page shows, as ghosts, what landed meanwhile.
export const AWAY_MS = 5 * 60_000;

// How long the ghosts stay after the reader comes back, if nothing clears them first.
export const GHOST_HOLD_MS = 90_000;

// When the reader last saw the page, for a return at `now` with `stored` as the page kept it, or
// 0 when there is nothing to show: the page is not live, nothing usable was kept, or the reader
// was away less than AWAY_MS.
export function seenOnReturn(stored, now, live) {
  if (!live || stored === null || stored === undefined) return 0;
  const seen = Number(stored);
  if (!Number.isFinite(seen) || seen <= 0 || seen > now) return 0;
  return now - seen >= AWAY_MS ? seen : 0;
}
