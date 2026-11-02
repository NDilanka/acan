// The needs-you inbox: one count and one list of everything that waits on the human. That is
// each open question, each clash not yet settled, each scope lane that waits for a seal, each
// stopped lane, each order whose receipt has residue to rule on, and each advisory vision check
// that found something inconsistent or unclear. A click flies to the machine. The list logic has
// no DOM, so Node can test it. `installInbox` binds it to the page.
import { flyTo, goKeys } from "./lanes.js";

export const NOTIFY_KEY = "acan.notify";

const PLANT_VIEW_CHECK_MS = 2000;

// The order a kind sorts in: what blocks work first, then what only asks for a look.
const KIND_ORDER = ["clash", "question", "stopped", "seal", "residue", "vision"];

const DONE_PHASES = ["landed", "cancelled"];

// The newest advisory vision verdict of each task that found something to doubt, by task id.
// A later run of the same task that doubts nothing clears it.
export function visionDoubts(events) {
  const doubts = new Map();
  for (const { seq, at, event } of events) {
    if (event?.type !== "vision.checked") continue;
    const inconsistent = event.counts?.inconsistent ?? 0;
    const unclear = event.counts?.unclear ?? 0;
    if (inconsistent + unclear === 0) {
      doubts.delete(event.task);
    } else {
      doubts.set(event.task, { task: event.task, run: event.gate_run, inconsistent, unclear, seq, at });
    }
  }
  return doubts;
}

function plural(count, word) {
  return `${count} ${word}${count === 1 ? "" : "s"}`;
}

// The receipt items of one plant view entry, as the plate lists them.
function viewItems(entry) {
  return entry?.play?.items ?? entry?.items ?? [];
}

// Every item that waits on the human, sorted by kind and then id. Each has a `key` that names
// it across rebuilds, so a re-ingested log yields the same keys and nothing twice. `plantViews`
// is the page's map of plant views by order; `doubts` is `visionDoubts` of the log.
export function inboxItems(view, plantViews = new Map(), doubts = new Map()) {
  const orders = new Map(view.orders.map((order) => [order.id, order]));
  const tasks = new Map(view.tasks.map((task) => [task.id, task]));
  const live = (orderId) => !DONE_PHASES.includes(orders.get(orderId)?.phase);
  const machineOf = (orderId) => orders.get(orderId)?.machine ?? null;
  const items = new Map();
  const add = (item) => items.set(item.key, item);

  for (const clash of view.clashes) {
    if (clash.state !== "open" && clash.state !== "proposed") continue;
    const machine = tasks.get(clash.a)?.machine ?? tasks.get(clash.b)?.machine ?? null;
    const text = clash.state === "proposed"
      ? `${clash.label}: a settle was proposed and waits for yours`
      : `${clash.label}: two tasks overlap, so choose how to settle it`;
    add({ key: `clash:${clash.id}`, kind: "clash", id: clash.id, text, machine });
  }
  for (const question of view.questions) {
    add({ key: `question:${question.id}`, kind: "question", id: question.id,
      text: `q-${question.id} asks: ${question.text}`, machine: machineOf(question.order) });
  }
  for (const lane of view.lanes) {
    if (!live(lane.order)) continue;
    if (lane.stopped) {
      add({ key: `stopped:${lane.order}`, kind: "stopped", id: lane.order,
        text: `o-${lane.order} stopped because its repair budget ran out`, machine: machineOf(lane.order) });
    } else if (lane.state !== "confirmed") {
      add({ key: `seal:${lane.order}:${lane.rev}`, kind: "seal", id: lane.order,
        text: `o-${lane.order} scope rev ${lane.rev} waits for your seal`, machine: machineOf(lane.order) });
    }
  }
  for (const [order, entry] of plantViews) {
    const residue = viewItems(entry).filter((item) => item.state === "residue");
    const id = Number(String(order).replace(/^o-/, ""));
    if (residue.length === 0 || !live(id)) continue;
    add({ key: `residue:${order}`, kind: "residue", id,
      text: `${order} has ${plural(residue.length, "residue item")} to rule on`, machine: residue[0].machine_id ?? machineOf(id) });
  }
  for (const doubt of doubts.values()) {
    const task = tasks.get(doubt.task);
    if (task === undefined || !live(task.order) || task.state === "dropped") continue;
    const found = [doubt.inconsistent && `${doubt.inconsistent} inconsistent`, doubt.unclear && `${doubt.unclear} unclear`]
      .filter((part) => part).join(", ");
    add({ key: `vision:${doubt.task}:${doubt.run}`, kind: "vision", id: doubt.task,
      text: `t-${doubt.task} vision check found ${found}; it is advisory, so take a look`, machine: task.machine ?? null });
  }
  return [...items.values()].sort((a, b) => KIND_ORDER.indexOf(a.kind) - KIND_ORDER.indexOf(b.kind) || a.id - b.id);
}

// The items in `items` whose key `seen` does not hold.
export function freshItems(seen, items) {
  return items.filter((item) => !seen.has(item.key));
}

// The page title with the count in front, such as "(3) acan".
export function titleWith(base, count) {
  return count === 0 ? base : `(${count}) ${base}`;
}

// What a screen reader hears when the list changes.
export function announcement(fresh, count) {
  const total = count === 0 ? "Nothing waits on you." : `${plural(count, "thing")} ${count === 1 ? "waits" : "wait"} on you.`;
  if (fresh.length === 0) return total;
  const stop = /[.?!]$/.test(fresh[0].text) ? "" : ".";
  return `New: ${fresh[0].text}${stop}${fresh.length > 1 ? ` And ${fresh.length - 1} more.` : ""} ${total}`;
}

// A browser notification goes out only for a live page whose tab is hidden, and only after the
// user asked for them and the browser allowed them.
export function shouldNotify({ hidden, optedIn, permission, live }) {
  return hidden && optedIn && permission === "granted" && live;
}

function element(tag, className, text) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  if (text !== undefined) el.textContent = text;
  return el;
}

const KIND_WORDS = {
  clash: "clash",
  question: "question",
  stopped: "stopped",
  seal: "seal",
  residue: "residue",
  vision: "vision",
};

// Binds the inbox to `#inbox`, and the count to the page title.
export function installInbox(kit) {
  const panel = document.getElementById("inbox");
  const head = element("h3");
  const badge = element("span", "badge", "0");
  badge.setAttribute("aria-hidden", "true");
  const title = element("span", undefined, "Needs you ");
  title.append(badge);
  const keys = element("span", "keys");
  keys.innerHTML = "<kbd>G</kbd> <kbd>I</kbd>";
  head.append(title, keys);
  const list = element("ol");
  const empty = element("p", "empty", "Nothing waits on you.");
  const status = element("p", "sr-only");
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "polite");
  const optIn = element("button", "opt-in", "Notify me while this tab is hidden");
  optIn.type = "button";
  panel.append(head, list, empty, optIn, status);

  const baseTitle = document.title;
  const canNotify = typeof Notification !== "undefined";
  const optedIn = () => localStorage.getItem(NOTIFY_KEY) === "1";
  const paintOptIn = () => {
    optIn.hidden = !canNotify || optedIn() || Notification.permission === "denied";
  };
  optIn.addEventListener("click", async () => {
    const permission = await Notification.requestPermission();
    if (permission === "granted") localStorage.setItem(NOTIFY_KEY, "1");
    paintOptIn();
  });
  paintOptIn();

  let seen = null;
  let shownCount = -1;
  let last = { version: -1, feed: -1, views: -1 };
  const paint = () => {
    const view = kit.state.view;
    if (view === null) return;
    const now = { version: kit.state.renderedVersion, feed: kit.feed.version, views: viewStamp(kit.state.plantViews) };
    if (now.version === last.version && now.feed === last.feed && now.views === last.views) return;
    last = now;
    panel.hidden = false;
    const items = inboxItems(view, kit.state.plantViews, visionDoubts(kit.feed.events));
    // What is new is judged against the last live list only, so rewinding the map and coming back
    // announces nothing twice.
    const live = kit.isLive();
    const fresh = seen === null || !live ? [] : freshItems(seen, items);
    // The first list is on screen already, so only a later change is announced.
    const changed = shownCount !== -1 && (fresh.length > 0 || items.length !== shownCount);
    shownCount = items.length;
    if (live || seen === null) seen = new Set(items.map((item) => item.key));

    badge.textContent = String(items.length);
    badge.classList.toggle("some", items.length > 0);
    panel.setAttribute("aria-label", `needs you: ${items.length}`);
    document.title = titleWith(baseTitle, items.length);
    empty.hidden = items.length > 0;
    list.hidden = items.length === 0;
    const focused = list.contains(document.activeElement) ? document.activeElement.dataset.key : null;
    list.replaceChildren(...items.map((item) => {
      const li = element("li");
      const button = element("button", `item ${item.kind}`);
      button.type = "button";
      button.dataset.key = item.key;
      button.append(element("span", "kind", KIND_WORDS[item.kind]), element("span", "text", item.text));
      button.setAttribute("aria-label", item.machine ? `${item.text}. Go to ${item.machine}.` : item.text);
      if (item.machine === null) button.disabled = true;
      button.addEventListener("click", () => flyTo(kit, item.machine));
      li.append(button);
      if (fresh.includes(item)) li.classList.add("moved");
      return li;
    }));
    if (focused) list.querySelector(`button[data-key="${CSS.escape(focused)}"]`)?.focus();
    if (fresh.length > 0) {
      badge.classList.remove("bump");
      void badge.offsetWidth;
      badge.classList.add("bump");
    }
    if (changed) status.textContent = announcement(fresh, items.length);
    const notify = shouldNotify({
      hidden: document.hidden,
      optedIn: optedIn(),
      permission: canNotify ? Notification.permission : "default",
      live,
    });
    if (notify) {
      // The tag is the item's key, so a browser shows one notification per item at most.
      for (const item of fresh) {
        try {
          new Notification(`acan: ${KIND_WORDS[item.kind]}`, { body: item.text, tag: item.key });
        } catch (err) {
          console.error("the browser refused the notification", err);
        }
      }
    }
  };

  kit.onPaint(paint);
  // A plant view is fetched after the view that asked for it, so its residue shows on this check
  // rather than at the next event.
  setInterval(paint, PLANT_VIEW_CHECK_MS);
  goKeys.set("i", () => list.querySelector("button:not(:disabled)")?.focus());
}

// A number that moves when a plant view arrives or its receipt count moves.
function viewStamp(plantViews) {
  let sum = plantViews.size;
  for (const entry of plantViews.values()) sum += (entry.recorded ?? 0) + viewItems(entry).length;
  return sum;
}
