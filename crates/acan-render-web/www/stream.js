// The origin's stream: the owner token, the plant and log loads, and the WebSocket that folds
// each event into the view as it arrives. A stream that cannot be applied reloads the log.
import { requestRender } from "./main.js";
import { TOKEN_KEY, freeze, gate, link, state, upto } from "./state.js";

const RECONNECT_MS = 1000;

export function setLink(text) {
  link.textContent = text;
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

async function get(path) {
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
  state.app.load_log(upto === null ? log : keepBefore(log, upto));
  requestRender();
}

function keepBefore(text, seq) {
  return JSON.stringify(JSON.parse(text).filter((event) => event.seq < seq));
}

function connect() {
  const next = state.app.next_seq();
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  const ws = new WebSocket(`${scheme}://${location.host}/api/stream?from=${next}`);
  state.socket = ws;
  ws.onopen = () => {
    ws.send(JSON.stringify({ token: state.token }));
    setLink(`live · seq ${state.app.next_seq()}`);
  };
  ws.onmessage = (message) => {
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
