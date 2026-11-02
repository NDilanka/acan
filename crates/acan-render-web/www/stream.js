// The origin's stream: the owner token, the plant and log loads, and the WebSocket that folds
// each event into the view as it arrives. A stream that cannot be applied reloads the log. While
// the timeline scrubber holds the map in the past, the stream keeps every event in the hold
// instead, and the scrubber folds them all when the map goes live again.
import { requestRender } from "./main.js";
import { createFeed, createHold, eventsBefore } from "./scrub.js";
import { TOKEN_KEY, freeze, gate, link, replay, state, upto } from "./state.js";

const RECONNECT_MS = 1000;

// Every event the app holds, for the watch panels. See `installFeed`.
export const feed = createFeed();

// The live log while the scrubber shows the past. See `createHold`.
export const hold = createHold();

const holdListeners = [];

// Runs `listener` each time the hold takes an event or a reloaded log.
export function onHoldEvent(listener) {
  holdListeners.push(listener);
}

function heldMore() {
  for (const listener of holdListeners) listener();
}

// Makes the feed follow the app: each log the app loads and each event it ingests is kept, from
// every path that folds one (the stream, the replay, and a frozen page's own commands). An event
// the app refuses is not kept.
export function installFeed(app) {
  const load = app.load_log.bind(app);
  const ingest = app.ingest.bind(app);
  app.load_log = (json) => {
    load(json);
    const parsed = loading.events;
    loading.events = null;
    feed.load(parsed ?? JSON.parse(json));
  };
  app.ingest = (json) => {
    ingest(json);
    feed.add(JSON.parse(json));
  };
}

// The events `loadEvents` is loading, so the feed takes them without parsing the JSON again.
const loading = { events: null };

// Folds `events` as the whole log, as `load_log` does.
export function loadEvents(events) {
  loading.events = events;
  try {
    state.app.load_log(JSON.stringify(events));
  } finally {
    loading.events = null;
  }
}

// The time the panels read "2m ago" against: now on a live page, and the newest event's time on
// a frozen or replayed page, so a recorded run does not read as hours stale.
export function clock() {
  if (!isLive()) return feed.lastAt() ?? Date.now();
  return Date.now();
}

// Whether the page shows the origin as it is now: not frozen, not a replay, and not rewound.
export function isLive() {
  return !freeze && !replay && !hold.on;
}

// The link shows the stream's state, or the scrubber's label while the map is held. The stream's
// latest words are kept, so the link reads right again once the map is live.
const linkWords = { stream: "", held: null };

export function setLink(text) {
  linkWords.stream = text;
  link.textContent = linkWords.held ?? text;
}

// Shows `text` in the link while the map is held, or the stream's words again for null.
export function showHold(text) {
  linkWords.held = text;
  link.textContent = text ?? linkWords.stream;
}

export function forgetToken() {
  sessionStorage.removeItem(TOKEN_KEY);
  state.token = null;
  if (state.socket) {
    const old = state.socket;
    state.socket = null;
    old.close();
  }
  gate.hidden = false;
  setLink("sign in");
}

export async function get(path) {
  const res = await fetch(path, { headers: { Authorization: `Bearer ${state.token}` } });
  if (res.status === 401 || res.status === 403) {
    forgetToken();
    throw new Error("the owner token was refused");
  }
  if (!res.ok) {
    throw new Error(`${path} answered ${res.status}`);
  }
  return res.text();
}

export async function start() {
  setLink("loading");
  state.app.set_plant(await get("/api/plant"));
  await reload();
  if (freeze) {
    setLink(`frozen · seq ${state.app.next_seq()}`);
  } else {
    connect();
  }
}

async function reload() {
  const log = await get("/api/events?from=0");
  const kept = upto === null ? log : keepBefore(log, upto);
  if (hold.on) {
    hold.replace(JSON.parse(kept));
    heldMore();
    return;
  }
  state.app.load_log(kept);
  requestRender();
}

function keepBefore(text, seq) {
  return JSON.stringify(eventsBefore(JSON.parse(text), seq));
}

function connect() {
  const next = hold.on ? hold.next() : state.app.next_seq();
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${scheme}://${location.host}/api/stream?from=${next}`);
  state.socket = ws;
  ws.onopen = () => {
    ws.send(JSON.stringify({ token: state.token }));
    setLink(`live · seq ${state.app.next_seq()}`);
  };
  ws.onmessage = (message) => {
    if (hold.on) {
      try {
        if (hold.push(JSON.parse(message.data))) heldMore();
      } catch (err) {
        console.error("a streamed event could not be read while the map is held", err);
      }
      return;
    }
    try {
      state.app.ingest(message.data);
      requestRender();
    } catch (err) {
      console.error("the stream could not be applied; reloading the log", err);
      ws.close();
    }
  };
  ws.onclose = (event) => {
    if (state.socket !== ws) return;
    state.socket = null;
    if (!state.token) return;
    if (event.code === 1008) {
      forgetToken();
      return;
    }
    setLink("reconnecting");
    setTimeout(resync, RECONNECT_MS);
  };
}

async function resync() {
  if (!state.token) return;
  try {
    await reload();
    connect();
  } catch (err) {
    console.error(err);
    setLink("reconnecting");
    setTimeout(resync, RECONNECT_MS * 2);
  }
}
