// Node tests for the backend choice: which backends the page tries, in what order, and that a
// retry after a failed backend draws on a fresh canvas. Run with:
//   node --test crates/acan-render-web/tests/js/backend.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { NO_BACKEND_LINE, backendPlan, createOnFirst, replaceCanvas } from "../../www/backend.js";

test("with no override the page tries WebGPU first when the browser has it", () => {
  assert.deepEqual(backendPlan(null, true), ["webgpu", "webgl"]);
  assert.deepEqual(backendPlan("", true), ["webgpu", "webgl"]);
  assert.deepEqual(backendPlan(null, false), ["webgl"]);
});

test("?backend= picks the backend whether or not the browser has WebGPU", () => {
  assert.deepEqual(backendPlan("webgl", true), ["webgl"]);
  assert.deepEqual(backendPlan("webgpu", false), ["webgpu", "webgl"]);
  assert.throws(() => backendPlan("vulkan", true), /unknown backend "vulkan"/);
});

// A fake canvas that remembers which context type it handed out, as a browser canvas does.
function fakeCanvas(name) {
  return { name, context: null };
}

function fakeCreate(working) {
  const calls = [];
  const create = async (canvas, backend) => {
    calls.push([canvas.name, backend]);
    if (canvas.context !== null && canvas.context !== backend) {
      throw new Error("webgl2 not available or canvas already in use");
    }
    canvas.context = backend;
    if (!working.includes(backend)) throw new Error(`${backend}: no adapter`);
    return { backend };
  };
  return { create, calls };
}

test("a working first backend builds on the page's canvas", async () => {
  const { create, calls } = fakeCreate(["webgpu", "webgl"]);
  const canvas = fakeCanvas("page");
  const made = await createOnFirst(["webgpu", "webgl"], canvas, {
    create,
    freshCanvas: () => assert.fail("no fresh canvas is needed"),
  });
  assert.equal(made.backend, "webgpu");
  assert.equal(made.canvas, canvas);
  assert.deepEqual(calls, [["page", "webgpu"]]);
});

test("a failed WebGPU falls back to WebGL on a fresh canvas", async () => {
  const { create, calls } = fakeCreate(["webgl"]);
  const failed = [];
  let copies = 0;
  const made = await createOnFirst(["webgpu", "webgl"], fakeCanvas("page"), {
    create,
    freshCanvas: () => fakeCanvas(`copy ${++copies}`),
    onFail: (backend, err) => failed.push([backend, err.message]),
  });
  assert.equal(made.backend, "webgl");
  assert.equal(made.canvas.name, "copy 1");
  assert.deepEqual(made.app, { backend: "webgl" });
  assert.deepEqual(calls, [["page", "webgpu"], ["copy 1", "webgl"]]);
  assert.deepEqual(failed, [["webgpu", "webgpu: no adapter"]]);
});

test("reusing the claimed canvas is what broke the fallback", async () => {
  const { create } = fakeCreate(["webgl"]);
  await assert.rejects(
    createOnFirst(["webgpu", "webgl"], fakeCanvas("page"), { create, freshCanvas: (old) => old }),
    (err) => err.causes[1].err.message === "webgl2 not available or canvas already in use",
  );
});

test("when every backend fails the page gets one plain sentence", async () => {
  const { create } = fakeCreate([]);
  await assert.rejects(
    createOnFirst(["webgpu", "webgl"], fakeCanvas("page"), { create, freshCanvas: () => fakeCanvas("copy") }),
    (err) => {
      assert.equal(err.message, NO_BACKEND_LINE);
      assert.deepEqual(err.causes.map((cause) => cause.backend), ["webgpu", "webgl"]);
      return true;
    },
  );
});

test("a forced WebGL that fails makes no copy of the canvas", async () => {
  const { create, calls } = fakeCreate([]);
  await assert.rejects(
    createOnFirst(["webgl"], fakeCanvas("page"), {
      create,
      freshCanvas: () => assert.fail("no fresh canvas for a single backend"),
    }),
    { message: NO_BACKEND_LINE },
  );
  assert.deepEqual(calls, [["page", "webgl"]]);
});

test("replaceCanvas swaps a shallow copy into the old canvas's place", () => {
  const copy = { id: "plant" };
  const old = {
    id: "plant",
    cloneNode(deep) {
      assert.equal(deep, false);
      return copy;
    },
    replaceWith(next) {
      this.replacedBy = next;
    },
  };
  assert.equal(replaceCanvas(old), copy);
  assert.equal(old.replacedBy, copy);
});
