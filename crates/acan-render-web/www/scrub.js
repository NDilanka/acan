// The log the page holds, as the watch panels read it: every event the wasm side has folded, in
// seq order. This file has no DOM at load, so Node can test it.

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
