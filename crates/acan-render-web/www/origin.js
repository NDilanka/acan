// The page's reads of the origin on each paint: each receipt's plant view and each lane's Seal
// route. A replay has no origin to ask, so it makes neither read. It marks each receipt as not in
// the replay instead, and the Seal block says what the replay lacks (see slip.js). This module
// touches no DOM, so Node can test it. `repaints` names the plate and slip repaint functions.
import { markReplayReceipts, syncPlantViews } from "./plant.js";
import { syncSeals } from "./seal.js";

export function syncOrigin(state, fetchText, repaints, replay) {
  if (replay) {
    markReplayReceipts(state);
    return;
  }
  syncPlantViews(state, fetchText, repaints.plate);
  syncSeals(state, fetchText, repaints.slip);
}
