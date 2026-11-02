// The Seal and the Strike: which lanes ask the origin's confirmation route, what the Seal and the
// Strike send, and what the slip says of the route's answer. The wasm side checks the route's hash
// against the page's own. This module has no DOM imports, so Node can test it.

// Orders whose route is being fetched. A sync skips them, so a slow fetch is not repeated.
const inFlight = new Set();

// The revision and hash a route answer is kept under. The route names the scope the page holds, so
// a new revision or a new hash asks again. The Seal fetches the route again when it is sent.
export function sealKey(lane) {
  return `${lane.rev}|${lane.scope_hash}`;
}

// Fetches the route of each lane whose revision or hash moved. `fetchText` is the stream's `get`,
// and `repaint` redraws the slip once an answer is kept.
export function syncSeals(state, fetchText, repaint) {
  for (const lane of state.view.lanes) {
    const key = sealKey(lane);
    if (state.sealViews.get(lane.order)?.key === key || inFlight.has(lane.order)) continue;
    fetchSeal(state, fetchText, repaint, lane, key);
  }
}

async function fetchSeal(state, fetchText, repaint, lane, key) {
  inFlight.add(lane.order);
  try {
    const { body, check } = await askRoute(state, fetchText, lane.order);
    state.sealViews.set(lane.order, { key, body, check, error: null });
  } catch (err) {
    const error = String(err?.message ?? err);
    console.error(`the confirmation of o-${lane.order} was not kept: ${error}`);
    state.sealViews.set(lane.order, { key, body: null, check: null, error });
  } finally {
    inFlight.delete(lane.order);
    repaint();
    // A revision that moved while this fetch was in flight is asked for now, not at the next event.
    syncSeals(state, fetchText, repaint);
  }
}

// The origin's confirmation body for `order`, and the wasm side's check of it against the page's
// scope. The check throws when the body is not the route's shape or the page holds no such scope.
export async function askRoute(state, fetchText, order) {
  const id = `o-${order}`;
  const text = await fetchText(`/api/orders/${id}/scope/confirmation`);
  const check = JSON.parse(state.app.seal_check(id, text));
  return { body: JSON.parse(text), check };
}

// What the slip shows for a lane's Seal:
// - `hidden` until the route has answered for this revision and hash, or after it failed;
// - `withheld` when the route names another scope, so no Seal is shown;
// - `sealed` when the view carries a confirmation of this scope (the origin accepted it);
// - `unsealed` while the revision waits for a human to seal it.
export function sealOf(state, lane) {
  const entry = state.sealViews.get(lane.order);
  if (!entry || entry.key !== sealKey(lane) || entry.error !== null) return { kind: "hidden" };
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

// The scope one revision on, with the struck plot removed. Its constraints and open choices stay,
// so the Strike narrows the scope and never widens it.
export function strikeScope(scope, itemId) {
  const may = scope.body.may.filter((item) => item.id !== itemId);
  if (may.length === scope.body.may.length) throw new Error(`the scope has no plot ${itemId}`);
  return { ...scope, rev: scope.rev + 1, body: { ...scope.body, may } };
}

// The lease count in words, as the Strike warns it.
export function leaseWords(count) {
  if (count === 0) return "ends no live leases";
  return `ends ${count} live ${count === 1 ? "lease" : "leases"}`;
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
