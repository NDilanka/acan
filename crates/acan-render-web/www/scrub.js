// The log the page holds, and the timeline scrubber that rewinds the map along it. The feed is
// every event the wasm side has folded, in seq order. The hold keeps the live log while the map
// shows an earlier point, so no event is lost while the user looks back. The logic has no DOM
// at load, so Node can test it. `installScrub` binds the scrubber to the page.
import { goKeys } from "./lanes.js";

// A drag rebuilds the map once it pauses this long, or at once on release.
export const SETTLE_MS = 150;

// The events the app holds. `load` replaces them, as `load_log` does, and `add` appends one
// event, as `ingest` does. An event the feed already holds is passed over, as the mirror passes
// over a replayed event, so a resent event is never counted twice. `version` moves on every
// change, so a panel can skip a rebuild when nothing moved.
export function createFeed() {
  return {
    events: [],
    next: 0,
    version: 0,
    load(events) {
      this.events = events.slice();
      this.next = this.events.length === 0 ? 0 : this.events.at(-1).seq + 1;
      this.version += 1;
    },
    add(event) {
      if (event.seq < this.next) return false;
      this.events.push(event);
      this.next = event.seq + 1;
      this.version += 1;
      return true;
    },
    // When the newest event was accepted, in unix milliseconds, or null for an empty feed.
    lastAt() {
      return this.events.length === 0 ? null : this.events.at(-1).at;
    },
  };
}

// The events before seq `seq`, as `?upto=` folds them.
export function eventsBefore(events, seq) {
  return events.filter((event) => event.seq < seq);
}

// The live log while the map shows the past. `start` takes the log the app holds; `push` adds
// an event the stream sends meanwhile, once; `replace` takes a log reloaded meanwhile; `end`
// gives the whole live log back to be folded. `waiting` counts the events that came in while
// the map was held.
export function createHold() {
  return {
    on: false,
    live: [],
    waiting: 0,
    start(events) {
      this.on = true;
      this.live = events.slice();
      this.waiting = 0;
    },
    next() {
      return this.live.length === 0 ? 0 : this.live.at(-1).seq + 1;
    },
    push(event) {
      if (event.seq < this.next()) return false;
      this.live.push(event);
      this.waiting += 1;
      return true;
    },
    replace(events) {
      const before = this.next();
      this.live = events.slice();
      this.waiting += Math.max(0, this.next() - before);
    },
    end() {
      const live = this.live;
      this.on = false;
      this.live = [];
      this.waiting = 0;
      return live;
    },
  };
}

// The scrubber's range over `events`: from the first event after the world opened, since the
// log must open with it, to the newest. Null when there is nothing to rewind.
export function scrubRange(events) {
  if (events.length < 2) return null;
  return { min: events[1].seq, max: events.at(-1).seq };
}

// The seq kept inside the range.
export function clampSeq(seq, range) {
  return Math.min(Math.max(Math.round(seq), range.min), range.max);
}

// The events the map shows at seq `seq`: every event up to and including it.
export function eventsAt(events, seq) {
  return eventsBefore(events, seq + 1);
}

// The words the scrubber and the link show for the point the map is at.
export function holdLabel(seq) {
  return `HISTORY · seq ${seq}`;
}

export function timeWords(at, timeZone) {
  return new Date(at).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", second: "2-digit", timeZone });
}

// Runs `fn` once input has paused for `ms`, or at once on `flush`. A flush cancels the wait, so
// a release right after a pause rebuilds once, not twice.
export function createSettle(fn, ms = SETTLE_MS, timers = globalThis) {
  let timer = null;
  let pending;
  return {
    poke(value) {
      pending = value;
      timers.clearTimeout(timer);
      timer = timers.setTimeout(() => {
        timer = null;
        fn(pending);
      }, ms);
    },
    flush(value) {
      timers.clearTimeout(timer);
      timer = null;
      fn(value);
    },
    cancel() {
      timers.clearTimeout(timer);
      timer = null;
    },
  };
}

function element(tag, className, text) {
  const el = document.createElement(tag);
  if (className) el.className = className;
  if (text !== undefined) el.textContent = text;
  return el;
}

// Binds the scrubber to `#scrub`. Dragging back holds the stream and rebuilds the map at the
// point the drag settles on. Esc, the Live button, or a release at the newest event folds the
// whole live log again, events that came in meanwhile included.
export function installScrub(kit) {
  const { feed, hold } = kit;
  const panel = document.getElementById("scrub");
  if (kit.replay) return;
  const head = element("h3");
  const keys = element("span", "keys");
  keys.innerHTML = "<kbd>G</kbd> <kbd>T</kbd> · <kbd>Esc</kbd> live";
  head.append(element("span", undefined, "Timeline"), keys);
  const range = element("input");
  range.type = "range";
  range.step = "1";
  range.setAttribute("aria-label", "Rewind the map to an event");
  const row = element("p", "row");
  const label = element("span", "where");
  label.setAttribute("aria-live", "polite");
  const live = element("button", "live", "Live");
  live.type = "button";
  live.hidden = true;
  row.append(label, live);
  panel.append(head, range, row);

  const timing = { rebuilds: 0, lastMs: 0 };
  kit.scrubTiming = timing;

  const showAt = (seq) => {
    const event = hold.live.find((e) => e.seq === seq);
    const when = event ? ` · ${timeWords(event.at)}` : "";
    const waiting = hold.waiting > 0 ? ` · ${hold.waiting} new` : "";
    label.textContent = `${holdLabel(seq)}${when}${waiting}`;
    range.setAttribute("aria-valuetext", `seq ${seq}${when}`);
    kit.showHold(holdLabel(seq));
  };

  const rebuild = (seq) => {
    if (!hold.on) return;
    const started = performance.now();
    kit.loadEvents(eventsAt(hold.live, seq));
    timing.lastMs = performance.now() - started;
    timing.rebuilds += 1;
    panel.dataset.rebuildMs = timing.lastMs.toFixed(1);
    kit.requestRender();
  };
  const settle = createSettle(rebuild);

  const resume = () => {
    if (!hold.on) return;
    settle.cancel();
    kit.loadEvents(hold.end());
    document.body.classList.remove("history");
    kit.showHold(null);
    paintLive();
    kit.requestRender();
  };

  const begin = () => {
    if (hold.on) return true;
    if (scrubRange(feed.events) === null) return false;
    hold.start(feed.events);
    document.body.classList.add("history");
    live.hidden = false;
    panel.classList.add("held");
    return true;
  };

  const paintLive = () => {
    const extent = scrubRange(hold.on ? hold.live : feed.events);
    panel.hidden = extent === null;
    if (extent === null) return;
    range.min = String(extent.min);
    range.max = String(extent.max);
    if (hold.on) {
      showAt(clampSeq(Number(range.value), extent));
      return;
    }
    range.value = String(extent.max);
    live.hidden = true;
    panel.classList.remove("held");
    label.textContent = `Live · seq ${extent.max}`;
    range.setAttribute("aria-valuetext", `live, seq ${extent.max}`);
  };

  // At the newest event the map is live again, so a release there resumes.
  const pick = (final) => {
    if (!begin()) return;
    const extent = scrubRange(hold.live);
    const seq = clampSeq(Number(range.value), extent);
    if (final && seq === extent.max) {
      resume();
      return;
    }
    showAt(seq);
    if (final) settle.flush(seq);
    else settle.poke(seq);
  };
  range.addEventListener("input", () => pick(false));
  range.addEventListener("change", () => pick(true));
  range.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      resume();
      range.blur();
    }
  });
  live.addEventListener("click", resume);

  kit.onPaint(() => {
    if (!hold.on) paintLive();
  });
  kit.onHoldEvent(() => paintLive());
  // Esc goes live once no panel is left to close; keys.js owns the order and calls this.
  kit.goLive = resume;
  goKeys.set("t", () => range.focus());
}
