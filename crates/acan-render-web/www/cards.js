// The clash card: its rows, the settlement options with their keys, and the fade after a
// settlement. Settling sends the command, and a frozen page folds the settle's own events.
import { requestRender } from "./main.js";
import { card, freeze, state } from "./state.js";
import { forgetToken } from "./stream.js";

// A settled card shows its one line this long, then fades out over FADE_MS.
const FADE_HOLD_MS = 1500;

const FADE_MS = 400;

export function openCard(index) {
  state.app.open_card(index);
  state.note = "";
  cancelFade();
  showCard();
}

export function toggleCard() {
  state.app.toggle_card();
  state.note = "";
  cancelFade();
  showCard();
}

function showCard() {
  state.markState = JSON.parse(state.app.marks_json());
  paintCard();
  requestRender();
}

// The clash card: the two declarations with their hues, and the witness the clash would break.
// Below them, the settlement options as a list, each with its key chip. The suggested option says
// so. Once the clash is settled, one line says how, and the card fades out.
export function paintCard() {
  const data = state.markState.card;
  if (!data || !state.view) {
    card.hidden = true;
    return;
  }
  card.hidden = false;
  const clash = state.view.clashes[data.clash];
  const unsettled = clash.state === "open" || clash.state === "proposed";

  const head = document.createElement("div");
  head.className = "head";
  const title = document.createElement("h2");
  title.textContent = data.title;
  const close = document.createElement("button");
  close.type = "button";
  close.className = "close";
  close.textContent = "close";
  close.addEventListener("click", toggleCard);
  head.append(title, close);
  const children = [head];

  if (unsettled) {
    cancelFade();
    for (const row of data.rows) {
      if (row.kind !== "settle") children.push(cardRow(row));
    }
    const suggested = data.rows.find((row) => row.kind === "settle" && row.suggested);
    children.push(optionList(clash, suggested ? suggested.key : null));
  } else if (data.resolved) {
    const settled = document.createElement("p");
    settled.className = "resolved";
    settled.textContent = `settled · ${data.resolved}`;
    children.push(settled);
    scheduleFade();
  }
  if (state.note) {
    const error = document.createElement("p");
    error.className = "note";
    error.textContent = state.note;
    children.push(error);
  }
  card.replaceChildren(...children);
}

function cardRow(row) {
  const el = document.createElement("p");
  el.className = `row ${row.kind}`;
  if (row.kind === "declaration") {
    const dot = document.createElement("span");
    dot.className = "dot";
    dot.style.background = row.hue;
    const text = document.createElement("span");
    text.textContent = row.text;
    el.title = row.agent;
    el.append(dot, text);
  } else {
    el.textContent = row.text;
  }
  return el;
}

function optionList(clash, suggestedKey) {
  const list = document.createElement("ol");
  list.className = "options";
  for (const option of clash.options) {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.className = "option";
    button.disabled = state.settling;
    const chip = document.createElement("kbd");
    chip.textContent = String(option.key);
    const label = document.createElement("span");
    label.textContent = option.label;
    button.append(chip, label);
    if (option.key === suggestedKey) {
      const tag = document.createElement("span");
      tag.className = "tag";
      tag.textContent = "suggested";
      button.append(tag);
    }
    button.addEventListener("click", () => settle(clash, option));
    item.append(button);
    list.append(item);
  }
  return list;
}

// A settled card shows its one line for FADE_HOLD_MS, then fades out and closes.
function scheduleFade() {
  if (state.fadeTimer !== null) return;
  state.fadeTimer = setTimeout(() => {
    card.classList.add("fading");
    state.fadeTimer = setTimeout(closeSettledCard, FADE_MS);
  }, FADE_HOLD_MS);
}

function cancelFade() {
  if (state.fadeTimer !== null) clearTimeout(state.fadeTimer);
  state.fadeTimer = null;
  card.classList.remove("fading");
}

function closeSettledCard() {
  state.fadeTimer = null;
  card.classList.remove("fading");
  state.app.close_card();
  state.markState = JSON.parse(state.app.marks_json());
  paintCard();
  requestRender();
}

// Keys 1, 2 and 3 settle the open clash on the card with the matching choice.
export function settleByKey(key) {
  const data = state.markState.card;
  if (!data || !state.view) return;
  const clash = state.view.clashes[data.clash];
  if (!clash || (clash.state !== "open" && clash.state !== "proposed")) return;
  const option = clash.options[key - 1];
  if (option) settle(clash, option);
}

async function settle(clash, option) {
  if (!state.token || state.settling) return;
  state.settling = true;
  state.note = "";
  paintCard();
  try {
    const res = await fetch("/api/commands", {
      method: "POST",
      headers: { Authorization: `Bearer ${state.token}`, "Content-Type": "application/json" },
      body: JSON.stringify({ cmd: "settle_clash", clash: clash.id, choice: option.choice }),
    });
    if (res.status === 401 || res.status === 403) {
      forgetToken();
    } else if (!res.ok) {
      state.note = `the origin refused: ${(await res.text()).slice(0, 160)}`;
    } else if (freeze) {
      // The frozen page does not hear the stream, so it folds the settle's own events here.
      for (const event of await res.json()) state.app.ingest(JSON.stringify(event));
      requestRender();
    }
  } catch (err) {
    state.note = "the settle did not reach the server";
  } finally {
    state.settling = false;
    paintCard();
  }
}
