// The receipt tour's page logic, with no DOM, so Node can test it. The wasm side admits the tour,
// fills every caption and moves the camera. This file only chooses which step is shown, what the
// chip says, and how an item is toned. It computes no class, count, verdict or denominator: each
// number it shows comes from the payload `tour_load` returns.

// How long the camera takes to reach a step. Matches `tour::MOVE_MS` in the wasm crate.
export const MOVE_MS = 600;

// How long a step's caption stays before the next step plays.
export const DWELL_MS = 2200;

export const STEP_MS = MOVE_MS + DWELL_MS;

// The two modes of the toggle. Plain is the default: the director is not the default until it
// wins the measured comparison.
export const PLAIN = "plain";
export const DIRECTED = "directed";

// Classes that count toward a pass, and the states that need a look whatever the class.
const PASS_CLASSES = ["preserved", "obligation_fulfilled"];
const ATTENTION_STATES = ["bounced", "residue", "stopped"];
// Classes the page cannot judge: they are grey, never green.
const UNKNOWN_CLASSES = ["unstable_base", "incomparable", "error"];

// The chip. In plain mode it reads `plain`. In directed mode it names the order that plays: the
// admitted director, or the deterministic order when the director's tour was not admitted.
export function chipText(mode, play) {
  if (mode === PLAIN || !play) return "plain";
  if (play.mode === "directed") return `directed · ${play.model}`;
  return "deterministic";
}

// What the plant stages for a step, as the wasm side takes it: the spots of the step's marks in
// directed mode, and nothing in plain mode, where every machine keeps its colour. An empty text
// clears the stage.
export function stageArgument(mode, step) {
  if (mode !== DIRECTED || !step) return "";
  return JSON.stringify(step.spots ?? []);
}

// The index kept inside a walk of `count` steps. An empty walk stays at 0.
export function clampIndex(index, count) {
  if (count <= 0) return 0;
  return Math.min(Math.max(index, 0), count - 1);
}

// The step a walk shows at `index`, or null when there is no walk.
export function stepAt(play, index) {
  if (!play || play.steps.length === 0) return null;
  return play.steps[clampIndex(index, play.steps.length)];
}

// Playback over a walk of `count` steps: `{ index, playing }`. Each tick moves to the next step,
// and the last step stops the walk where it is.
export function advance(run, count) {
  if (!run.playing) return run;
  if (run.index + 1 >= count) return { index: clampIndex(run.index, count), playing: false };
  return { index: run.index + 1, playing: true };
}

// The tone of an item: `pass` only for a class that counts toward a pass in a settled state,
// `unknown` for a class the page cannot judge, `residue` for an amber residue, and `fail` for
// the rest.
export function itemTone(item) {
  if (UNKNOWN_CLASSES.includes(item.class)) return "unknown";
  if (item.state === "residue") return "residue";
  if (PASS_CLASSES.includes(item.class) && !ATTENTION_STATES.includes(item.state)) return "pass";
  return "fail";
}

// The first line of the receipt: the hash, short.
export function shortHash(hash) {
  return hash.replace(/^sha256:/, "").slice(0, 12);
}

// A commit as the plate shows it: seven characters.
export function shortSha(sha) {
  return sha.slice(0, 7);
}

// The count lines of the plate. Each figure is the payload's own, with its denominator beside it.
export function countLines(play) {
  const counts = play.counts;
  const byClass = Object.entries(counts.by_class)
    .filter(([, count]) => count > 0)
    .map(([name, count]) => `${count} ${name.replaceAll("_", " ")}`);
  const behavior = counts.behavior;
  return [
    `${play.items.length} items: ${byClass.join(" · ")}`,
    `probes ${counts.probes_run} of ${counts.probes_total} run`,
    `routes ${counts.routes_covered} of ${counts.routes_total} covered`,
    `obligations ${behavior.passed} of ${behavior.obligations_total} passed · ${behavior.failed} failed · ${behavior.unknown} unknown`,
    `${counts.masks_applied} masks applied`,
  ];
}

// The closed line over the count lines: the item total and the obligations that passed, each with
// its denominator. The rest of the counts open beneath it.
export function countSummary(play) {
  const behavior = play.counts.behavior;
  return `${play.items.length} items · obligations ${behavior.passed} of ${behavior.obligations_total} passed`;
}
