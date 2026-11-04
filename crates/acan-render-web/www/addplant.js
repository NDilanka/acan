// The "+" in the shell and the form it opens. A folder path is posted to the server, and the new
// codebase is put on the canvas as it starts its scan, with no reload. The request and the words
// the form shows are in addplant-model.js.
import { addNote, addRequest } from "./addplant-model.js";
import { state } from "./state.js";

// `added` takes `{ id, name }` of the new codebase and resolves to true when it is on the canvas,
// or false when this page cannot show it without a reload.
export function installAddPlant({ added }) {
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
    const { url, init } = addRequest(state.token, input.value);
    note.textContent = "Adding…";
    let status = 0;
    let body = null;
    try {
      const res = await fetch(url, init);
      status = res.status;
      body = await res.json().catch(() => null);
    } catch {
      status = 0;
    }
    if (status !== 202 || !body) {
      note.textContent = addNote(status, body);
      return;
    }
    input.value = "";
    let live = false;
    try {
      live = await added({ id: body.id, name: body.name });
    } catch (err) {
      console.error(`the new codebase ${body.id} could not be put on the canvas`, err);
    }
    if (live) {
      open(false);
      note.textContent = "";
    } else {
      note.textContent = "Added. Reload the page to see it beside this one.";
    }
  });
}
