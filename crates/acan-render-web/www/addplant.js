// The live add: the "+" in the shell, its form, and the Settings "Add a project" row. Both send the
// folder path to the server, and the new codebase is put on the canvas as its first scan starts,
// with no reload. The request and the words the page shows are in addplant-model.js.
import { RELOAD_NOTE, addNote, addRequest } from "./addplant-model.js";
import { state } from "./state.js";

// Puts a codebase on the canvas once the server has added it. Set by installAddPlant. It resolves to
// true when the codebase is on the canvas, or false when this page cannot show it without a reload.
let attach = async () => false;

// Adds the folder at `path` on the server, then puts the new codebase on the canvas. Resolves to the
// status and body of the answer, and `live`: whether the canvas now shows the new codebase.
export async function addCodebase(path) {
  const { url, init } = addRequest(state.token, path);
  let status = 0;
  let body = null;
  try {
    const res = await fetch(url, init);
    status = res.status;
    body = await res.json().catch(() => null);
  } catch {
    status = 0;
  }
  if (status !== 202 || !body) return { status, body, live: false };
  let live = false;
  try {
    live = await attach({ id: body.id, name: body.name });
  } catch (err) {
    console.error(`the new codebase ${body.id} could not be put on the canvas`, err);
  }
  return { status, body, live };
}

// The line an add shows once the server has added the codebase.
export function addedNote(live) {
  return live ? addNote(202) : RELOAD_NOTE;
}

// `added` takes `{ id, name }` of the new codebase and resolves to true when it is on the canvas,
// or false when this page cannot show it without a reload.
export function installAddPlant({ added }) {
  attach = added;
  const button = document.getElementById("add-plant");
  const form = document.getElementById("add-form");
  const input = document.getElementById("add-path");
  const note = document.getElementById("add-note");
  if (!button || !form || !input || !note) return;

  const open = (on) => {
    form.hidden = !on;
    button.setAttribute("aria-expanded", String(on));
    if (on) input.focus();
  };
  button.addEventListener("click", () => open(form.hidden));
  form.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    open(false);
    button.focus();
  });
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    note.textContent = "Adding…";
    const answer = await addCodebase(input.value);
    if (answer.status !== 202 || !answer.body) {
      note.textContent = addNote(answer.status, answer.body);
      return;
    }
    input.value = "";
    if (answer.live) {
      open(false);
      note.textContent = "";
    } else {
      note.textContent = RELOAD_NOTE;
    }
  });
}
