// The agent lanes strip: one row per registered agent, with its task, machine, last activity
// and check. A row whose agent holds a task but has gone quiet reads stale, then away, by the same
// rule and the same clock as the map's drones (see `quietState`). The row logic has no DOM, so
// Node can test it. `installLanes` binds the strip to the page.
//
// Hover hook for the map: each row is `<li data-lane="<index>" data-agent="<agent id>">`, stable
// across repaints. `highlightLane(agentId)` marks that agent's row (class `hl`, and
// `data-highlighted="true"`), and `highlightLane(null)` clears it. The mark survives a repaint, so
// a caller sets it once on enter and clears it once on leave. The map's caller is the drone
// hover: main.js dispatches `acan:drone-hover` on document, a CustomEvent whose detail is
// `{ agent: <id> | null }`, with null when the pointer leaves the drone. This module listens for
// it once, at load, through `listenForDroneHover`.

import { workerCommand, workerSummary } from "./firstrun.js";
import { isRailOpen } from "./rail.js";

// A worker renews its lease every 20 seconds, so two minutes with no event from an agent that
// holds a task means it has stopped talking to the origin. From AWAY_MS the map takes its drone
// down and shows an away dot. Both mirror STALE_MS and AWAY_MS in the drone code (marks.rs), which
// the map ring reads; change them together.
export const STALE_MS = 2 * 60 * 1000;
export const AWAY_MS = 10 * 60 * 1000;

// How often the "2m ago" words are refreshed while nothing else changes.
export const TICK_MS = 15 * 1000;

// How long after G a digit or letter still counts as part of the G chord.
export const GO_MS = 1500;

// The letters other panels answer after G, such as `i` for the inbox. A digit rides an agent.
export const goKeys = new Map();

// When each agent was last heard of, as `{ at, seq }` by agent id. An agent is heard of when it
// is the actor, when it is registered, and when an event names a task it holds a lease on.
export function lastActivity(events) {
  const holders = new Map();
  const last = new Map();
  const touch = (agent, stamped) => {
    if (agent !== undefined && agent !== null) last.set(agent, { at: stamped.at, seq: stamped.seq });
  };
  for (const stamped of events) {
    const event = stamped.event;
    if (!event) continue;
    if (stamped.actor?.kind === "agent") touch(stamped.actor.id, stamped);
    switch (event.type) {
      case "agent_registered":
        touch(event.agent?.id, stamped);
        break;
      case "lease_granted":
        holders.set(event.task, event.lease?.holder);
        touch(event.lease?.holder, stamped);
        break;
      case "lease_ended":
        touch(holders.get(event.task), stamped);
        holders.delete(event.task);
        break;
      default:
        if (event.task !== undefined) touch(holders.get(event.task), stamped);
    }
  }
  return last;
}

// A quiet time in words: "just now", "40s ago", "2m ago", "3h ago" or "2d ago".
export function agoWords(ms) {
  const seconds = Math.floor(Math.max(0, ms) / 1000);
  if (seconds < 10) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

// The agent whose row is marked, or null. Kept here so a repaint marks the new row again.
let highlighted = null;

// Marks the lane row of `agentId`, or clears the mark for null. `root` is the strip; tests pass
// a stand-in. Returns whether a row now carries the mark.
export function highlightLane(agentId, root = typeof document === "undefined" ? null : document.getElementById("lanes")) {
  highlighted = agentId ?? null;
  return markRows(root);
}

// Makes `target` mark the lane of the drone under the pointer on each `acan:drone-hover`. `root`
// is passed to highlightLane, so undefined means the page's strip; tests pass stand-ins for both.
export function listenForDroneHover(target, root) {
  target.addEventListener("acan:drone-hover", (event) => highlightLane(event.detail?.agent ?? null, root));
}

if (typeof document !== "undefined") listenForDroneHover(document);

function markRows(root) {
  let found = false;
  for (const li of root?.querySelectorAll("li[data-agent]") ?? []) {
    const on = highlighted !== null && li.dataset.agent === highlighted;
    li.classList.toggle("hl", on);
    if (on) li.dataset.highlighted = "true";
    else delete li.dataset.highlighted;
    found ||= on;
  }
  return found;
}

function byName(a, b) {
  if (a.id < b.id) return -1;
  return a.id > b.id ? 1 : 0;
}

// How long an away agent has been quiet, as the map's away dot says it: whole minutes, then whole
// hours from an hour on, such as "12m" or "2h".
export function awayStamp(quietMs) {
  const minutes = Math.floor(quietMs / 60_000);
  return minutes < 60 ? `${minutes}m` : `${Math.floor(minutes / 60)}h`;
}

// The agents that wait on the user: their task is in an open or proposed clash, or their order
// asks a question. The map gives their drones a waiting ring, never stale or away.
export function waitingAgents(view) {
  const clashed = new Set((view.clashes ?? []).filter((c) => c.state === "open" || c.state === "proposed").flatMap((c) => [c.a, c.b]));
  const asked = new Set((view.questions ?? []).map((q) => q.order));
  const tasks = new Map(view.tasks.map((task) => [task.id, task]));
  return new Set(view.agents.filter((agent) => agent.task !== null && agent.task !== undefined
    && (clashed.has(agent.task) || asked.has(tasks.get(agent.task)?.order))).map((agent) => agent.id));
}

// "stale", "away" or null for one agent, by the map's rule: only an agent that holds a task (not
// on read-back) and does not wait on the user goes quiet; stale from STALE_MS to AWAY_MS, away
// from AWAY_MS. `quietMs` is null on a page with no live clock (frozen, replayed or rewound),
// which the map is given no time on either, so nothing there is stale or away.
export function quietState({ holds, waiting, quietMs }) {
  if (!holds || waiting || quietMs === null) return null;
  if (quietMs >= AWAY_MS) return "away";
  return quietMs >= STALE_MS ? "stale" : null;
}

// One row per agent, sorted by name as Tab rides them. `now` is the clock the "ago" words are
// read against, in unix milliseconds. `live` says the page has the live clock, Date.now(), the
// one the map's drones read; without it no row is stale or away.
export function laneRows(view, activity, now, { live = true } = {}) {
  const tasks = new Map(view.tasks.map((task) => [task.id, task]));
  const waiting = waitingAgents(view);
  return [...view.agents].sort(byName).map((agent, index) => {
    const task = agent.task === null || agent.task === undefined ? null : tasks.get(agent.task) ?? null;
    const seen = activity.get(agent.id) ?? null;
    const quiet = seen === null ? null : Math.max(0, now - seen.at);
    const state = quietState({
      holds: task !== null && agent.activity !== "readback",
      waiting: waiting.has(agent.id),
      quietMs: live ? quiet : null,
    });
    return {
      agent: agent.id,
      key: index < 9 ? index + 1 : null,
      task: task === null ? null : `t-${task.id}`,
      title: task?.title ?? "",
      machine: agent.machine ?? task?.machine ?? null,
      activity: agent.activity.replaceAll("_", " "),
      ago: quiet === null ? "not heard from" : agoWords(quiet),
      quiet: state,
      stale: state === "stale",
      away: state === "away",
      stamp: state === "away" ? awayStamp(quiet) : null,
      check: task?.check ?? null,
      rideable: Array.isArray(agent.body),
    };
  });
}

// The check in words, for the row's label: "check w-3 passed".
export function checkWords(check) {
  return check === null ? "" : `check ${check.name} ${check.state}`;
}

// What the strip says with no agent, on a server without GET /api/workers: the line and the
// command that starts an outside worker, the same command the first-run checklist offers.
export function emptyLines(origin) {
  return {
    line: "No worker connected.",
    command: workerCommand(origin),
  };
}

// The strip's head status and its empty state, from the shared GET /api/workers answer and the
// view's agents. With the route, its words are the first-run checklist's own; without it (404,
// or no answer yet) the strip infers from the view's agents as before.
export function workerLines(answer, agents, origin) {
  const summary = workerSummary(answer, agents, origin);
  if (summary.kind === "outside" || summary.kind === "connected") {
    const { line, command } = emptyLines(origin);
    return { kind: summary.kind, status: summary.status, line, command, setKey: false };
  }
  return { kind: summary.kind, status: summary.status, line: summary.detail, command: null, setKey: summary.setKey };
}

// The G chord. `armed` is when G was pressed, or null. Returns the chord's new state, whether
// the key was taken, and what to do: `{ kind: "agent", index }` for a digit 1 to 9, or
// `{ kind: "go", key }` for a letter another panel answers.
export function goStep(armed, key, now, letters = goKeys) {
  if (key === "g" || key === "G") return { armed: now, taken: true, action: null };
  if (armed === null || now - armed > GO_MS) return { armed: null, taken: false, action: null };
  if (/^[1-9]$/.test(key)) return { armed: null, taken: true, action: { kind: "agent", index: Number(key) - 1 } };
  const letter = key.toLowerCase();
  if (letters.has(letter)) return { armed: null, taken: true, action: { kind: "go", key: letter } };
  return { armed: null, taken: false, action: null };
}

// Eases the camera to machine `machine`. A ride is left first, and the move waits until the
// ride has eased back, since the wasm side moves no camera during a ride.
export function flyTo(kit, machine) {
  const { state, requestRender } = kit;
  const reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  if (state.ridden !== "") state.app.leave();
  let tries = 0;
  const go = () => {
    if (state.app.tour_go(machine, "machine", reduced) || ++tries > 10) {
      requestRender();
      return;
    }
    requestRender();
    setTimeout(go, 120);
  };
  go();
}

// Rides the row's agent, or flies to its machine when it has no drone to ride.
function visit(kit, row) {
  if (row.rideable && kit.state.app.ride(row.agent)) {
    kit.requestRender();
  } else if (row.machine !== null) {
    flyTo(kit, row.machine);
  }
}

function element(tag, className, text) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  if (text !== undefined) el.textContent = text;
  return el;
}

// The words a row shows, joined, so a change can be seen and marked.
function signature(row) {
  return [row.task, row.activity, row.machine, row.quiet, row.check?.state].join("|");
}

function rowButton(row) {
  const button = element("button", "lane");
  button.type = "button";
  const key = element("kbd", "key", row.key === null ? "" : String(row.key));
  if (row.key === null) key.hidden = true;
  const name = element("span", "name", row.agent);
  const words = row.away ? `away · ${row.stamp}` : row.stale ? `stale · ${row.ago}` : row.ago;
  const when = element("span", row.quiet ? `when ${row.quiet}` : "when", words);
  const doing = element("span", "doing", [row.task, row.activity, row.machine].filter((part) => part).join(" · "));
  const check = element("span", `chip ${row.check?.state ?? ""}`);
  if (row.check !== null) {
    check.append(element("i"), element("span", undefined, row.check.name));
  }
  button.append(key, name, when, doing, check);
  if (row.title) button.title = row.title;
  const where = row.rideable ? "Ride" : row.machine ? "Go to the machine of" : "No drone for";
  const parts = [`${where} ${row.agent}`, row.task && `on ${row.task} ${row.activity}`, row.machine && `at ${row.machine}`,
    `last heard ${row.ago}`, row.quiet, checkWords(row.check)];
  button.setAttribute("aria-label", parts.filter((part) => part).join(", "));
  return button;
}

// The worker line as nodes: its words with `code` in backticks, the command, and Set a key, which
// opens the page's key setup when the shell shows its button. The command folds under a summary,
// since the first-run checklist often shows the same one; `open` keeps it unfolded across repaints.
function workerWords(workers, open = false) {
  const nodes = [element("span", undefined, "")];
  nodes[0].replaceChildren(...workers.line.split("`").map((part, i) => (i % 2 === 0 ? document.createTextNode(part) : element("code", undefined, part))));
  if (workers.command) {
    const fold = element("details", "start");
    fold.open = open;
    fold.append(element("summary", undefined, "start one"), element("code", undefined, workers.command));
    nodes.push(fold);
  }
  if (workers.setKey) {
    const opener = ["setup-open", "setup-manage"].map((id) => document.getElementById(id)).find((el) => el !== null && !el.hidden);
    if (opener) {
      const button = element("button", "set-key", "Set a key");
      button.type = "button";
      button.addEventListener("click", () => opener.click());
      nodes.push(button);
    }
  }
  return nodes;
}

// Binds the strip to `#lanes`. `kit` carries the page state, the feed, the clock, and the
// registration points for paint and keys.
export function installLanes(kit) {
  const panel = document.getElementById("lanes");
  const list = element("ol");
  const empty = element("p", "empty");
  const head = element("h3");
  const status = element("span", "status");
  status.setAttribute("aria-live", "polite");
  head.append(element("span", undefined, "Agents"), status, element("span", "keys"));
  head.lastChild.innerHTML = "<kbd>G</kbd> <kbd>1</kbd>-<kbd>9</kbd> ride";
  // A worker waiting for a model key says so above the rows too, since its rows can still show.
  const waiting = element("p", "workers");
  panel.append(head, waiting, list, empty);
  const shown = new Map();
  let last = { version: -1, feed: -1, tick: -1 };

  const paint = () => {
    const view = kit.state.view;
    if (view === null) return;
    const now = kit.clock();
    const tick = Math.floor(now / TICK_MS);
    kit.workers?.refresh();
    const answer = kit.workers?.answer() ?? null;
    if (last.version === kit.state.renderedVersion && last.feed === kit.feed.version && last.tick === tick && last.answer === answer) return;
    last = { version: kit.state.renderedVersion, feed: kit.feed.version, tick, answer };
    panel.hidden = !isRailOpen("lanes");
    const rows = laneRows(view, lastActivity(kit.feed.events), now, { live: kit.isLive() });
    kit.lanes = rows;
    empty.hidden = rows.length > 0;
    list.hidden = rows.length === 0;
    const workers = workerLines(answer, view.agents.length, location.origin);
    // With no rows the empty state says it in full, so the head does not repeat it.
    status.textContent = rows.length === 0 ? "" : workers.status;
    status.dataset.kind = workers.kind;
    waiting.hidden = !(workers.setKey && rows.length > 0);
    if (!waiting.hidden) waiting.replaceChildren(...workerWords(workers));
    if (rows.length === 0) empty.replaceChildren(...workerWords(workers, empty.querySelector("details")?.open));
    const focused = list.contains(document.activeElement) ? document.activeElement.dataset.agent : null;
    const items = rows.map((row, index) => {
      const before = shown.get(row.agent);
      const li = element("li");
      li.dataset.lane = String(index);
      li.dataset.agent = row.agent;
      const button = rowButton(row);
      button.dataset.agent = row.agent;
      button.addEventListener("click", () => visit(kit, row));
      li.append(button);
      // A row whose task, activity, machine or check moved glows once, so a change is seen.
      if (before !== undefined && before !== signature(row)) li.classList.add("moved");
      return li;
    });
    shown.clear();
    for (const row of rows) shown.set(row.agent, signature(row));
    list.replaceChildren(...items);
    markRows(panel);
    // A rebuild keeps the keyboard where it was.
    if (focused) list.querySelector(`button[data-agent="${CSS.escape(focused)}"]`)?.focus();
  };

  kit.onPaint(paint);
  kit.workers?.onChange(paint);
  setInterval(paint, TICK_MS);
  // G and a digit ride that lane's agent; keys.js owns the chord and calls this.
  kit.rideLane = (index) => {
    const row = kit.lanes?.[index];
    if (row) visit(kit, row);
  };
}
