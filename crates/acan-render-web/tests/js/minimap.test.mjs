// Node tests for the projects map: the boxes it reads, the fit, the footprints, the view's
// ground, the keys and the words, and its place on the page. Run with:
//   node --test crates/acan-render-web/tests/js/minimap.test.mjs
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";

import {
  CLICK_PX,
  KEEP_SHARE,
  MAP_PAD_PX,
  MINIMAP_KEY,
  MIN_FOOT_PX,
  MIN_SPAN,
  collapsedFrom,
  fitWorld,
  focusOrder,
  footprints,
  insideQuad,
  keepFit,
  labelOf,
  nearestId,
  offMap,
  pointsAttr,
  readBoxes,
  rovingNext,
  sameFloats,
  shouldShow,
  toMap,
  toWorld,
  union,
  upTick,
  viewShape,
} from "../../www/minimap-model.js";

const www = (name) => readFileSync(new URL(`../../www/${name}`, import.meta.url), "utf8");

const SIZE = [184, 122];

const close = (a, b, eps = 1e-6) => Math.abs(a - b) <= eps;

test("the constants match the design", () => {
  assert.equal(MINIMAP_KEY, "acan.minimap");
  assert.deepEqual([MAP_PAD_PX, MIN_SPAN, MIN_FOOT_PX, CLICK_PX, KEEP_SHARE], [8, 400, 6, 4, 0.6]);
});

test("the boxes pair with the ids, skip a project with no plant or no id, and clamp the built share", () => {
  const floats = Float32Array.from([
    0, 0, 100, 50, 1,
    NaN, NaN, NaN, NaN, NaN,
    200, 0, 300, 80, 0.25,
    400, 0, 500, 50, 1.5,
    600, 0, 700, 50, -1,
    800, 0, 900, 50, 1,
  ]);
  const entries = readBoxes(floats, ["shop", "gone", "api", "docs", "web"]);
  assert.deepEqual(entries.map((e) => e.id), ["shop", "api", "docs", "web"]);
  assert.deepEqual(entries[0], { id: "shop", box: [0, 0, 100, 50], built: 1 });
  assert.equal(entries[1].built, 0.25);
  assert.equal(entries[2].built, 1);
  assert.equal(entries[3].built, 0);
  assert.deepEqual(readBoxes([], []), []);
  assert.deepEqual(readBoxes([0, 0, 1], ["a"]), [], "a short tail is not read");
});

test("the map shows for two plants or more", () => {
  const one = { id: "a", box: [0, 0, 1, 1], built: 1 };
  assert.equal(shouldShow([]), false);
  assert.equal(shouldShow([one]), false);
  assert.equal(shouldShow([one, { ...one, id: "b" }]), true);
});

test("the union holds every box", () => {
  assert.equal(union([]), null);
  assert.deepEqual(union([{ box: [0, 10, 100, 50] }, { box: [-20, 30, 60, 90] }]), [-20, 10, 100, 90]);
});

test("the fit keeps the aspect and centres; a row is width-bound and a stack height-bound", () => {
  const row = fitWorld([0, 0, 1000, 100], SIZE);
  assert.ok(close(row.s, (SIZE[0] - 16) / 1000));
  const [l, t] = toMap(row, [0, 0]);
  const [r, b] = toMap(row, [1000, 100]);
  assert.ok(close(l, MAP_PAD_PX) && close(r, SIZE[0] - MAP_PAD_PX));
  assert.ok(close((t + b) / 2, SIZE[1] / 2));
  assert.ok(close((r - l) / (b - t), 10), "the aspect is kept");
  const stack = fitWorld([0, 0, 500, 2000], SIZE);
  assert.ok(close(stack.s, (SIZE[1] - 16) / 2000));
  const [sl, st] = toMap(stack, [0, 0]);
  const [sr, sb] = toMap(stack, [500, 2000]);
  assert.ok(close(st, MAP_PAD_PX) && close(sb, SIZE[1] - MAP_PAD_PX));
  assert.ok(close((sl + sr) / 2, SIZE[0] / 2));
});

test("MIN_SPAN caps the scale of tiny plants, and the map and world points round-trip", () => {
  const tiny = fitWorld([10, 10, 30, 20], SIZE);
  assert.ok(close(tiny.s, (SIZE[0] - 16) / MIN_SPAN));
  const [x, y] = toMap(tiny, [20, 15]);
  assert.ok(close(x, SIZE[0] / 2) && close(y, SIZE[1] / 2));
  const t = fitWorld([-300, 40, 900, 600], SIZE);
  const back = toWorld(t, toMap(t, [123.5, -45.25]));
  assert.ok(close(back[0], 123.5, 1e-9) && close(back[1], -45.25, 1e-9));
  assert.equal(fitWorld(null, SIZE), null);
});

test("the fit is kept for a move inside the map, and refit when a plant leaves it or the union shrinks", () => {
  const box = [0, 0, 1000, 400];
  const t = fitWorld(box, SIZE);
  assert.equal(keepFit(t, box, SIZE), true);
  assert.equal(keepFit(t, [100, 50, 900, 350], SIZE), true, "a plant moved inside the map keeps the fit");
  assert.equal(keepFit(t, [0, 0, 1200, 400], SIZE), false, "a plant pushed out refits");
  assert.equal(keepFit(t, [0, -200, 1000, 400], SIZE), false);
  assert.equal(keepFit(t, [200, 100, 700, 300], SIZE), false, "under 60% refits");
  assert.equal(keepFit(t, [100, 0, 750, 300], SIZE), true, "65% keeps the fit");
  assert.equal(keepFit(null, box, SIZE), false);
});

test("the footprints follow the boxes, a thin box takes 6 px centred, and the look follows the built share", () => {
  const t = { s: 0.1, tx: 10, ty: 20 };
  const entries = [
    { id: "a", box: [0, 0, 400, 200], built: 1 },
    { id: "b", box: [500, 100, 510, 100], built: 0.4 },
    { id: "c", box: [0, 300, 100, 400], built: 0 },
  ];
  const [a, b, c] = footprints(entries, t);
  assert.deepEqual(a, { id: "a", left: 10, top: 20, width: 40, height: 20, built: 1, look: "built" });
  assert.equal(b.width, MIN_FOOT_PX);
  assert.equal(b.height, MIN_FOOT_PX);
  assert.ok(close(b.left + b.width / 2, 60.5) && close(b.top + b.height / 2, 30));
  assert.equal(b.look, "filling");
  assert.equal(c.look, "unmapped");
});

test("the view maps its four corners and its cut, and is null for no view", () => {
  const t = { s: 2, tx: 1, ty: 3 };
  const shape = viewShape(Float32Array.from([0, 0, 10, 0, 10, 5, 0, 5, 1, 5, 2.5]), t);
  assert.deepEqual(shape.points, [[1, 3], [21, 3], [21, 13], [1, 13]]);
  assert.equal(shape.cut, true);
  assert.deepEqual(shape.pivot, [11, 8]);
  assert.equal(viewShape([0, 0, 10, 0, 10, 5, 0, 5, 0, 5, 2.5], t).cut, false);
  assert.equal(viewShape([], t), null);
  assert.equal(viewShape(null, t), null);
  assert.equal(viewShape([0, 0, NaN, 0, 10, 5, 0, 5, 0, 5, 2.5], t), null);
});

test("a view on the map has no pointer; one wholly off it points from the nearest border", () => {
  const square = (x, y, d) => [[x, y], [x + d, y], [x + d, y + d], [x, y + d]];
  assert.equal(offMap({ points: square(50, 50, 20), cut: false, pivot: [60, 60] }, SIZE), null);
  assert.equal(offMap({ points: square(-10, -10, 20), cut: false, pivot: [0, 0] }, SIZE), null, "overlapping a corner");
  const right = offMap({ points: square(300, 50, 20), cut: false, pivot: [310, 60] }, SIZE, 6);
  assert.deepEqual(right, { x: SIZE[0] - 6, y: 60, angle: 0 });
  const above = offMap({ points: square(80, -100, 20), cut: false, pivot: [90, -90] }, SIZE);
  assert.equal(above.x, 90);
  assert.equal(above.y, 0);
  assert.equal(above.angle, -90);
  // A turned view whose bounding box meets the map's corner but whose sides do not.
  const diamond = [[-10, -25], [5, -10], [-10, 5], [-25, -10]];
  assert.deepEqual(offMap({ points: diamond, cut: false, pivot: [-10, -10] }, SIZE), { x: 0, y: 0, angle: -135 });
  assert.equal(offMap(null, SIZE), null);
});

test("the up tick starts at the top edge's middle and points away from the view", () => {
  const points = [[0, 0], [20, 0], [20, 10], [0, 10]];
  assert.deepEqual(upTick(points, 4), { from: [10, 0], to: [10, -4] });
  // Turned upside down, the top edge is at the bottom and the tick points down.
  const turned = [[20, 10], [0, 10], [0, 0], [20, 0]];
  assert.deepEqual(upTick(turned, 4), { from: [10, 10], to: [10, 14] });
});

test("a point is inside a quad in either winding, and outside past an edge", () => {
  const quad = [[0, 0], [10, 2], [12, 12], [-2, 10]];
  assert.equal(insideQuad(quad, [5, 5]), true);
  assert.equal(insideQuad([...quad].reverse(), [5, 5]), true);
  assert.equal(insideQuad(quad, [0, 0]), true, "a corner counts");
  assert.equal(insideQuad(quad, [5, -1]), false);
  assert.equal(insideQuad(quad, [20, 5]), false);
});

test("the nearest box holds the point, else is the closest, the first on a tie", () => {
  const entries = [
    { id: "a", box: [0, 0, 100, 100] },
    { id: "b", box: [200, 0, 300, 100] },
    { id: "c", box: [50, 150, 250, 180] },
    { id: "d", box: [80, 80, 120, 120] },
  ];
  assert.equal(nearestId(entries, [10, 10]), "a");
  assert.equal(nearestId(entries, [90, 90]), "a", "inside two, the first wins");
  assert.equal(nearestId(entries, [110, 110]), "d");
  assert.equal(nearestId(entries, [140, 20]), "a", "equal gaps pick the first");
  assert.equal(nearestId(entries, [160, 20]), "b");
  assert.equal(nearestId(entries, [150, 200]), "c");
  assert.equal(nearestId([], [0, 0]), null);
});

test("the arrows wrap, Home and End go to the ends, and other keys do nothing", () => {
  assert.equal(rovingNext(3, 0, "ArrowRight"), 1);
  assert.equal(rovingNext(3, 2, "ArrowRight"), 0);
  assert.equal(rovingNext(3, 2, "ArrowDown"), 0);
  assert.equal(rovingNext(3, 0, "ArrowLeft"), 2);
  assert.equal(rovingNext(3, 1, "ArrowUp"), 0);
  assert.equal(rovingNext(3, -1, "ArrowRight"), 0);
  assert.equal(rovingNext(3, -1, "ArrowLeft"), 2);
  assert.equal(rovingNext(3, 1, "Home"), 0);
  assert.equal(rovingNext(3, 1, "End"), 2);
  assert.equal(rovingNext(3, 1, "Enter"), null);
  assert.equal(rovingNext(0, -1, "ArrowRight"), null);
});

test("the plants are walked by centre x, then y", () => {
  const rects = [
    { id: "c", left: 100, top: 0, width: 10, height: 10 },
    { id: "b", left: 0, top: 50, width: 10, height: 10 },
    { id: "a", left: 0, top: 0, width: 10, height: 10 },
  ];
  assert.deepEqual(focusOrder(rects), ["a", "b", "c"]);
  assert.equal(rects[0].id, "c", "the input is not sorted in place");
});

test("a plant's words name it, where it is and how far it is mapped", () => {
  assert.deepEqual(labelOf({ name: "shop", here: true, built: 1 }), { text: "shop", aria: "shop, on screen" });
  assert.deepEqual(labelOf({ name: "api", built: 0.4 }), { text: "api · mapping 40%", aria: "api, mapping 40%" });
  assert.deepEqual(labelOf({ name: "docs", built: 0 }), { text: "docs · not mapped yet", aria: "docs, not mapped yet" });
  assert.deepEqual(labelOf({ name: "web", built: 1, degraded: true }), { text: "web · degraded", aria: "web, degraded" });
  assert.equal(labelOf({ name: "x", built: 0.999 }).text, "x · mapping 99%");
  assert.equal(labelOf({ name: "x", built: 0.001 }).text, "x · mapping 1%");
});

test("the stored fold reads only \"collapsed\"; anything else is open", () => {
  assert.equal(collapsedFrom("collapsed"), true);
  for (const text of [null, undefined, "", "open", "COLLAPSED", "{\"broken\""]) assert.equal(collapsedFrom(text), false, String(text));
});

test("points round to half a pixel, and float lists compare within eps", () => {
  assert.equal(pointsAttr([[1.2, 3.3], [-0.1, 10.76]]), "1,3.5 0,11");
  assert.equal(pointsAttr([[1.24, 2]]), pointsAttr([[1.1, 2]]));
  assert.equal(sameFloats([1, 2, NaN], Float32Array.from([1.0005, 2, NaN])), true);
  assert.equal(sameFloats([1, 2], [1, 2.01]), false);
  assert.equal(sameFloats([1, 2], [1, 2, 3]), false);
  assert.equal(sameFloats([1], null), false);
  assert.equal(sameFloats([], []), true);
});

test("the map sits outside the right rail, with its toggle and its view layer", () => {
  const html = www("index.html");
  const rail = html.match(/<div id="watch">([\s\S]*?)<\/div>/)[1];
  assert.doesNotMatch(rail, /minimap/);
  const aside = html.match(/<aside id="minimap"[^>]*>([\s\S]*?)<\/aside>/);
  assert.ok(aside, "index.html has #minimap");
  assert.match(aside[0], /class="island"/);
  assert.match(aside[0], /aria-label="Projects map"/);
  assert.match(aside[0], /\shidden>/, "it starts hidden");
  assert.ok(html.indexOf('id="minimap"') > html.indexOf('<div id="watch">'));
  const toggle = aside[1].match(/<button[^>]*id="minimap-toggle"[^>]*>/)[0];
  assert.match(toggle, /aria-expanded="true"/);
  assert.match(toggle, /aria-controls="minimap-map"/);
  assert.match(aside[1], /id="minimap-map" role="group"/);
  for (const part of ["polygon class=\"fill\"", "polyline class=\"edge\"", "line class=\"top\"", "path class=\"tick\"", "path class=\"off\""]) {
    assert.ok(aside[1].includes(`<${part}`), part);
  }
  assert.match(html, /<link rel="stylesheet" href="minimap\.css">/);
  assert.match(html, /#watch \{[^}]*max-height: calc\(100vh - 24px - var\(--minimap-room, 0px\)\)/);
});

test("the names and flags keep clear of the map", () => {
  const block = www("marks.js").match(/export const HUD_SELECTORS = \[([\s\S]*?)\]/)[1];
  assert.ok([...block.matchAll(/"([^"]+)"/g)].some((m) => m[1] === "#minimap"));
});

test("the map's style uses theme colours only, with no red, amber, hex or transition", () => {
  const css = www("minimap.css");
  assert.doesNotMatch(css, /--red|--amber/);
  assert.doesNotMatch(css, /#[0-9a-fA-F]{3,8}\b(?![\w-])/);
  assert.doesNotMatch(css, /\btransition\b/);
  assert.doesNotMatch(css, /rgba?\(|hsla?\(/);
});

test("main.js places the map each frame, right after the plates", () => {
  const main = www("main.js");
  assert.match(main, /import \{ placeMinimap \} from "\.\/minimap\.js";/);
  assert.match(main, /placePlates\(\);\n\s*placeMinimap\(\);/);
});

test("G then M is registered once, by the map", () => {
  const files = readdirSync(new URL("../../www/", import.meta.url)).filter((name) => name.endsWith(".js"));
  const owners = files.filter((name) => /goKeys\.set\(\s*"m"/.test(www(name)));
  assert.deepEqual(owners, ["minimap.js"]);
  assert.equal((www("minimap.js").match(/goKeys\.set\(/g) ?? []).length, 1);
});

test("the map's module imports only names its neighbours export", async () => {
  const source = www("minimap.js");
  for (const [, names, file] of source.matchAll(/import \{([^}]+)\} from "\.\/([\w-]+\.js)";/g)) {
    const target = www(file);
    for (const name of names.split(",").map((part) => part.trim()).filter((part) => part !== "")) {
      assert.match(target, new RegExp(`export (?:async )?(?:function|const|let) ${name}\\b`), `${file} exports ${name}`);
    }
  }
});
