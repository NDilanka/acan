// The plant view of each order that has receipts. The page fetches an order's plant view when
// its receipt count moves, and keeps the summary only once the wasm side has checked the view's
// counts. It also keeps the play the view admits, which the receipt plate shows. While a view
// shows the deterministic order, the director's answer may still arrive, so the page asks again
// for a few minutes. This module has no imports and touches no DOM, so Node can test it. The
// caller passes `onChange` to repaint.

// Orders whose plant view is being fetched. A sync skips them, so a slow fetch is not repeated.
const inFlight = new Set();

// While a view shows the deterministic order, the page asks again every RECHECK_EVERY_MS. The
// checks stop RECHECK_FOR_MS after the first view that showed the deterministic order for the
// receipt's count, and sooner when the director's tour arrives or the view cannot be read.
export const RECHECK_EVERY_MS = 10_000;
export const RECHECK_FOR_MS = 180_000;

// The browser's timer and clock. A test passes its own.
const BROWSER_TIMERS = { schedule: (fn, wait) => setTimeout(fn, wait), now: () => Date.now() };

// `fetchText` is the stream's `get`. A failed fetch or check is kept with the count it was for,
// so the order is not asked again until its count moves. `timers` sets the recheck's timer and
// reads the clock.
export function syncPlantViews(state, fetchText, onChange = () => {}, timers = BROWSER_TIMERS) {
  for (const { order, recorded } of state.view.receipts) {
    if (state.plantViews.get(order)?.recorded === recorded || inFlight.has(order)) continue;
    fetchPlantView(state, fetchText, order, recorded, onChange, timers);
  }
}

async function fetchPlantView(state, fetchText, order, recorded, onChange, timers) {
  inFlight.add(order);
  const prior = state.plantViews.get(order);
  // When the window opened by the first deterministic view for this count, if one has opened.
  const opened = prior?.recorded === recorded ? prior.since : undefined;
  try {
    const text = await fetchText(`/api/orders/${order}/plant-view`);
    const summary = JSON.parse(state.app.plant_view_summary(text));
    const { play, playError, items } = playOf(state.app, text);
    const since = play?.mode === "deterministic" ? (opened ?? timers.now()) : null;
    const entry = { recorded, summary, error: null, play, playError, items, since };
    state.plantViews.set(order, entry);
    if (changed(prior, entry)) onChange();
    if (since !== null && timers.now() - since < RECHECK_FOR_MS) {
      timers.schedule(
        () => recheck(state, fetchText, order, recorded, since, onChange, timers),
        RECHECK_EVERY_MS,
      );
    }
  } catch (err) {
    const error = String(err?.message ?? err);
    console.error(`the plant view of ${order} was not kept: ${error}`);
    const entry = { recorded, summary: null, error, play: null, playError: null, items: [], since: null };
    state.plantViews.set(order, entry);
    onChange();
  } finally {
    inFlight.delete(order);
    // A count that moved while this fetch was in flight is fetched now, not at the next event.
    syncPlantViews(state, fetchText, onChange, timers);
  }
}

// Asks for the view again while the window it was shown in is still open. A count that moved, a
// window that was replaced, or a fetch still in flight is left to that fetch.
function recheck(state, fetchText, order, recorded, since, onChange, timers) {
  const entry = state.plantViews.get(order);
  if (entry?.recorded !== recorded || entry.since !== since || inFlight.has(order)) return;
  fetchPlantView(state, fetchText, order, recorded, onChange, timers);
}

// Whether the page must repaint for `next`: a new count, or a changed error, summary or play. A
// check that finds the same view repaints nothing, so the plate keeps its rows as they are.
function changed(prior, next) {
  if (prior?.recorded !== next.recorded) return true;
  const shown = (entry) =>
    JSON.stringify([entry.error, entry.playError, entry.summary, entry.play, entry.items]);
  return shown(prior) !== shown(next);
}

// What the page plays over one view: the admitted play, or why none plays. The plain card
// still shows, so a refused tour is never a missing receipt: without a play, its items come from
// the view itself, with no `must reach` mark, which only the admitted play sets.
export function playOf(app, text) {
  try {
    return { play: JSON.parse(app.tour_load(text)), playError: null, items: [] };
  } catch (err) {
    const playError = String(err?.message ?? err);
    console.error(`the tour of this view cannot be played: ${playError}`);
    return { play: null, playError, items: viewItems(text) };
  }
}

// The items of a view, as the plate lists them without a play. Empty when the text is not a view.
function viewItems(text) {
  try {
    return JSON.parse(text).items.map(({ item_id, machine_id, class: itemClass, state }) => ({
      item_id,
      machine_id,
      class: itemClass,
      state,
    }));
  } catch {
    return [];
  }
}
