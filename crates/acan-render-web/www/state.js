// Shared page state and the DOM handles. An ES module cannot assign another module's `let`,
// so the mutable page state lives on `state`, and every module reads and writes it as
// `state.name`. The DOM handles are read once, when the page loads.

export const TOKEN_KEY = "acan.owner";

export const params = new URLSearchParams(location.search);

// `?freeze=1` holds the plant still for screenshots. The page loads the log and folds only the
// events of its own settles, so the server's later events, such as a lease ending, do not move
// the marks.
export const freeze = params.get("freeze") === "1";

// `?replay=1` plays a recorded run from the files in `replay/`. The page then makes no API call,
// opens no socket and sends no command.
export const replay = params.get("replay") === "1";

// `?upto=N` folds only the events before seq N. A frozen screenshot uses it to show the demo at
// an earlier point, such as after a witness passes and before its task lands.
export const upto = params.has("upto") ? Number(params.get("upto")) : null;

export const $ = (id) => document.getElementById(id);

// The renderer canvas. The page swaps in a fresh copy when a backend fails, so this is a `let`,
// and importers see the copy through the live binding.
export let canvas = $("plant");

export function setCanvas(next) {
  canvas = next;
}

// The band of the canvas, `{ left, right }` in CSS pixels, that a panel opened beside a machine may
// use: right of the receipt plate and left of the right rail, when they show, so it never covers
// either.
export function freeBand() {
  let left = 0;
  let right = canvas.clientWidth;
  const rail = document.getElementById("watch")?.getBoundingClientRect();
  if (rail && rail.width > 0 && rail.height > 0) right = Math.min(right, rail.left);
  const receipt = document.getElementById("receipt");
  if (receipt && !receipt.hidden) {
    const box = receipt.getBoundingClientRect();
    if (box.width > 0) left = Math.max(left, box.right);
  }
  return right - left < 320 ? { left: 0, right } : { left, right };
}

export const labelsEl = $("labels");

export const leadersEl = $("leaders");

export const flagsEl = $("flags");

export const card = $("card");

export const gate = $("gate");

export const link = $("link");

export const stale = $("stale");

export const slipEl = $("slip");

export const usagePanel = $("usage");

export const metaBody = $("meta-body");

export const metaNote = $("meta-note");

export const metaBadge = $("meta-badge");

export const metaSection = $("usage-meta");

export const metaLegend = $("meta-legend");

export const historyEl = $("usage-history");

export const historyBody = $("history-body");

export const repoEl = $("repo");

export const noticeEl = $("notice");

export const hintEl = $("hint");

export const hintRest = hintEl.innerHTML;

export const shellEl = $("shell");

export const helpToggle = $("help-toggle");

export const povEls = ["pov-vignette", "pov-frame", "pov-reticle", "pov-chip"].map((id) => $(id));

export const povChip = $("pov-chip");

export const povDot = $("pov-dot");

export const povAgent = $("pov-agent");

export const povLine = $("pov-line");

export const povAlt = $("pov-alt");

export const povBracket = $("pov-bracket");

// The mutable page state. Each field starts as it did in the single-file page.
export const state = {
  // `?pov=agent-a` rides that agent once the view places its drone, for screenshots.
  povWanted: params.get("pov"),
  app: null,
  token: null,
  // True while the origin has no codebase: no stream is open and no project route is called. See
  // `emptystart.js`.
  empty: false,
  view: null,
  renderedVersion: -1,
  // The marks as the wasm side last gave them: the flags in order, and the open clash card.
  markState: { flags: [], card: null },
  flagEls: [],
  leaderEls: [],
  fadeTimer: null,
  socket: null,
  settling: false,
  note: "",
  scheduled: false,
  lastFrame: null,
  drag: null,
  press: null,
  altDown: false,
  // The agent the page rides, or "" when it does not ride.
  ridden: "",
  // The id of the order this page posted last, or null. The slip shows it while it is in flight.
  postedOrder: null,
  // Whether the slip shows: the shown order has a scope lane. See `slip.js`.
  hasSlip: false,
  // The origin's confirmation route for each order with a scope, as `seal.js` keeps it.
  sealViews: new Map(),
  // The ConfirmScope last sent for each order, with the revision it was built on. See `seal.js`.
  sealSent: new Map(),
  // The open-choice answers and broad acknowledgements given on the scope shown, by `hash`.
  sealPicks: { hash: null, choices: new Map(), acks: new Set() },
  // The plot whose Strike waits for its confirm, or null.
  strikeArmed: null,
  // Whether a Seal or a Strike is being sent, and the note the last one left.
  slipPending: false,
  slipNote: "",
  // The scope-draft answer of each order with no lane yet, as `slip.js` fetches it: `{ answer,
  // inFlight, retryAt }`, with `answer` null until the route has a draft or a failure to show.
  drafts: new Map(),
  labelGeneration: 0,
  labels: new Map(),
  // The plant view summary of each order that has receipts, as the wasm side checked it, with the
  // play it admitted. See `plant.js`.
  plantViews: new Map(),
  // The receipt plate: the mode the toggle shows ("plain" or "directed"), the step playing, whether
  // the walk runs and its timer, the order it follows, and the stage the plant shows (the argument
  // last given to `tour_stage`). See `plate.js`.
  plate: { mode: "plain", index: 0, playing: false, timer: null, order: null, stage: "" },
  // The origin's usage answer, `{ rows, error }`, or null until the first one. See `usage.js`.
  serverUsage: null,
  usageInFlight: false,
  usageDirty: false,
};

