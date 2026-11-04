// The codebase scan, as the page follows it. Its frames come live from the stream, and the plant
// route says where a scan stands when the page loads. A page shows the scan while its first map
// runs: a status line says how far it is, and writes wait (see `guard.js`). A rescan of a map that
// has mapped runs quietly on the map on screen, and the page reads the plant again when it ends.
// Nothing here touches the page at import, so Node can test it.
import { setMappingBlock } from "./guard.js";

const ENDED = new Set(["done", "failed"]);

// The status line for a first map, or null when there is none to show. A failed map keeps its writes off.
export function mappingText(status) {
  if (status.stage === "failed") return "Mapping failed, so writes are off.";
  if (status.stage === "waiting") return "Waiting to map the codebase…";
  const { modules_done: done = 0, modules_total: total = 0 } = status;
  return total > 0 ? `Mapping the codebase… ${done}/${total} modules` : "Mapping the codebase…";
}

// The reason a write waits, while the first map runs or after it failed.
export function writeReason(status) {
  return status.stage === "failed" ? "The codebase did not map, so writes are off." : "Mapping the codebase first.";
}

// Reads one streamed frame that belongs to a scan or a transition touch, or null for any other
// frame. A scan frame carries `kind` ("status" or "progress") and its parsed `frame`. A touch has
// no more to read. `project` is the project a multi-project frame is for, or null.
export function scanFrameOf(text) {
  if (!text.includes('"scan_') && !text.includes("transition_touched")) return null;
  let frame;
  try {
    frame = JSON.parse(text);
  } catch {
    return null;
  }
  if (frame === null || typeof frame !== "object") return null;
  const project = typeof frame.project === "string" ? frame.project : null;
  if (frame.type === "transition_touched") return { kind: "touch", project, frame: null, json: text };
  if (frame.type === "scan_status") return { kind: "status", project, frame, json: text };
  if (frame.type === "scan_progress") return { kind: "progress", project, frame, json: text };
  return null;
}

// The page's view of the scan of the plant on screen. `app` is the renderer, `paint` shows the
// status line (null hides it), and `reduced` says whether motion is reduced.
export function scanView({ app, paint, reduced }) {
  // Whether the first map runs, or has failed: its status shows, and writes wait.
  let onMap = false;
  // Whether a scan is running, so its end reads the plant again.
  let running = false;
  let status = null;

  function show() {
    const shown = onMap && status !== null ? status : null;
    setMappingBlock(shown === null ? null : writeReason(shown));
    paint(shown);
  }

  return {
    // Folds the plant route's answer, which carries the scan's summary. Call it after every plant load.
    applyPlant(text) {
      const { scan = { stage: "done" } } = JSON.parse(text);
      running = !ENDED.has(scan.stage);
      onMap = running || scan.stage === "failed";
      status = scan;
      app.set_scan(JSON.stringify({ stage: scan.stage, filled: [] }));
      show();
    },

    // Folds one live scan frame. Returns "reload" when a scan that was running has ended, so the
    // caller reads the plant again. A touch changes nothing here.
    fold(scan) {
      if (scan.kind === "touch") return null;
      const { frame } = scan;
      if (scan.kind === "status") {
        app.set_scan(scan.json);
      } else if (frame.module !== undefined && frame.module !== null) {
        app.scan_fill(frame.module.name, reduced());
      }
      status = frame;
      if (!ENDED.has(frame.stage)) {
        running = true;
        show();
        return null;
      }
      const reload = running;
      running = false;
      onMap = onMap && frame.stage === "failed";
      show();
      return reload ? "reload" : null;
    },
  };
}

// Shows the status line, or hides it for null. The page has one element for it, and a page without
// it shows nothing.
export function paintMapping(status) {
  const el = document.getElementById("mapping");
  if (el === null) return;
  el.hidden = status === null;
  el.textContent = status === null ? "" : mappingText(status);
}
