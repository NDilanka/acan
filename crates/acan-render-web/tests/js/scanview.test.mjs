// Node tests for the codebase scan the page follows. Run with:
//   node --test crates/acan-render-web/tests/js/scanview.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { historyBlock, setMappingBlock } from "../../www/guard.js";
import { mappingText, scanFrameOf, scanView, writeReason } from "../../www/scanview.js";

// A renderer that records what the page asks of it.
function fakeApp() {
  const calls = [];
  return {
    calls,
    set_scan: (json) => calls.push(["set_scan", json]),
    scan_fill: (name, reduced) => {
      calls.push(["scan_fill", name, reduced]);
      return true;
    },
  };
}

// A fresh view over a fake renderer, with the page's status line and motion setting stubbed.
function fresh({ reduced = false } = {}) {
  setMappingBlock(null);
  const app = fakeApp();
  const shown = [];
  const view = scanView({ app, paint: (status) => shown.push(status), reduced: () => reduced });
  return { app, shown, view };
}

const plant = (stage) => JSON.stringify({ machines: [], scan: { stage, modules_done: 0, modules_total: 3 } });

const status = (stage, filled = []) =>
  JSON.stringify({ type: "scan_status", stage, modules_done: 1, modules_total: 3, filled });

const progress = (stage, module) =>
  JSON.stringify({ type: "scan_progress", stage, modules_done: 1, modules_total: 3, ...(module ? { module } : {}) });

const core = { machine: 0, name: "core", files: 2, symbols: 9, exported: 4, imports: 1, exports: [], belts: [] };

test("the status line counts the modules and says when there are none yet", () => {
  assert.equal(mappingText({ stage: "fill", modules_done: 1, modules_total: 3 }), "Mapping the codebase… 1/3 modules");
  assert.equal(mappingText({ stage: "skeleton", modules_done: 0, modules_total: 0 }), "Mapping the codebase…");
  assert.equal(mappingText({ stage: "failed", modules_done: 1, modules_total: 3 }), "Mapping failed, so writes are off.");
});

test("a write waits for the first map, and a failed map says why", () => {
  assert.equal(writeReason({ stage: "states" }), "Mapping the codebase first.");
  assert.equal(writeReason({ stage: "failed" }), "The codebase did not map, so writes are off.");
});

test("a scan frame or a touch is read, and any other frame is passed over", () => {
  assert.equal(scanFrameOf(status("fill")).kind, "status");
  assert.equal(scanFrameOf(progress("fill", core)).kind, "progress");
  assert.equal(scanFrameOf(JSON.stringify({ type: "transition_touched", agent: "a" })).kind, "touch");
  assert.equal(scanFrameOf(JSON.stringify({ project: "shop", type: "scan_progress", stage: "done" })).project, "shop");
  assert.equal(scanFrameOf(progress("fill", core)).project, null, "a one-plant frame names no project");
  assert.equal(scanFrameOf(JSON.stringify({ seq: 4, event: { type: "landed", path: "scan_a.rs" } })), null);
  assert.equal(scanFrameOf('{"type":"scan_progress"'), null, "a frame that does not parse is passed over");
});

test("a first map shows its outlines and blocks writes until it ends", () => {
  const { app, shown, view } = fresh();
  view.applyPlant(plant("skeleton"));
  assert.deepEqual(app.calls, [["set_scan", '{"stage":"skeleton","filled":[]}']]);
  assert.equal(shown.at(-1).stage, "skeleton");
  assert.equal(historyBlock("send", false), "Mapping the codebase first.");
  assert.equal(historyBlock("cancel", true), "Mapping the codebase first.");

  assert.equal(view.fold(scanFrameOf(progress("fill", core))), null);
  assert.deepEqual(app.calls.at(-1), ["scan_fill", "core", false]);
  assert.equal(shown.at(-1).modules_done, 1);
});

test("a fill runs as reduced motion asks it to", () => {
  const { app, view } = fresh({ reduced: true });
  view.applyPlant(plant("skeleton"));
  view.fold(scanFrameOf(progress("fill", core)));
  assert.deepEqual(app.calls.at(-1), ["scan_fill", "core", true]);
});

test("the scan's end reads the plant again and lifts the block", () => {
  const { shown, view } = fresh();
  view.applyPlant(plant("skeleton"));
  assert.equal(view.fold(scanFrameOf(progress("done"))), "reload");
  assert.equal(shown.at(-1), null, "the status line hides");
  assert.equal(historyBlock("send", false), null, "writes open");
});

test("a page that loads after its scan has ended reads no more than the plant", () => {
  const { view } = fresh();
  view.applyPlant(plant("done"));
  assert.equal(view.fold(scanFrameOf(status("done"))), null, "a connect to an ended scan does not reload");
});

test("a scan that ends before the stream connects is read again", () => {
  const { view } = fresh();
  view.applyPlant(plant("skeleton"));
  assert.equal(view.fold(scanFrameOf(status("done"))), "reload");
});

test("a rescan of a mapped codebase runs quietly and writes stay open", () => {
  const { app, shown, view } = fresh();
  view.applyPlant(plant("done"));
  assert.equal(view.fold(scanFrameOf(status("skeleton"))), null);
  assert.equal(shown.at(-1), null, "no status line on a mapped page");
  assert.equal(historyBlock("send", false), null, "writes stay open during a rescan");
  view.fold(scanFrameOf(progress("fill", core)));
  assert.deepEqual(app.calls.at(-1), ["scan_fill", "core", false], "the renderer builds it at once");
  assert.equal(view.fold(scanFrameOf(progress("done"))), "reload", "the plant is read again");
});

test("a connect's status carries the modules that already filled", () => {
  const { app, view } = fresh();
  view.applyPlant(plant("fill"));
  const text = status("fill", [core]);
  view.fold(scanFrameOf(text));
  assert.deepEqual(app.calls.at(-1), ["set_scan", text]);
});

test("a failed first map reads the plant again and keeps writes off", () => {
  const { shown, view } = fresh();
  view.applyPlant(plant("skeleton"));
  assert.equal(view.fold(scanFrameOf(progress("failed"))), "reload", "the plant says the scan failed");
  assert.equal(shown.at(-1).stage, "failed");
  view.applyPlant(plant("failed"));
  assert.equal(shown.at(-1).stage, "failed", "the plant's own failed scan keeps the line");
  assert.equal(historyBlock("send", false), "The codebase did not map, so writes are off.");
});

test("a touch leaves the scan and the renderer alone", () => {
  const { app, shown, view } = fresh();
  view.applyPlant(plant("skeleton"));
  const before = shown.length;
  assert.equal(view.fold(scanFrameOf(JSON.stringify({ type: "transition_touched" }))), null);
  assert.equal(app.calls.length, 1, "only the plant's own status was set");
  assert.equal(shown.length, before);
});
