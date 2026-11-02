// The agent lanes strip: one row per registered agent, with its task, machine, last activity
// and check. A row whose agent holds a task but has sent nothing for STALE_MS is flagged stale.
// The row logic has no DOM, so Node can test it. `installLanes` binds the strip to the page.

// A worker renews its lease every 20 seconds, so two minutes with no event from an agent that
// holds a task means it has stopped talking to the origin.
export const STALE_MS = 2 * 60 * 1000;

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

function byName(a, b) {
  if (a.id < b.id) return -1;
  return a.id > b.id ? 1 : 0;
}

// One row per agent, sorted by name as Tab rides them. `now` is the clock the rows are read
// against, in unix milliseconds. Only an agent that holds a task can be stale: an idle agent has
// nothing to report.
export function laneRows(view, activity, now, staleMs = STALE_MS) {
  const tasks = new Map(view.tasks.map((task) => [task.id, task]));
  return [...view.agents].sort(byName).map((agent, index) => {
    const task = agent.task === null || agent.task === undefined ? null : tasks.get(agent.task) ?? null;
    const seen = activity.get(agent.id) ?? null;
    const quiet = seen === null ? null : Math.max(0, now - seen.at);
    return {
      agent: agent.id,
      key: index < 9 ? index + 1 : null,
      task: task === null ? null : `t-${task.id}`,
      title: task?.title ?? "",
      machine: agent.machine ?? task?.machine ?? null,
      activity: agent.activity.replaceAll("_", " "),
      ago: quiet === null ? "not heard from" : agoWords(quiet),
      stale: task !== null && quiet !== null && quiet >= staleMs,
      check: task?.check ?? null,
      rideable: Array.isArray(agent.body),
    };
  });
}

// The check in words, for the row's label: "check w-3 passed".
export function checkWords(check) {
  return check === null ? "" : `check ${check.name} ${check.state}`;
}

// What the strip says with no agent: the line and the command that starts a worker.
export function emptyLines(origin) {
  return {
    line: "No worker connected. Start one with:",
    command: `acan-worker --server ${origin} --token <agent token> --repo <repo path>`,
  };
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
  return [row.task, row.activity, row.machine, row.stale, row.check?.state].join("|");
}

function rowButton(row) {
  const button = element("button", "lane");
  button.type = "button";
  const key = element("kbd", "key", row.key === null ? "" : String(row.key));
  if (row.key === null) key.hidden = true;
  const name = element("span", "name", row.agent);
  const when = element("span", row.stale ? "when stale" : "when", row.stale ? `stale · ${row.ago}` : row.ago);
  const doing = element("span", "doing", [row.task, row.activity, row.machine].filter((part) => part).join(" · "));
  const check = element("span", `chip ${row.check?.state ?? ""}`);
  if (row.check !== null) {
    check.append(element("i"), element("span", undefined, row.check.name));
  }
  button.append(key, name, when, doing, check);
  if (row.title) button.title = row.title;
  const where = row.rideable ? "Ride" : row.machine ? "Go to the machine of" : "No drone for";
  const parts = [`${where} ${row.agent}`, row.task && `on ${row.task} ${row.activity}`, row.machine && `at ${row.machine}`,
    `last heard ${row.ago}`, row.stale && "stale", checkWords(row.check)];
  button.setAttribute("aria-label", parts.filter((part) => part).join(", "));
  return button;
}

// Binds the strip to `#lanes`. `kit` carries the page state, the feed, the clock, and the
// registration points for paint and keys.
export function installLanes(kit) {
  const panel = document.getElementById("lanes");
  const list = element("ol");
  const empty = element("p", "empty");
  const head = element("h3");
  head.append(element("span", undefined, "Agents"), element("span", "keys"));
  head.lastChild.innerHTML = "<kbd>G</kbd> <kbd>1</kbd>-<kbd>9</kbd> ride";
  panel.append(head, list, empty);
  const shown = new Map();
  let armed = null;
  let last = { version: -1, feed: -1, tick: -1 };

  const paint = () => {
    const view = kit.state.view;
    if (view === null) return;
    const now = kit.clock();
    const tick = Math.floor(now / TICK_MS);
    if (last.version === kit.state.renderedVersion && last.feed === kit.feed.version && last.tick === tick) return;
    last = { version: kit.state.renderedVersion, feed: kit.feed.version, tick };
    panel.hidden = false;
    const rows = laneRows(view, lastActivity(kit.feed.events), now);
    kit.lanes = rows;
    empty.hidden = rows.length > 0;
    list.hidden = rows.length === 0;
    if (rows.length === 0) {
      const { line, command } = emptyLines(location.origin);
      empty.replaceChildren(element("span", undefined, line), element("code", undefined, command));
    }
    const focused = list.contains(document.activeElement) ? document.activeElement.dataset.agent : null;
    const items = rows.map((row) => {
      const before = shown.get(row.agent);
      const li = element("li");
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
    // A rebuild keeps the keyboard where it was.
    if (focused) list.querySelector(`button[data-agent="${CSS.escape(focused)}"]`)?.focus();
  };

  kit.onPaint(paint);
  setInterval(paint, TICK_MS);
  kit.onKey((event) => {
    if (event.repeat) return false;
    const step = goStep(armed, event.key, performance.now());
    armed = step.armed;
    if (step.action?.kind === "agent") {
      const row = kit.lanes?.[step.action.index];
      if (row) visit(kit, row);
    } else if (step.action?.kind === "go") {
      goKeys.get(step.action.key)();
    }
    if (step.taken) event.preventDefault();
    return step.taken;
  });
}
