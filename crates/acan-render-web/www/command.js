// The command bar's queries and verbs, over the view the page holds. No model is asked: a query
// matches names, ids and paths the view and the receipts already carry. This module has no imports
// and touches no DOM, so Node can test it.
//
// A query is an optional verb, then a term:
// - `go auth` or `fly auth` flies to a machine, or to the machine of an agent or an order;
// - `ride w2` rides an agent's drone;
// - `agent w2` finds agents, to ride;
// - `touched auth` finds the declared task paths and the receipt items that name the term;
// - `order here` opens the compose panel where the page looks, `order auth` opens it on auth;
// - `seal` opens the slip on the order it shows;
// - a bare term finds machines, agents, orders, questions and clashes.
// Each result is `{ kind, key, label, detail, verb, target }`. `verb` says what Enter does.

export const MAX_RESULTS = 8;

const VERBS = new Map([
  ["go", "go"],
  ["fly", "go"],
  ["ride", "ride"],
  ["agent", "agent"],
  ["agents", "agent"],
  ["touched", "touched"],
  ["order", "order"],
  ["seal", "seal"],
]);

// The order kinds sort in when two results score the same.
const KIND_RANK = { question: 0, clash: 1, machine: 2, order: 3, agent: 4, path: 5, item: 6, lane: 7, hint: 8 };

export const VERB_WORDS = { go: "go", ride: "ride", order: "order", seal: "seal", question: "answer", clash: "open", hint: "fill" };

export function parseQuery(text) {
  const trimmed = String(text ?? "").trim().replace(/\s+/g, " ");
  const space = trimmed.indexOf(" ");
  const head = (space < 0 ? trimmed : trimmed.slice(0, space)).toLowerCase();
  if (VERBS.has(head)) {
    return { verb: VERBS.get(head), term: space < 0 ? "" : trimmed.slice(space + 1) };
  }
  return { verb: null, term: trimmed };
}

// How well `term` matches `text`, lower is better, or null when it does not: 0 the same, 1 a
// prefix, 2 the start of a part after `/`, `.`, `-`, `_` or a space, 3 inside it, 4 its letters
// in order. An empty term matches everything at 5.
export function score(term, text) {
  const t = String(term).toLowerCase();
  const s = String(text).toLowerCase();
  if (t === "") return 5;
  if (s === t) return 0;
  if (s.startsWith(t)) return 1;
  if (s.split(/[/._\-\s]+/).some((part) => part.startsWith(t))) return 2;
  if (s.includes(t)) return 3;
  let at = 0;
  for (const ch of s) {
    if (ch === t[at]) at += 1;
    if (at === t.length) return 4;
  }
  return null;
}

// The score of an id term such as `o-3`, `o3` or `3` against the id `prefix-n`: 0 when it names
// it, else null.
export function idScore(term, prefix, id) {
  const t = String(term).toLowerCase().replace(/\s+/g, "");
  const n = String(id);
  return t === n || t === `${prefix}${n}` || t === `${prefix}-${n}` ? 0 : null;
}

// The results for `text`. `extra` carries what the view does not: `{ receipts, shownOrder }`,
// where `receipts` are the receipt items `{ order, item_id, machine_id, class }` and
// `shownOrder` is the id of the order the slip shows, or null.
export function search(view, text, extra = {}) {
  const { verb, term } = parseQuery(text);
  const found = [];
  const push = (result, rank) => {
    if (rank !== null) found.push({ ...result, rank });
  };
  switch (verb) {
    case null:
      if (term === "") return suggestions(view, extra);
      machines(view, term, "go", push);
      agents(view, term, "ride", push);
      orders(view, term, push);
      waiting(view, term, push);
      break;
    case "go":
      if (term === "") return [hint("go", "go ", "a machine, an agent or an order")];
      machines(view, term, "go", push);
      agents(view, term, "go", push);
      orders(view, term, push);
      break;
    case "ride":
    case "agent":
      agents(view, term, "ride", push);
      break;
    case "touched":
      if (term === "") return [hint("touched", "touched ", "a path or a machine")];
      touched(view, term, extra.receipts ?? [], push);
      break;
    case "order":
      if (term === "" || term.toLowerCase() === "here") {
        return [{ kind: "machine", key: "order-here", label: "order here", detail: "where the page looks", verb: "order", target: { machine: null } }];
      }
      machines(view, term, "order", push);
      break;
    case "seal":
      lanes(view, term, extra.shownOrder ?? null, push);
      break;
    default:
  }
  return ranked(found);
}

function ranked(found) {
  const seen = new Set();
  return found
    .sort((a, b) => a.rank - b.rank || KIND_RANK[a.kind] - KIND_RANK[b.kind] || a.label.length - b.label.length
      || a.label.localeCompare(b.label))
    .filter((result) => (seen.has(result.key) ? false : seen.add(result.key)))
    .slice(0, MAX_RESULTS)
    .map(({ rank, ...result }) => result);
}

// An empty bar shows what waits on the human first, then the verbs.
function suggestions(view, extra) {
  const out = [];
  waiting(view, "", (result) => out.push(result));
  out.push(hint("go", "go ", "fly to a machine, an agent or an order"));
  out.push(hint("ride", "ride ", "ride an agent's drone"));
  out.push({ kind: "machine", key: "order-here", label: "order here", detail: "post an order where the page looks", verb: "order", target: { machine: null } });
  out.push(hint("touched", "touched ", "find declared paths and receipt items"));
  if (extra.shownOrder !== null && extra.shownOrder !== undefined
    && view.lanes.some((lane) => lane.order === extra.shownOrder)) {
    out.push({ kind: "lane", key: "seal", label: "seal", detail: `open the slip on o-${extra.shownOrder}`, verb: "seal", target: { order: extra.shownOrder } });
  }
  return out.slice(0, MAX_RESULTS);
}

function hint(name, fill, detail) {
  return { kind: "hint", key: `hint-${name}`, label: fill.trim(), detail, verb: "hint", target: { fill } };
}

function machines(view, term, verb, push) {
  for (const machine of view.machines) {
    push({
      kind: "machine",
      key: `${verb}-machine-${machine.name}`,
      label: verb === "order" ? `order on ${machine.name}` : machine.name,
      detail: machine.abnormal ? "machine · a clash stands here" : "machine",
      verb,
      target: { machine: machine.name },
    }, score(term, machine.name));
  }
}

function agents(view, term, verb, push) {
  for (const agent of view.agents) {
    const activity = agent.activity.replace(/_/g, " ");
    const where = agent.machine === null ? activity : `${activity} on ${agent.machine}`;
    if (verb === "go" && agent.machine === null) continue;
    push({
      kind: "agent",
      key: `${verb}-agent-${agent.id}`,
      label: agent.id,
      detail: `agent · ${where}`,
      verb,
      target: verb === "go" ? { machine: agent.machine } : { agent: agent.id },
    }, score(term, agent.id));
  }
}

function orders(view, term, push) {
  for (const order of view.orders) {
    const byId = idScore(term, "o", order.id);
    const bySentence = term.length >= 3 && order.sentence.toLowerCase().includes(term.toLowerCase()) ? 3 : null;
    push({
      kind: "order",
      key: `go-order-${order.id}`,
      label: `o-${order.id} · ${order.sentence}`,
      detail: `order · ${order.phase.replace(/_/g, " ")} on ${order.machine}`,
      verb: "go",
      target: { machine: order.machine },
    }, byId ?? bySentence);
  }
}

// The open questions and the unsettled clashes: what waits on the human.
function waiting(view, term, push) {
  for (const question of view.questions) {
    const rank = term === "" ? 0 : idScore(term, "q", question.id) ?? (term.length >= 3 && question.text.toLowerCase().includes(term.toLowerCase()) ? 3 : null);
    push({
      kind: "question",
      key: `question-${question.id}`,
      label: `q-${question.id} · ${question.text}`,
      detail: `waits on you · o-${question.order} asks`,
      verb: "question",
      target: { question: question.id },
    }, rank);
  }
  view.clashes.forEach((clash, index) => {
    if (clash.state !== "open" && clash.state !== "proposed") return;
    const rank = term === "" ? 0 : idScore(term, "c", clash.id);
    push({
      kind: "clash",
      key: `clash-${clash.id}`,
      label: clash.label,
      detail: "waits on you · a clash to settle",
      verb: "clash",
      target: { clash: index },
    }, rank);
  });
}

// The declared paths of the tasks and the receipt items that name the term. Each result says
// which it is, so a declaration is never read as a change that landed.
function touched(view, term, receipts, push) {
  for (const task of view.tasks) {
    for (const path of task.paths) {
      const rank = score(term, path);
      if (rank === null || rank > 3) continue;
      push({
        kind: "path",
        key: `path-${task.id}-${path}`,
        label: path,
        detail: `declared path · t-${task.id} of o-${task.order}${task.machine ? ` on ${task.machine}` : ""}`,
        verb: "go",
        target: { machine: task.machine },
      }, rank);
    }
  }
  for (const item of receipts) {
    const rank = Math.min(score(term, item.item_id) ?? 9, score(term, item.machine_id ?? "") ?? 9);
    if (rank > 3) continue;
    push({
      kind: "item",
      key: `item-${item.order}-${item.item_id}`,
      label: item.item_id,
      detail: `receipt item · o-${item.order} · ${item.class} on ${item.machine_id}`,
      verb: "go",
      target: { machine: item.machine_id },
    }, rank);
  }
}

// The scope lanes. Only the lane of the order the slip shows can be sealed from the slip.
function lanes(view, term, shownOrder, push) {
  for (const lane of view.lanes) {
    const rank = term === "" ? (lane.order === shownOrder ? 0 : 1) : idScore(term, "o", lane.order);
    const sealed = lane.confirmation ? `sealed rev ${lane.confirmation.scope_rev}` : `unsealed rev ${lane.rev}`;
    push({
      kind: "lane",
      key: `seal-${lane.order}`,
      label: `seal o-${lane.order}`,
      detail: lane.order === shownOrder ? `scope · ${sealed}` : `scope · ${sealed} · the slip shows o-${shownOrder} now`,
      verb: "seal",
      target: { order: lane.order },
    }, rank);
  }
}

// The receipt items of every order the page holds a plant view of, for `touched`.
export function receiptItems(plantViews) {
  const out = [];
  for (const [order, entry] of plantViews) {
    const items = entry.play ? entry.play.items : entry.items ?? [];
    for (const item of items) out.push({ order, item_id: item.item_id, machine_id: item.machine_id, class: item.class });
  }
  return out;
}
