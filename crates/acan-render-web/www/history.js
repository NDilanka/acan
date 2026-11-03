// The land and history ribbon: one line per order, from what the log says happened to it. An
// order is posted, then sealed when a human confirms its scope, then gated when a gate run
// closes or a receipt is recorded, and it ends landed with a commit, failed with the reason, or
// cancelled. A live order has a Cancel that sends `cancel_order`. The ribbon logic has no DOM,
// so Node can test it. `installHistory` binds it to the page.
import { historyBlock } from "./guard.js";
import { flyTo, goKeys } from "./lanes.js";
import { isRailOpen, openRailTab } from "./rail.js";

// The ribbon shows this many orders at rest, newest first. Alt shows every one.
export const REST_ORDERS = 5;

// How long the Cancel waits for its confirm before it disarms.
export const ARM_MS = 4000;

const SHA_CHARS = 7;

const DONE_PHASES = ["landed", "cancelled"];

function verdictWord(verdict) {
  const word = typeof verdict === "string" ? verdict : verdict?.verdict;
  return word ? word.replaceAll("_", " ") : null;
}

// What happened to each order, by order id, in log order. Tasks are tied to their order by the
// split, and gate runs to their task by the build report.
export function orderHistory(events) {
  const orders = new Map();
  const taskOrder = new Map();
  const runTask = new Map();
  const orderOfTask = (task) => orders.get(taskOrder.get(task));
  const orderOfRun = (run) => orderOfTask(runTask.get(run));
  for (const { at, seq, event } of events) {
    if (!event) continue;
    switch (event.type) {
      case "order_posted":
        orders.set(event.order, {
          id: event.order,
          machine: event.machine,
          sentence: event.sentence,
          posted: { at, seq },
          sealed: null,
          gated: null,
          landed: [],
          failed: null,
          cancelled: null,
        });
        break;
      case "order_split":
        for (const [task] of event.tasks ?? []) taskOrder.set(task, event.order);
        break;
      // A re-bind to a moved head carries the seal forward, so it reads as sealed too.
      case "scope_confirmed":
      case "scope_rebound": {
        const order = orders.get(event.order);
        if (order) order.sealed = { at, seq, rev: event.confirmation?.scope_rev ?? null };
        break;
      }
      case "scope_revised": {
        // A new revision voids the seal, so the ribbon waits for the next one.
        const order = orders.get(event.order);
        if (order) order.sealed = null;
        break;
      }
      case "build_reported":
        runTask.set(event.run, event.task);
        break;
      case "gate_closed": {
        const order = orderOfRun(event.run);
        if (order) order.gated = { at, seq, verdict: verdictWord(event.verdict) };
        break;
      }
      case "receipt_recorded": {
        const order = orderOfRun(event.run);
        if (order) order.gated = { at, seq, verdict: verdictWord(event.record?.verdict) };
        break;
      }
      case "landed": {
        const order = orderOfTask(event.task);
        if (order) {
          order.landed.push({ task: event.task, commit: String(event.commit), at, seq });
          if (order.failed?.task === event.task) order.failed = null;
        }
        break;
      }
      case "land_failed": {
        const order = orderOfTask(event.task);
        if (order) order.failed = { task: event.task, reason: event.reason, at, seq };
        break;
      }
      case "order_cancelled": {
        const order = orders.get(event.order);
        if (order) order.cancelled = { at, seq };
        break;
      }
      default:
    }
  }
  return orders;
}

export function shortSha(commit) {
  return String(commit).slice(0, SHA_CHARS);
}

// The clock time of an event, as "14:02".
export function clockWords(at, timeZone) {
  return new Date(at).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", timeZone });
}

// One row per order, newest first. Each row has its steps in order and how it ends, as words:
// `landed <sha>`, `failed: <reason>`, `cancelled`, or the live phase. `view` gives each order's
// phase, which decides whether it can still be cancelled.
export function ribbonRows(history, view, timeZone) {
  const phases = new Map(view.orders.map((order) => [order.id, order.phase]));
  return [...history.values()].reverse().map((order) => {
    const phase = phases.get(order.id) ?? "unknown";
    const steps = [{ kind: "posted", text: `posted ${clockWords(order.posted.at, timeZone)}` }];
    if (order.sealed) steps.push({ kind: "sealed", text: order.sealed.rev === null ? "sealed" : `sealed rev ${order.sealed.rev}` });
    if (order.gated) steps.push({ kind: "gated", text: order.gated.verdict ? `gated ${order.gated.verdict}` : "gated" });
    let end;
    if (order.cancelled || phase === "cancelled") {
      end = { kind: "cancelled", text: "cancelled" };
    } else if (order.landed.length > 0 && (phase === "landed" || order.failed === null)) {
      const last = order.landed.at(-1);
      const more = order.landed.length > 1 ? ` +${order.landed.length - 1}` : "";
      end = { kind: phase === "landed" ? "landed" : "landing", text: `landed ${shortSha(last.commit)}${more}`, commit: last.commit };
    } else if (order.failed) {
      end = { kind: "failed", text: `failed: ${order.failed.reason}` };
    } else {
      end = { kind: "live", text: phase.replaceAll("_", " ") };
    }
    return {
      id: order.id,
      label: `o-${order.id}`,
      machine: order.machine,
      sentence: order.sentence,
      steps,
      end,
      cancellable: !DONE_PHASES.includes(phase) && !order.cancelled,
    };
  });
}

// The command the Cancel sends, as the origin's HumanCmd takes it.
export function cancelCommand(order) {
  return { cmd: "cancel_order", order };
}

function parseBody(text) {
  try {
    const body = JSON.parse(text);
    return body !== null && typeof body === "object" ? body : null;
  } catch {
    return null;
  }
}

// A refused Cancel in one plain sentence, never the server's JSON.
export function cancelRefusal(status, text) {
  const body = parseBody(text);
  switch (body?.reason ?? body?.error) {
    case "forbidden":
      return "Only the order's poster or an owner can cancel it, so nothing was recorded.";
    case "wrong_state":
      return body.found === "landed"
        ? "The order has already landed, so it cannot be cancelled."
        : `The order is ${body.found ?? "in another state"}, so nothing was recorded.`;
    case "unknown":
    case "not_found":
      return "The server does not know this order, so nothing was recorded.";
    default:
      return status >= 500 ? "The server failed to take the cancel, so try again." : "The server refused the cancel, so nothing was recorded.";
  }
}

function element(tag, className, text) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  if (text !== undefined) el.textContent = text;
  return el;
}

// Binds the ribbon to `#history`. A Cancel asks once more before it sends.
export function installHistory(kit) {
  const panel = document.getElementById("history");
  const head = element("h3");
  const keys = element("span", "keys");
  keys.innerHTML = "<kbd>G</kbd> <kbd>H</kbd>";
  head.append(element("span", undefined, "Orders"), keys);
  const list = element("ol");
  const more = element("p", "more");
  const note = element("p", "note");
  note.setAttribute("role", "status");
  panel.append(head, list, more, note);

  const ui = { armed: null, armTimer: null, sending: null, note: "", shown: new Map() };
  let last = { version: -1, feed: -1, alt: null };

  const paint = (force = false) => {
    const view = kit.state.view;
    if (view === null) return;
    const now = { version: kit.state.renderedVersion, feed: kit.feed.version, alt: kit.state.altDown };
    if (!force && now.version === last.version && now.feed === last.feed && now.alt === last.alt) return;
    last = now;
    const rows = ribbonRows(orderHistory(kit.feed.events), view);
    panel.hidden = rows.length === 0 || !isRailOpen("history");
    const shown = kit.state.altDown ? rows : rows.slice(0, REST_ORDERS);
    more.hidden = shown.length === rows.length;
    more.textContent = `+${rows.length - shown.length} earlier · Alt shows all`;
    const blocked = rows.some((row) => row.cancellable) ? historyBlock("cancel", kit.hold.on) : null;
    note.textContent = blocked ?? ui.note;
    const focused = list.contains(document.activeElement) ? document.activeElement.dataset.focus : null;
    list.replaceChildren(...shown.map((row) => rowItem(row)));
    if (focused) list.querySelector(`[data-focus="${CSS.escape(focused)}"]`)?.focus();
    for (const row of rows) ui.shown.set(row.id, row.steps.length + row.end.text);
  };

  const rowItem = (row) => {
    const li = element("li", `order ${row.end.kind}`);
    const go = element("button", "go");
    go.type = "button";
    go.dataset.focus = `go-${row.id}`;
    go.title = row.sentence;
    go.setAttribute("aria-label", `${row.label} at ${row.machine}: ${row.sentence}. ${row.steps.map((step) => step.text).join(", ")}, ${row.end.text}. Go to the machine.`);
    go.addEventListener("click", () => flyTo(kit, row.machine));
    const line = element("span", "line");
    line.append(element("b", undefined, row.label));
    for (const step of [...row.steps, row.end]) {
      line.append(element("span", "sep", "›"), element("span", `step ${step.kind}`, step.text));
    }
    go.append(line, element("span", "sentence", row.sentence));
    li.append(go);
    const before = ui.shown.get(row.id);
    if (before !== undefined && before !== row.steps.length + row.end.text) li.classList.add("moved");
    if (row.cancellable) li.append(cancelButton(row));
    return li;
  };

  const cancelButton = (row) => {
    const armed = ui.armed === row.id;
    const sending = ui.sending === row.id;
    const button = element("button", armed ? "cancel armed" : "cancel", sending ? "Cancelling" : armed ? `Cancel ${row.label}?` : "Cancel");
    button.type = "button";
    button.dataset.focus = `cancel-${row.id}`;
    const blocked = historyBlock("cancel", kit.hold.on);
    button.disabled = sending || blocked !== null || !kit.isLive();
    if (blocked !== null) button.title = blocked;
    button.setAttribute("aria-label", armed ? `Confirm: cancel ${row.label}` : `Cancel ${row.label}`);
    button.addEventListener("click", () => (armed ? sendCancel(row) : arm(row)));
    return button;
  };

  const arm = (row) => {
    clearTimeout(ui.armTimer);
    ui.armed = row.id;
    ui.note = "";
    ui.armTimer = setTimeout(() => {
      ui.armed = null;
      paint(true);
    }, ARM_MS);
    paint(true);
  };

  const sendCancel = async (row) => {
    if (historyBlock("cancel", kit.hold.on) !== null) return;
    clearTimeout(ui.armTimer);
    ui.armed = null;
    ui.sending = row.id;
    ui.note = "";
    paint(true);
    try {
      const res = await fetch("/api/commands", {
        method: "POST",
        headers: { Authorization: `Bearer ${kit.state.token}`, "Content-Type": "application/json" },
        body: JSON.stringify(cancelCommand(row.id)),
      });
      if (res.status === 401 || res.status === 403) {
        kit.forgetToken();
      } else if (!res.ok) {
        ui.note = cancelRefusal(res.status, await res.text());
      } else {
        ui.note = `${row.label} is cancelled.`;
        // A frozen page does not hear the stream, so it folds the cancel's own events here.
        if (kit.freeze) {
          for (const event of await res.json()) kit.state.app.ingest(JSON.stringify(event));
          kit.requestRender();
        }
      }
    } catch (err) {
      console.error(err);
      ui.note = "The server did not answer, so the cancel may not be recorded. Try again.";
    } finally {
      ui.sending = null;
      paint(true);
    }
  };

  kit.onPaint(paint);
  // The page's own key listener sets `altDown` first, since it was added first.
  for (const type of ["keydown", "keyup"]) {
    window.addEventListener(type, (event) => {
      if (event.key === "Alt") paint();
    });
  }
  window.addEventListener("blur", () => paint());
  goKeys.set("h", () => {
  openRailTab("history");
  list.querySelector("button")?.focus();
});
}
