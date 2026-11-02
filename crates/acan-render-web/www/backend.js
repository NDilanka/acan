// Which renderer backend the page tries, in order, and the walk through them. A canvas keeps
// the first context type it hands out, so a failed WebGPU attempt leaves it holding a "webgpu"
// context and WebGL2 can no longer get one. Each retry therefore draws on a fresh copy of the
// canvas. This module touches no DOM at load time, so node can test it.

export const BACKENDS = ["webgpu", "webgl"];

export const NO_BACKEND_LINE = "This browser can't draw the map: it needs WebGPU or WebGL2.";

// `override` is the `?backend=` value, or null or "" when the page should choose. A forced
// WebGPU still falls back to WebGL, as it always has; a forced WebGL tries WebGL alone.
export function backendPlan(override, hasWebGpu) {
  if (override) {
    if (!BACKENDS.includes(override)) {
      throw new Error(`unknown backend "${override}": use webgpu or webgl`);
    }
    return override === "webgpu" ? ["webgpu", "webgl"] : ["webgl"];
  }
  return hasWebGpu ? ["webgpu", "webgl"] : ["webgl"];
}

// Tries each backend in `plan` until one builds. `create(canvas, backend)` builds the renderer
// and rejects on failure; `freshCanvas(old)` returns a clean canvas in place of `old`, and is
// called before every attempt after the first. `onFail(backend, err)` hears each failure.
// Resolves with the app, the backend that built it, and the canvas it draws on. When every
// backend fails it rejects with NO_BACKEND_LINE, and the failures sit on `causes`.
export async function createOnFirst(plan, canvas, { create, freshCanvas, onFail = () => {} }) {
  const causes = [];
  let target = canvas;
  for (const [index, backend] of plan.entries()) {
    if (index > 0) target = freshCanvas(target);
    try {
      const app = await create(target, backend);
      return { app, backend, canvas: target };
    } catch (err) {
      causes.push({ backend, err });
      onFail(backend, err);
    }
  }
  const error = new Error(NO_BACKEND_LINE);
  error.causes = causes;
  throw error;
}

// Puts a copy of `old` in its place in the page and returns it. The copy keeps the id, the
// classes and the attributes, including the drawing size; it has no context and no listeners.
export function replaceCanvas(old) {
  const next = old.cloneNode(false);
  old.replaceWith(next);
  return next;
}
