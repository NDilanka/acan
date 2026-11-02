// The slip: the scope of the order the order island shows. It lists the plots, the must-keep rules
// and the open choices, and the Seal and the Strike that act on them. The Seal sends ConfirmScope
// and the Strike sends ReviseScope, both to `/api/commands` as the clash card's settle does. The
// Seal paints solid only once the view carries the origin's confirmation, never on the click.
import { requestRender } from "./main.js";
import { shownOrder } from "./panels.js";
import {
  RETRY_LINE,
  SENT_LINE,
  UNSENT_LINE,
  budgetWords,
  forgetSeal,
  markSealAccepted,
  missingWords,
  picksFor,
  refusalWords,
  sealCommand,
  sealPanel,
  sentSeal,
  strikeScope,
  strikeWords,
} from "./seal.js";
import { freeze, slipEl, state } from "./state.js";
import { forgetToken, get } from "./stream.js";

export function paintSlip() {
  const lane = slipLane();
  state.hasSlip = lane !== null;
  slipEl.hidden = !state.hasSlip || state.ridden !== "";
  if (lane === null) {
    slipEl.replaceChildren();
    return;
  }
  const focused = document.activeElement;
  const focusKey = slipEl.contains(focused) ? focused.dataset.focusKey : undefined;
  const picks = picksFor(state, lane);
  slipEl.replaceChildren(
    head(lane),
    ...sealBlock(lane, picks),
    plotsBlock(lane),
    keepsBlock(lane),
    choicesBlock(lane, picks),
    broadBlock(lane, picks),
    ...(state.slipNote ? [el("p", "note", state.slipNote)] : []),
    el("p", "foot", "A plot names behavior the order may change, on the machines it lists. It is not an authorization boundary or a fence around the code."),
  );
  if (focusKey !== undefined) {
    slipEl.querySelector(`[data-focus-key="${focusKey}"]`)?.focus();
  }
}

// The scope of the order the island shows, or null when that order has no lane.
function slipLane() {
  const order = shownOrder(state.view);
  if (order === undefined) return null;
  return state.view.lanes.find((lane) => lane.order === order.id) ?? null;
}

function head(lane) {
  const order = shownOrder(state.view);
  const box = el("div", "head");
  box.append(el("h2", null, `scope · rev ${lane.rev}`));
  box.append(el("p", "sub", `o-${lane.order} on ${order.machine} · ${order.sentence}`));
  return box;
}

// The Seal's block: nothing until the route has answered for this revision, then the state the
// route and the view give. A stopped order takes no Seal. The Seal button is the only control that
// sends ConfirmScope.
function sealBlock(lane, picks) {
  const panel = sealPanel(state, lane, picks, Date.now());
  const halt = panel.halt === null ? [] : [el("p", "warn", panel.halt)];
  if (panel.kind === "hidden") {
    return [...halt, ...(panel.retrying ? [el("p", "sub", RETRY_LINE)] : [])];
  }
  if (panel.kind === "withheld") return [...halt, withheld(panel)];
  if (panel.kind === "sealed") return [...halt, sealedStamp(panel.confirmation)];

  const stamp = el("div", "stamp", `UNSEALED rev ${lane.rev}`);
  stamp.append(el("span", null, "No human has sealed this revision."));
  const children = [...halt, stamp];
  if (!panel.offer) return children;

  const send = el("button", null, state.slipPending ? "sending…" : `Seal rev ${lane.rev}`);
  send.type = "button";
  send.dataset.focusKey = "seal";
  send.disabled = state.slipPending || !panel.canSend;
  send.addEventListener("click", () => seal(lane));
  if (panel.accepted) children.push(el("p", "sub", SENT_LINE));
  else if (panel.sent) children.push(el("p", "sub", UNSENT_LINE));
  else if (panel.missing.length > 0) {
    children.push(el("p", "sub", `Needs ${panel.missing.map((missing) => missingWords(missing, lane)).join("; ")}.`));
  }
  children.push(send);
  return children;
}

function withheld(seal) {
  const { check } = seal;
  let text;
  if (seal.confirmed !== undefined) {
    text = `The seal on this page is for scope hash ${short(seal.confirmed)}, not the server's ${short(check.origin_hash)}. No Seal is shown.`;
  } else if (check.page_hash === null) {
    text = "This page could not hash the scope, so no Seal is shown.";
  } else {
    text = `The server's scope hash does not match this page's, so no Seal is shown. Page: rev ${check.page_rev}, hash ${short(check.page_hash)}. Server: rev ${check.origin_rev}, hash ${short(check.origin_hash)}.`;
  }
  return el("p", "mismatch", text);
}

function sealedStamp(confirmation) {
  const stamp = el("div", "stamp sealed");
  stamp.append(el("b", null, `SEALED rev ${confirmation.scope_rev}`));
  stamp.append(el("span", null, `by ${confirmation.by} · ${when(confirmation.at)}`));
  stamp.append(el("span", null, `hash ${short(confirmation.scope_hash)} · base ${confirmation.base_commit.slice(0, 8)}`));
  const budget = budgetWords(confirmation.budget);
  stamp.append(el("span", null, `enforced: ${budget.enforced.join("; ")}.`));
  stamp.append(el("span", null, `recorded, not enforced: ${budget.recorded.join(", ")}.`));
  if (confirmation.ruling_versions.length > 0) {
    const ids = confirmation.ruling_versions.map((ruling) => ruling.ruling_id).join(", ");
    stamp.append(el("span", null, `rulings bound: ${ids}. A ruling binds only where a human chose it.`));
  }
  return stamp;
}

function plotsBlock(lane) {
  const box = el("div", "block");
  box.append(el("h3", null, `may change · ${lane.may.length}`));
  if (lane.may.length === 0) box.append(el("p", "sub", "The order has no plots, so it may change nothing."));
  const list = el("ul");
  for (const item of lane.may) {
    const row = el("li");
    row.append(el("div", "mono", `${item.id} · ${item.change}`));
    row.append(el("div", "selector", item.selector));
    row.append(el("div", "sub", item.machines.join(", ")));
    const obligations = el("ul");
    for (const text of item.obligations) obligations.append(el("li", "obl", text));
    row.append(obligations);
    const strike = el("button", "danger", "Strike");
    strike.type = "button";
    strike.dataset.focusKey = `strike-${item.id}`;
    strike.disabled = state.slipPending;
    strike.setAttribute("aria-label", `Strike plot ${item.id}`);
    strike.addEventListener("click", () => {
      state.strikeArmed = item.id;
      state.slipNote = "";
      paintSlip();
    });
    row.append(strike);
    if (state.strikeArmed === item.id) row.append(strikeWarning(lane, item));
    list.append(row);
  }
  box.append(list);
  return box;
}

// The Strike's warning, shown before anything is sent. The lease count is the origin's count of
// tasks that hold a lease now, which a newer revision ends.
function strikeWarning(lane, item) {
  const box = el("div", "warn");
  const words = strikeWords(lane, item);
  box.append(el("p", null, words.removes));
  if (words.last !== null) box.append(el("p", "mismatch", words.last));
  box.append(el(
    "p",
    "sub",
    "A lease shows that an agent's authority is valid, not that its work is safe. Leases do not expire while a run gates. A newer revision ends them.",
  ));
  const confirm = el("button", "danger", state.slipPending ? "sending…" : "Strike");
  confirm.type = "button";
  confirm.dataset.focusKey = "strike-confirm";
  confirm.disabled = state.slipPending;
  confirm.addEventListener("click", () => strike(lane, item));
  const keep = el("button", null, "Keep");
  keep.type = "button";
  keep.dataset.focusKey = "strike-keep";
  keep.disabled = state.slipPending;
  keep.addEventListener("click", () => {
    state.strikeArmed = null;
    paintSlip();
  });
  box.append(confirm, " ", keep);
  return box;
}

function keepsBlock(lane) {
  const box = el("div", "block");
  box.append(el("h3", null, `must keep · ${lane.must_keep.length}`));
  const list = el("ul");
  for (const rule of lane.must_keep) {
    const row = el("li");
    row.append(el("div", null, rule.text));
    row.append(el("div", "sub", rule.machines.join(", ")));
    list.append(row);
  }
  box.append(list);
  return box;
}

function choicesBlock(lane, picks) {
  if (lane.open.length === 0) return el("div", "block");
  const box = el("div", "block");
  box.append(el("h3", null, `open choices · ${lane.open.length}`));
  for (const choice of lane.open) {
    const row = el("div", "choice");
    row.append(el("div", null, choice.question));
    row.append(el("div", "sub", choice.machine));
    const options = el("div");
    for (const option of choice.options) {
      const button = el("button", null, `${option.key} · ${option.label}`);
      button.type = "button";
      button.dataset.focusKey = `pick-${choice.id}-${option.key}`;
      button.setAttribute("aria-pressed", String(picks.choices.get(choice.id) === option.key));
      button.disabled = state.slipPending || sentSeal(state, lane) !== null;
      button.addEventListener("click", () => {
        picks.choices.set(choice.id, option.key);
        paintSlip();
      });
      options.append(button, " ");
    }
    row.append(options);
    box.append(row);
  }
  return box;
}

function broadBlock(lane, picks) {
  const broad = lane.may.filter((item) => item.broad);
  if (broad.length === 0) return el("div", "block");
  const box = el("div", "block");
  box.append(el("h3", null, `broad plots · ${broad.length}`));
  for (const item of broad) {
    const label = el("label", "ack");
    const check = document.createElement("input");
    check.type = "checkbox";
    check.dataset.focusKey = `ack-${item.id}`;
    check.checked = picks.acks.has(item.id);
    check.disabled = state.slipPending || sentSeal(state, lane) !== null;
    check.addEventListener("change", () => {
      if (check.checked) picks.acks.add(item.id);
      else picks.acks.delete(item.id);
      paintSlip();
    });
    label.append(check, ` ${item.id} is broad (${item.selector}). Acknowledge it.`);
    box.append(label);
  }
  return box;
}

// Sends the Seal: the ConfirmScope sent last for this revision, sent again as it was, or else one
// built from a fresh route answer and the human's answers. Nothing is sent for a stopped order, for
// a route that names another scope, or while an answer or acknowledgement is missing.
async function seal(lane) {
  if (!state.token || state.slipPending || lane.stopped) return;
  state.slipPending = true;
  state.slipNote = "";
  paintSlip();
  try {
    const built = await sealCommand(state, get, lane, picksFor(state, lane), Date.now());
    if (built.mismatch) {
      state.slipNote = "The server's scope no longer matches this page. Nothing was sent.";
    } else if (built.missing) {
      state.slipNote = `Not sent: needs ${built.missing.map((missing) => missingWords(missing, lane)).join("; ")}.`;
    } else {
      const outcome = await post(built.command);
      if (outcome === "accepted") markSealAccepted(state, lane);
      else if (outcome === "refused") forgetSeal(state, lane);
    }
  } catch {
    state.slipNote = sentSeal(state, lane) !== null
      ? "The server did not answer, so the seal may not be recorded. Seal sends the same confirmation again."
      : "The server did not answer. Nothing was sent.";
  } finally {
    state.slipPending = false;
    paintSlip();
  }
}

// Sends the Strike: the lane's scope one revision on, with the plot removed.
async function strike(lane, item) {
  if (!state.token || state.slipPending) return;
  state.slipPending = true;
  state.slipNote = "";
  paintSlip();
  try {
    const scope = strikeScope(lane.scope, item.id);
    if ((await post({ cmd: "revise_scope", order: lane.order, scope })) === "accepted") state.strikeArmed = null;
  } catch {
    state.slipNote = "The server did not answer, so the strike may not be recorded. Try again.";
  } finally {
    state.slipPending = false;
    paintSlip();
  }
}

// Posts one command. Returns "accepted" when the server took it, "refused" when it answered no
// (nothing was recorded), or "unsent" when it failed, so the same command may be sent again. A
// refusal is shown as a plain sentence, never as the server's JSON. In a frozen page the stream is
// not heard, so the command's own events are folded here, as the clash card's settle does.
async function post(command) {
  const res = await fetch("/api/commands", {
    method: "POST",
    headers: { Authorization: `Bearer ${state.token}`, "Content-Type": "application/json" },
    body: JSON.stringify(command),
  });
  if (res.status === 401 || res.status === 403) {
    forgetToken();
    return "refused";
  }
  if (!res.ok) {
    state.slipNote = refusalWords(res.status, await res.text());
    return res.status >= 500 ? "unsent" : "refused";
  }
  if (freeze) {
    for (const event of await res.json()) state.app.ingest(JSON.stringify(event));
    requestRender();
  }
  return "accepted";
}

function short(hash) {
  return hash.replace("sha256:", "").slice(0, 12);
}

function when(ms) {
  return `${new Date(ms).toISOString().slice(0, 16).replace("T", " ")} UTC`;
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}
