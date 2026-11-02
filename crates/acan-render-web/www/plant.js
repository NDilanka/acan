// The plant view of each order that has receipts. The page fetches an order's plant view when
// its receipt count moves, and keeps the summary only once the wasm side has checked the view's
// counts. It also keeps the play the view admits, which the receipt plate shows. This module has
// no imports and touches no DOM, so Node can test it. The caller passes `onChange` to repaint.

// Orders whose plant view is being fetched. A sync skips them, so a slow fetch is not repeated.
const inFlight = new Set();

// `fetchText` is the stream's `get`. A failed fetch or check is kept with the count it was for,
// so the order is not asked again until its count moves.
export function syncPlantViews(state, fetchText, onChange = () => {}) {
  for (const { order, recorded } of state.view.receipts) {
    if (state.plantViews.get(order)?.recorded === recorded || inFlight.has(order)) continue;
    fetchPlantView(state, fetchText, order, recorded, onChange);
  }
}

async function fetchPlantView(state, fetchText, order, recorded, onChange) {
  inFlight.add(order);
  try {
    const text = await fetchText(`/api/orders/${order}/plant-view`);
    const summary = JSON.parse(state.app.plant_view_summary(text));
    const { play, playError, items } = playOf(state.app, text);
    state.plantViews.set(order, { recorded, summary, error: null, play, playError, items });
    onChange();
  } catch (err) {
    const error = String(err?.message ?? err);
    console.error(`the plant view of ${order} was not kept: ${error}`);
    state.plantViews.set(order, { recorded, summary: null, error, play: null, playError: null, items: [] });
    onChange();
  } finally {
    inFlight.delete(order);
    // A count that moved while this fetch was in flight is fetched now, not at the next event.
    syncPlantViews(state, fetchText, onChange);
  }
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
