// The plant view of each order that has receipts. The page fetches an order's plant view when
// its receipt count moves, and keeps the summary only once the wasm side has checked the view's
// counts. This module has no imports and touches no DOM, so Node can test it.

// Orders whose plant view is being fetched. A sync skips them, so a slow fetch is not repeated.
const inFlight = new Set();

// `fetchText` is the stream's `get`. A failed fetch or check is kept with the count it was for,
// so the order is not asked again until its count moves.
export function syncPlantViews(state, fetchText) {
  for (const { order, recorded } of state.view.receipts) {
    if (state.plantViews.get(order)?.recorded === recorded || inFlight.has(order)) continue;
    fetchPlantView(state, fetchText, order, recorded);
  }
}

async function fetchPlantView(state, fetchText, order, recorded) {
  inFlight.add(order);
  try {
    const text = await fetchText(`/api/orders/${order}/plant-view`);
    const summary = JSON.parse(state.app.plant_view_summary(text));
    state.plantViews.set(order, { recorded, summary, error: null });
  } catch (err) {
    const error = String(err?.message ?? err);
    console.error(`the plant view of ${order} was not kept: ${error}`);
    state.plantViews.set(order, { recorded, summary: null, error });
  } finally {
    inFlight.delete(order);
    // A count that moved while this fetch was in flight is fetched now, not at the next event.
    syncPlantViews(state, fetchText);
  }
}
