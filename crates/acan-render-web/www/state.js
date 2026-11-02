// Shared page state and the DOM handles. An ES module cannot assign another module's `let`,
// so the mutable page state lives on `state`, and every module reads and writes it as
// `state.name`. The DOM handles are read once, when the page loads.

export const TOKEN_KEY = "acan.owner";

export const params = new URLSearchParams(location.search);

// `?freeze=1` holds the plant still for screenshots. The page loads the log and folds only the
// events of its own settles, so the server's later events, such as a lease ending, do not move
// the marks.
export const freeze = params.get("freeze") === "1";

// `?upto=N` folds only the events before seq N. A frozen screenshot uses it to show the demo at
// an earlier point, such as after a witness passes and before its task lands.
export const upto = params.has("upto") ? Number(params.get("upto")) : null;

export const $ = (id) => document.getElementById(id);

export const canvas = $("plant");

export const labelsEl = $("labels");

export const leadersEl = $("leaders");

export const flagsEl = $("flags");

export const card = $("card");

export const gate = $("gate");

export const link = $("link");

export const stale = $("stale");

export const orderEl = $("order");

export const orderLine = $("order-line");

export const usagePanel = $("usage");

export const usageBody = $("usage-body");

export const repoEl = $("repo");

export const noticeEl = $("notice");

export const hintEl = $("hint");

export const hintRest = hintEl.innerHTML;

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
  // Whether the view holds an order to show in the order island.
  hasOrder: false,
  labelGeneration: 0,
  labels: new Map(),
};

