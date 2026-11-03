// The Seal and the Strike: which lanes ask the origin's confirmation route, what the Seal and the
// Strike send, what the slip says of the route's answer, and what it says of a refusal. The wasm
// side checks the route's hash against the page's own. This module has no DOM imports, so Node can
// test it.
import { apiPath, target } from "./projects.js";

// Orders whose route is being fetched. A sync skips them, so a slow fetch is not repeated.
const inFlight = new Set();

// How long a failed route waits before it is asked again: 2 s, 5 s, 15 s, then every 30 s.
const RETRY_MS = [2000, 5000, 15000];
const RETRY_EVERY_MS = 30000;

export const LAST_PLOT = "This removes the last plot: the order will have nothing it may change.";
export const STOPPED_LINE = "This order is stopped: its repair budget ran out, so no work on it may start. A seal does not lift the stop.";
export const RETRY_LINE = "Couldn't reach the confirmation route; retrying";
// What the Seal block says in a replay, which has no origin to ask for the route.
export const REPLAY_SEAL_LINE = "The Seal needs the live origin, so it is not in this replay.";
export const SENT_LINE = "Sent. Waiting for the server to show the seal.";
export const UNSENT_LINE = "The last send did not reach the server. Seal sends the same confirmation again.";

export function retryDelay(failures) {
  return failures <= RETRY_MS.length ? RETRY_MS[failures - 1] : RETRY_EVERY_MS;
}

// The revision and hash a route answer is kept under. The route names the scope the page holds, so
// a new revision or a new hash asks again. The Seal fetches the route again when it is sent.
export function sealKey(lane) {
  return `${lane.rev}|${lane.scope_hash}`;
}

// Fetches the route of each lane whose revision or hash moved, and the route of each lane whose
// last fetch failed once its retry is due. `fetchText` is the stream's `get`, `repaint` redraws the
// slip once an answer is kept, and `schedule` sets the timer of a retry.
export function syncSeals(state, fetchText, repaint, schedule = setTimeout) {
  const ctx = { state, fetchText, repaint, schedule };
  for (const lane of state.view.lanes) {
    if (needsRoute(state, lane)) fetchSeal(ctx, lane);
  }
}

function needsRoute(state, lane) {
  if (inFlight.has(lane.order)) return false;
  const entry = state.sealViews.get(lane.order);
  if (entry?.key !== sealKey(lane)) return true;
  return entry.error !== null && entry.retryAt <= Date.now();
}

async function fetchSeal(ctx, lane) {
  const { state, fetchText, repaint, schedule } = ctx;
  const key = sealKey(lane);
  inFlight.add(lane.order);
  try {
    const { body, check } = await askRoute(state, fetchText, lane.order);
    state.sealViews.set(lane.order, { key, body, check, error: null, failures: 0, retryAt: 0 });
  } catch (err) {
    const error = String(err?.message ?? err);
    console.error(`the confirmation of o-${lane.order} was not kept: ${error}`);
    const prior = state.sealViews.get(lane.order);
    const failures = (prior?.key === key ? prior.failures : 0) + 1;
    const wait = retryDelay(failures);
    const retryAt = Date.now() + wait;
    state.sealViews.set(lane.order, { key, body: null, check: null, error, failures, retryAt });
    // A lost token is not a lost route: the page asks again once a token is held again.
    if (state.token) {
      schedule(() => {
        const entry = state.sealViews.get(lane.order);
        if (entry?.retryAt === retryAt) entry.retryAt = 0;
        syncSeals(state, fetchText, repaint, schedule);
      }, wait);
    }
  } finally {
    inFlight.delete(lane.order);
    repaint();
    // A revision that moved while this fetch was in flight is asked for now, not at the next event.
    syncSeals(state, fetchText, repaint, schedule);
  }
}

// The origin's confirmation body for `order`, and the wasm side's check of it against the page's
// scope. The check throws when the body is not the route's shape or the page holds no such scope.
export async function askRoute(state, fetchText, order) {
  const id = `o-${order}`;
  const text = await fetchText(apiPath(target(), `/api/orders/${id}/scope/confirmation`));
  const check = JSON.parse(state.app.seal_check(id, text));
  return { body: JSON.parse(text), check };
}

// What the slip shows for a lane's Seal:
// - `hidden` until the route has answered for this revision and hash. `retrying` is set while a
//   failed route waits to be asked again;
// - `withheld` when the route names another scope, so no Seal is shown;
// - `sealed` when the view carries a confirmation of this scope (the origin accepted it);
// - `unsealed` while the revision waits for a human to seal it.
export function sealOf(state, lane) {
  const entry = state.sealViews.get(lane.order);
  if (!entry || entry.key !== sealKey(lane)) return { kind: "hidden", retrying: false };
  if (entry.error !== null) return { kind: "hidden", retrying: true };
  if (entry.check.verdict !== "match") return { kind: "withheld", check: entry.check };
  if (lane.confirmation) {
    if (lane.confirmation.scope_hash !== entry.body.scope_hash) {
      return { kind: "withheld", check: entry.check, confirmed: lane.confirmation.scope_hash };
    }
    return { kind: "sealed", confirmation: lane.confirmation };
  }
  return { kind: "unsealed", body: entry.body };
}

// The open-choice answers and the broad-item acknowledgements the human gave. They are kept only
// while the scope they were given on is the one shown; a new scope starts them empty.
export function picksFor(state, lane) {
  if (state.sealPicks.hash !== lane.scope_hash) {
    state.sealPicks = { hash: lane.scope_hash, choices: new Map(), acks: new Set() };
  }
  return state.sealPicks;
}

// The ConfirmScope body for a route answer: the route's body with the human's answers and broad
// acknowledgements filled in. `missing` names each open choice without an answer and each broad
// plot without an acknowledgement, so the slip can say what is left. The route's selector hashes
// list the may-change items first, in scope order, so item `i` is hash `i`.
export function confirmationFor(body, lane, picks, at) {
  const missing = [];
  const answers = [];
  for (const choice of lane.open) {
    const option = picks.choices.get(choice.id);
    if (option === undefined) missing.push({ kind: "choice", id: choice.id });
    else answers.push({ choice_id: choice.id, option });
  }
  const broadGrants = [];
  lane.may.forEach((item, i) => {
    if (!item.broad) return;
    if (!picks.acks.has(item.id)) {
      missing.push({ kind: "broad", id: item.id });
      return;
    }
    broadGrants.push({ item_id: item.id, selector_hash: body.selector_hashes[i], by: body.by, at });
  });
  if (missing.length > 0) return { ok: false, missing };
  return { ok: true, confirmation: { ...body, answers, broad_grants: broadGrants, at } };
}

// The ConfirmScope a Seal sends for `lane`. The one sent last for this revision and hash is sent
// again as it was, with the same `at`, so the server takes it as the same confirmation. Otherwise
// one is built from a fresh route answer and the human's answers. The result is `{ command }`,
// `{ missing }` when an answer is left, or `{ mismatch: true }` when the route names another scope.
export async function sealCommand(state, fetchText, lane, picks, at) {
  const sent = sentSeal(state, lane);
  if (sent !== null) return { command: sent };
  const { body, check } = await askRoute(state, fetchText, lane.order);
  if (check.verdict !== "match") return { mismatch: true };
  const built = confirmationFor(body, lane, picks, at);
  if (!built.ok) return { missing: built.missing };
  const command = { cmd: "confirm_scope", order: lane.order, confirmation: built.confirmation };
  state.sealSent.set(lane.order, { key: sealKey(lane), command, accepted: false });
  return { command };
}

// What the slip's Seal block shows for `lane`, as the route and the view give it. `halt` is the line
// a stopped order carries. An unsealed revision offers the Seal when `offer` is set, which a stopped
// order never does, and `canSend` says whether the button is live. `missing` names what the human
// still owes an unsent confirmation.
export function sealPanel(state, lane, picks, now) {
  const sealState = sealOf(state, lane);
  const halt = lane.stopped ? STOPPED_LINE : null;
  if (sealState.kind === "hidden") return { kind: "hidden", halt, retrying: sealState.retrying };
  if (sealState.kind === "withheld") return { ...sealState, halt };
  if (sealState.kind === "sealed") return { ...sealState, halt };
  // A confirmation sent for this revision is sent again as it was, never built again.
  const sent = sentSeal(state, lane) !== null;
  const built = sent ? null : confirmationFor(sealState.body, lane, picks, now);
  const missing = built === null ? [] : built.missing ?? [];
  return {
    kind: "unsealed",
    halt,
    offer: !lane.stopped,
    sent,
    accepted: sent && sealAccepted(state, lane),
    missing,
    canSend: !lane.stopped && (sent || built.ok),
  };
}

// The line a missing answer or acknowledgement reads as: the choice's question, or the plot.
export function missingWords(missing, lane) {
  if (missing.kind === "choice") {
    const choice = lane.open.find((open) => open.id === missing.id);
    return `an answer to "${choice.question}"`;
  }
  return `an acknowledgement that plot ${missing.id} is broad`;
}

// The ConfirmScope last sent for this revision and hash, or null.
export function sentSeal(state, lane) {
  const entry = state.sealSent.get(lane.order);
  return entry?.key === sealKey(lane) ? entry.command : null;
}

// Whether the server accepted the ConfirmScope last sent for this revision and hash.
export function sealAccepted(state, lane) {
  const entry = state.sealSent.get(lane.order);
  return entry?.key === sealKey(lane) && entry.accepted;
}

export function markSealAccepted(state, lane) {
  const entry = state.sealSent.get(lane.order);
  if (entry?.key === sealKey(lane)) entry.accepted = true;
}

// Forgets the ConfirmScope sent for this revision and hash, once the server refused it for good.
export function forgetSeal(state, lane) {
  if (state.sealSent.get(lane.order)?.key === sealKey(lane)) state.sealSent.delete(lane.order);
}

// The scope one revision on, with the struck plot removed. Its constraints and open choices stay,
// so the Strike narrows the scope and never widens it. The last plot may go: the scope then has no
// may-change items.
export function strikeScope(scope, itemId) {
  const may = scope.body.may.filter((item) => item.id !== itemId);
  if (may.length === scope.body.may.length) throw new Error(`the scope has no plot ${itemId}`);
  return { ...scope, rev: scope.rev + 1, body: { ...scope.body, may } };
}

// What a Strike's warning says of a plot: what it removes and what it leaves. `last` is set when
// the plot is the order's last may-change item.
export function strikeWords(lane, item) {
  const obligations = item.obligations.length;
  const noun = obligations === 1 ? "obligation" : "obligations";
  return {
    removes: `Strike removes plot ${item.id} (${item.selector}, ${obligations} ${noun}). It ${leaseWords(lane.live_leases)}. Rev ${lane.rev + 1} waits for a seal.`,
    last: lane.may.length === 1 ? LAST_PLOT : null,
  };
}

// The lease count in words, as the Strike warns it.
export function leaseWords(count) {
  if (count === 0) return "ends no live leases";
  return `ends ${count} live ${count === 1 ? "lease" : "leases"}`;
}

// The plain sentence a refused command reads as. A refusal is a Rejection, which names its
// `reason`, or an error body, which names its `error`. The page shows the sentence, never the JSON.
// Each sentence says that nothing was recorded, which holds because a refused command appends
// nothing.
export function refusalWords(status, text) {
  const body = parseBody(text);
  const reason = body?.reason ?? body?.error;
  switch (reason) {
    case "forbidden":
      return "Only the order's poster or an owner can seal or strike this scope. Nothing was recorded.";
    case "stale_scope":
      return "The scope changed on the server, so this page is behind. Nothing was recorded.";
    case "budget_exhausted":
      return "The repair budget ran out, so this order is stopped. Nothing was recorded.";
    case "scope_not_confirmed":
      return "The scope has no seal yet. Nothing was recorded.";
    case "wrong_state":
      if (body.what === "scope" && body.found === "confirmed") {
        return "This revision already has a different seal on the server. Nothing was recorded.";
      }
      return `The ${body.what ?? "item"} is ${body.found ?? "in another state"}. Nothing was recorded.`;
    case "invalid":
      return `The server did not accept it: ${sentence(body.why)} Nothing was recorded.`;
    case "unknown":
    case "not_found":
      return `The server does not know this ${body.what ?? "item"}. Nothing was recorded.`;
    default:
      return status >= 500 ? "The server failed to take it. Try again." : "The server refused it. Nothing was recorded.";
  }
}

function parseBody(text) {
  try {
    const body = JSON.parse(text);
    return body !== null && typeof body === "object" ? body : null;
  } catch {
    return null;
  }
}

function sentence(text) {
  const plain = typeof text === "string" && text.length > 0 ? text : "the command is not valid";
  return /[.!?]$/.test(plain) ? plain : `${plain}.`;
}

// The repair budget a confirmation fixes, split as the slip states it. The server enforces the
// repair rounds (a bounce after the last round has no round left) and the new episodes (past the
// cap, the open episodes stop). It records the wall time and the model cost and enforces neither.
export function budgetWords(budget) {
  const rounds = budget.repair_rounds;
  return {
    enforced: [
      `repair rounds: at most ${rounds} ${rounds === 1 ? "round" : "rounds"}`,
      `new episodes: at most ${budget.max_episodes}`,
    ],
    recorded: [
      `${Math.round(budget.wall_time_ms / 60000)} min wall time`,
      `$${(budget.model_cost_micros / 1e6).toFixed(2)} model cost`,
    ],
  };
}
