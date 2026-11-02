// The clash card: its rows, the settlement options with their keys, and the fade after a
// settlement. Settling sends the command, and a frozen page folds the settle's own events. The
// question card below is its twin, for the questions an order asks.
import { announce, composeOpen } from "./compose.js";
import { requestRender } from "./main.js";
import { composeSpot } from "./order.js";
import {
  answerCommand,
  answerEffect,
  answerRefusal,
  answeredLine,
  freshQuestions,
  openQuestion,
  questionTitle,
} from "./question.js";
import { $, canvas, card, freeze, replay, state } from "./state.js";
import { forgetToken } from "./stream.js";

// A settled card shows its one line this long, then fades out over FADE_MS.
const FADE_HOLD_MS = 1500;

const FADE_MS = 400;

export function openCard(index) {
  closeQuestionCard();
  state.app.open_card(index);
  state.note = "";
  cancelFade();
  showCard();
}

export function toggleCard() {
  closeQuestionCard();
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
  // The replay holds no token and sends nothing. It says so rather than fail silently.
  if (replay) {
    state.note = "the replay is read-only";
    paintCard();
    return;
  }
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
      state.note = `the server refused: ${(await res.text()).slice(0, 160)}`;
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

// The question card: an open question and its two answers, beside the question's pin. It is the
// clash card's twin, and only one of the two is open at a time, so keys 1 and 2 always mean one
// card. A question the page has not shown before opens its card by itself, unless another card or
// the compose panel is open, and takes no focus then: a screen reader hears it from the live line.

// The open card, `{ question, sending, note, answered, returnFocus }`, or null. `answered` is the
// option sent, "elsewhere" when the question closed without this page's answer, or null.
let asked = null;

const seenQuestions = new Set();

let askFadeTimer = null;

const questionEl = () => $("question");

export function questionOpen() {
  return asked !== null;
}

export function openQuestionCard(id, { focus = true } = {}) {
  const question = state.view === null ? undefined : openQuestion(state.view, id);
  if (question === undefined) return;
  if (state.markState.card) closeClashCard();
  cancelAskFade();
  const returnFocus = focus ? document.activeElement : null;
  asked = { question, sending: false, note: "", answered: null, returnFocus };
  seenQuestions.add(id);
  paintQuestion();
  questionEl().hidden = false;
  placeQuestion();
  if (focus) questionEl().querySelector("button.option")?.focus();
  requestRender();
}

export function closeQuestionCard() {
  if (asked === null) return;
  const { returnFocus } = asked;
  const hadFocus = questionEl().contains(document.activeElement);
  asked = null;
  cancelAskFade();
  questionEl().hidden = true;
  if (hadFocus) {
    const back = returnFocus instanceof HTMLElement && returnFocus.isConnected && returnFocus !== document.body
      ? returnFocus
      : canvas;
    back.focus({ preventScroll: true });
  }
  requestRender();
}

// A: closes the open question card, or opens the card of the first open question.
export function toggleQuestionCard() {
  if (asked !== null) {
    closeQuestionCard();
    return;
  }
  const first = state.view?.questions[0];
  if (first) openQuestionCard(first.id);
}

// Keys 1, 2 and 3: an open question card takes 1 and 2, else the clash card settles.
export function pressDigit(key) {
  if (asked === null) {
    settleByKey(key);
    return;
  }
  answer(key);
}

// The view changed. The open card follows its question; a question that closed without this
// page's answer says so and fades. A new question opens its card when nothing else is open.
export function syncQuestions() {
  if (state.view === null) return;
  if (asked !== null && asked.answered === null && !asked.sending) {
    const current = openQuestion(state.view, asked.question.id);
    if (current !== undefined) asked.question = current;
    else asked.answered = "elsewhere";
  }
  const fresh = freshQuestions(state.view, seenQuestions);
  for (const id of fresh) seenQuestions.add(id);
  if (fresh.length > 0 && asked === null && !state.markState.card && !composeOpen()) {
    openQuestionCard(fresh[0], { focus: false });
    announce(`${questionTitle(asked.question)}: ${asked.question.text} Press 1 or 2 to answer.`);
  }
  if (asked !== null) paintQuestion();
}

export function closeClashCard() {
  cancelFade();
  state.app.close_card();
  state.markState = JSON.parse(state.app.marks_json());
  paintCard();
}

function paintQuestion() {
  if (asked === null) return;
  const { question } = asked;
  const head = document.createElement("div");
  head.className = "head";
  const title = document.createElement("h2");
  title.id = "question-title";
  title.textContent = questionTitle(question);
  const close = document.createElement("button");
  close.type = "button";
  close.className = "close";
  close.textContent = "close";
  close.setAttribute("aria-label", "Close the question card");
  close.addEventListener("click", closeQuestionCard);
  head.append(title, close);
  const ask = document.createElement("p");
  ask.className = "ask";
  ask.textContent = question.text;
  const children = [head, ask];
  if (asked.answered === null) {
    cancelAskFade();
    children.push(answerList(question));
    const effect = document.createElement("p");
    effect.className = "sub";
    effect.textContent = answerEffect(question);
    children.push(effect);
  } else {
    const done = document.createElement("p");
    done.className = "resolved";
    done.textContent = asked.answered === "elsewhere" ? "answered elsewhere" : answeredLine(question, asked.answered);
    children.push(done);
    scheduleAskFade();
  }
  if (asked.note) {
    const note = document.createElement("p");
    note.className = "note";
    note.textContent = asked.note;
    children.push(note);
  }
  const focusedKey = questionEl().contains(document.activeElement) ? document.activeElement.dataset.key : undefined;
  questionEl().replaceChildren(...children);
  if (focusedKey !== undefined) questionEl().querySelector(`[data-key="${focusedKey}"]`)?.focus();
}

function answerList(question) {
  const list = document.createElement("ol");
  list.className = "options";
  question.options.forEach((text, option) => {
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.className = "option";
    button.dataset.key = String(option + 1);
    button.disabled = asked.sending;
    const chip = document.createElement("kbd");
    chip.textContent = String(option + 1);
    const label = document.createElement("span");
    label.textContent = text;
    button.append(chip, label);
    button.addEventListener("click", () => answer(option + 1));
    item.append(button);
    list.append(item);
  });
  return list;
}

async function answer(key) {
  if (asked === null || asked.sending || asked.answered !== null) return;
  const command = answerCommand(asked.question, key);
  if (command === null) return;
  if (replay || !state.token) {
    asked.note = replay ? "The replay is read-only, so nothing was sent." : "Sign in with the owner token first. Nothing was sent.";
    paintQuestion();
    return;
  }
  const { question } = asked;
  asked.sending = true;
  asked.note = "";
  paintQuestion();
  let note = "";
  let answered = null;
  try {
    const res = await fetch("/api/commands", {
      method: "POST",
      headers: { Authorization: `Bearer ${state.token}`, "Content-Type": "application/json" },
      body: JSON.stringify(command),
    });
    if (!res.ok) {
      note = answerRefusal(res.status, await res.text());
      if (res.status === 401) forgetToken();
    } else {
      const events = await res.json();
      answered = command.option;
      announce(`Answered q-${question.id}: ${question.options[command.option]}.`);
      // The frozen page does not hear the stream, so it folds the answer's own events here.
      if (freeze) {
        for (const event of events) state.app.ingest(JSON.stringify(event));
        requestRender();
      }
    }
  } catch {
    note = "The answer did not reach the server. Try again.";
  }
  if (note) announce(note);
  if (asked === null || asked.question.id !== question.id) return;
  asked.sending = false;
  asked.note = note;
  if (answered !== null) asked.answered = answered;
  paintQuestion();
}

function scheduleAskFade() {
  if (askFadeTimer !== null) return;
  askFadeTimer = setTimeout(() => {
    questionEl().classList.add("fading");
    askFadeTimer = setTimeout(closeQuestionCard, FADE_MS);
  }, FADE_HOLD_MS);
}

function cancelAskFade() {
  if (askFadeTimer !== null) clearTimeout(askFadeTimer);
  askFadeTimer = null;
  questionEl().classList.remove("fading");
}

// Places the card beside its question's pin as the camera stands this frame. A question with no
// pin keeps the clash card's corner.
export function placeQuestion() {
  if (asked === null) return;
  const el = questionEl();
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  const at = asked.question.at;
  if (at === null) {
    el.style.left = `${width - el.offsetWidth - 16}px`;
    el.style.top = "16px";
    return;
  }
  const [x, y] = state.app.world_to_screen(at[0], at[1]);
  const spot = composeSpot({ x, y }, el.offsetWidth, el.offsetHeight, width, height);
  el.style.left = `${spot.left}px`;
  el.style.top = `${spot.top}px`;
}
