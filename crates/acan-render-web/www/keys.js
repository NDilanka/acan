// The map's key table: every key the page answers, which panel owns it, and the order the panels
// are asked in. It has no DOM, so Node can test it; main.js reads the page into `ctx` and runs the
// action the table names.
//
// The order, first to last: the G chord (G, then a digit rides that lane's agent, or a letter goes
// to a panel), Esc (the topmost open panel closes, then a rewound map goes live, then a ride
// ends), then each single key.
import { goStep } from "./lanes.js";

// The open panels Esc closes, topmost first. The command bar sits over everything, the compose
// panel over the map's cards, the vision viewer over the pins, then the question and clash cards.
export const ESCAPE_ORDER = ["command", "compose", "viewer", "question", "clash"];

const PAN_PX = 80;

const ARROWS = {
  ArrowLeft: [PAN_PX, 0],
  ArrowRight: [-PAN_PX, 0],
  ArrowUp: [0, PAN_PX],
  ArrowDown: [0, -PAN_PX],
};

// Single keys: the action, whether a held key repeats it, and whether the browser's own use of the
// key is stopped. "/" and "o" move the focus into a text field, so the key must not be typed there.
const SINGLE = {
  "/": { kind: "open-command", repeats: false, prevent: true },
  "?": { kind: "toggle-help", repeats: false, prevent: true },
  a: { kind: "toggle-question", repeats: false },
  o: { kind: "order-here", repeats: false, prevent: true },
  c: { kind: "toggle-card", repeats: false },
  "+": { kind: "zoom", dir: 1, repeats: true },
  "=": { kind: "zoom", dir: 1, repeats: true },
  "-": { kind: "zoom", dir: -1, repeats: true },
  _: { kind: "zoom", dir: -1, repeats: true },
  0: { kind: "fit", repeats: true },
  f: { kind: "ride-centre", repeats: false },
  q: { kind: "turn", dir: -1, repeats: false },
  e: { kind: "turn", dir: 1, repeats: false },
  p: { kind: "play", repeats: false },
  t: { kind: "mode", repeats: false },
  "[": { kind: "step", delta: -1, repeats: false },
  "]": { kind: "step", delta: 1, repeats: false },
};

// `press` is `{ key, code, repeat, shift }`. `ctx` is `{ armed, now, letters, open, held }`:
// when G was pressed (or null), the time now in ms, the letters a panel answers after G, which
// panels are open (`open.command` and so on), and whether the scrubber holds the map in the past.
// `ctx.riding` says a drone is ridden. Returns `{ armed, action, prevent }`: the chord's new
// state, the action to run (null for a key the map does not use), and whether to stop the
// browser's default.
export function keyAction(press, ctx) {
  let armed = ctx.armed ?? null;
  const done = (action, prevent = false) => ({ armed, action, prevent });
  if (!press.repeat) {
    const step = goStep(armed, press.key, ctx.now, ctx.letters ?? new Map());
    armed = step.armed;
    if (step.taken) {
      if (step.action === null) return done({ kind: "arm-go" }, true);
      if (step.action.kind === "agent") return done({ kind: "ride-lane", index: step.action.index }, true);
      return done({ kind: "go", letter: step.action.key }, true);
    }
  }
  if (press.key === "Escape") {
    const top = ESCAPE_ORDER.find((panel) => ctx.open?.[panel]);
    if (top !== undefined) return done({ kind: "close", panel: top }, true);
    if (ctx.held) return done({ kind: "go-live" }, true);
    if (ctx.riding) return done({ kind: "leave-ride" });
    return done(null);
  }
  if (ARROWS[press.key]) {
    const [dx, dy] = ARROWS[press.key];
    return done({ kind: "pan", dx, dy }, true);
  }
  if (press.key === "5" || press.code === "Numpad5") return done(press.repeat ? null : { kind: "projection" });
  if (/^[1-3]$/.test(press.key)) return done(press.repeat ? null : { kind: "digit", n: Number(press.key) });
  if (press.key === "Tab") return done({ kind: "ride-step", dir: press.shift ? -1 : 1 }, true);
  const single = SINGLE[press.key.length === 1 ? press.key.toLowerCase() : press.key];
  if (single === undefined) return done(null);
  const { repeats, prevent = false, ...action } = single;
  if (press.repeat && !repeats) return done(null, prevent);
  return done(action, prevent);
}
