// The state machines on the map, drawn in one SVG layer: a dial on each building's roof, and at
// near detail its stations and belts on the ground. `sceneOf` in statemachine.js says where each
// shape stands; this file only places them each frame, as `placePlates` places the plates. The
// shapes are kept in pools, so a frame moves them rather than rebuilding the layer.
import { guessTop, riseScale } from "./order.js";
import { allStateMachines, lightNow, sceneOf } from "./statemachine.js";
import { state } from "./state.js";

const NS = "http://www.w3.org/2000/svg";

// Screen radii in CSS px: a dial's outer and inner tick ends, its disc, and a station.
const DIAL_PX = 9;
const DIAL_INNER_PX = 4.5;
const DISC_PX = 10.5;
const STATION_PX = 3.2;

const pools = new Map();
let layer = null;

function svgLayer() {
  if (layer === null) layer = document.getElementById("statemap");
  return layer;
}

// The first `n` elements of `kind`, made on first use. Elements past `n` are hidden.
function pooled(kind, n, tag) {
  let list = pools.get(kind);
  if (list === undefined) {
    list = [];
    pools.set(kind, list);
  }
  while (list.length < n) {
    const el = document.createElementNS(NS, tag);
    layer.append(el);
    list.push(el);
  }
  for (let i = 0; i < list.length; i += 1) list[i].style.display = i < n ? "" : "none";
  return list.slice(0, n);
}

function setAttrs(el, attrs) {
  for (const [name, value] of Object.entries(attrs)) el.setAttribute(name, String(value));
}

function pathOf(points) {
  return points.map(([x, y], i) => `${i === 0 ? "M" : "L"}${x.toFixed(1)} ${y.toFixed(1)}`).join("");
}

// The lines of one dial's ticks: each tick runs from the inner radius to the outer, at its angle
// from straight up, clockwise.
function tickLines(dial) {
  return dial.ticks.map((tick) => {
    const rad = (tick.angle * Math.PI) / 180;
    const sx = Math.sin(rad);
    const sy = -Math.cos(rad);
    return {
      x1: dial.x + sx * DIAL_INNER_PX,
      y1: dial.y + sy * DIAL_INNER_PX,
      x2: dial.x + sx * DIAL_PX,
      y2: dial.y + sy * DIAL_PX,
      lit: tick.lit,
    };
  });
}

function paintScene(scene, close) {
  const belts = pooled("belt", scene.belts.length, "path");
  scene.belts.forEach((belt, i) => {
    const classes = ["belt"];
    if (belt.low) classes.push("low");
    if (belt.stub) classes.push("stub");
    if (belt.lit !== null) classes.push("lit");
    setAttrs(belts[i], { d: pathOf(belt.points), class: classes.join(" ") });
    belts[i].style.opacity = belt.lit?.kind === "fade" ? String(belt.lit.alpha) : "";
  });

  const stations = pooled("station", scene.stations.length, "circle");
  scene.stations.forEach((station, i) => setAttrs(stations[i], { cx: station.x, cy: station.y, r: STATION_PX, class: "station" }));
  const names = pooled("name", close ? scene.stations.length : 0, "text");
  scene.stations.slice(0, names.length).forEach((station, i) => {
    setAttrs(names[i], { x: station.x + STATION_PX + 3, y: station.y, class: "name", "dominant-baseline": "central" });
    names[i].textContent = station.name;
  });

  const discs = pooled("disc", scene.dials.length, "circle");
  scene.dials.forEach((dial, i) => setAttrs(discs[i], { cx: dial.x, cy: dial.y, r: DISC_PX, class: "dial" }));
  const lines = scene.dials.flatMap(tickLines);
  const ticks = pooled("tick", lines.length, "line");
  lines.forEach((line, i) => setAttrs(ticks[i], { x1: line.x1, y1: line.y1, x2: line.x2, y2: line.y2, class: line.lit ? "tick lit" : "tick" }));

  const mores = pooled("more", scene.more.length, "text");
  scene.more.forEach((more, i) => {
    setAttrs(mores[i], { x: more.x, y: more.y, class: "more", "dominant-baseline": "central" });
    mores[i].textContent = more.text;
  });
}

function hideAll() {
  for (const list of pools.values()) {
    for (const el of list) el.style.display = "none";
  }
}

// Places the state machines for this frame. Returns true while a belt is still fading, so the
// frame loop asks for the next frame; a replay's fade waits for its next entry instead.
export function placeStateMaps() {
  const svg = svgLayer();
  if (svg === null || state.app === null || state.view === null) return false;
  const machines = allStateMachines();
  if (machines.length === 0) {
    hideAll();
    return false;
  }
  const project = (x, y) => {
    const p = state.app.world_to_screen(x, y);
    return [p[0], p[1]];
  };
  const detail = state.app.detail();
  const scene = sceneOf({
    buildings: state.view.machines,
    machines,
    project,
    riseAt: (x, y) => riseScale(project, x, y),
    topOf: (building) => {
      const top = state.app.machine_top(building.name);
      return top >= 0 ? top : guessTop(building);
    },
    near: detail >= 1,
    now: lightNow(),
  });
  paintScene(scene, detail >= 2);
  return scene.fading;
}
