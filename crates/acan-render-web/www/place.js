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

// Decides what shows, in one pass. `placed` holds the boxes already placed: the drones and the
// flags that stay where they are. `labels` are `{ box }` in priority order. A label shows when
// it is in view and clear of everything placed before it, and a shown label is placed in turn.
// `plots` are `{ left, top, w, h }` at their natural spots. They are placed after the labels, so a
// plot flag moves clear of every label and never hides one. Returns `labelShown`, one boolean per
// label, and `plotTops`, one top per plot.
export function placeOverlays({ placed, labels, plots, width, height, pad }) {
  const boxes = [...placed];
  const labelShown = labels.map(({ box }) => {
    const clear = inView(box, width, height) && boxes.every((other) => !overlaps(box, other));
    if (clear) boxes.push(box);
    return clear;
  });
  const plotTops = plots.map(({ left, top, w, h }) => {
    const y = clearOf(left, top, w, h, boxes, pad);
    boxes.push(boxAt(left, y, w, h, pad));
    return y;
  });
  return { labelShown, plotTops };
}
