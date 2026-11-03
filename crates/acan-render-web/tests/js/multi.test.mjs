// The canvas of several projects: the route prefix, the project list, the packing, the stream's
// frames and cursors, the merged rail, the target choice, and `go <project>`. Run with:
//   node --test crates/acan-render-web/tests/js/multi.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  PACK_GAP,
  PLACEHOLDER,
  STASHED,
  apiPath,
  canvasState,
  chooseTarget,
  closeAction,
  countsByProject,
  entityKey,
  freshStash,
  pack,
  placesFrom,
  projectList,
  readFrame,
  resumeStep,
  stepCursor,
  subscription,
  swapState,
  target,
  worldBounds,
} from "../../www/projects.js";
import { inboxItems, mergeInbox } from "../../www/inbox.js";
import { projectMatches, search } from "../../www/command.js";

test("a path without a project is unchanged, and with one it takes the project's prefix", () => {
  assert.equal(apiPath(null, "/api/commands"), "/api/commands");
  assert.equal(apiPath("", "/api/plant"), "/api/plant");
  assert.equal(apiPath("shop", "/api/commands"), "/api/p/shop/commands");
  assert.equal(apiPath("shop", "/api/events?from=0"), "/api/p/shop/events?from=0");
  assert.equal(apiPath("shop", "/api/orders/o-3/scope-draft"), "/api/p/shop/orders/o-3/scope-draft");
  assert.equal(apiPath("a b", "/api/usage"), "/api/p/a%20b/usage");
  assert.equal(apiPath("shop", "/version"), "/version", "a route outside /api/ is the process's");
});

test("the calls go to the project on screen only on a canvas of several", () => {
  const saved = { ...canvasState };
  try {
    Object.assign(canvasState, { multi: false, active: "acan" });
    assert.equal(target(), null);
    Object.assign(canvasState, { multi: true, active: "shop" });
    assert.equal(target(), "shop");
  } finally {
    Object.assign(canvasState, saved);
  }
});

test("only a list of two or more projects turns the canvas on", () => {
  const two = [
    { id: "acan", name: "Acan", default: true, status: "ready", head: { seq: 412, commit: "9f3c" }, needs_you: { total: 3 } },
    { id: "shop", name: "", status: "degraded", head: { seq: 0, commit: null } },
  ];
  assert.equal(projectList(404, { error: "not_found" }), null, "an older origin keeps one plant");
  assert.equal(projectList(200, [two[0]]), null, "one project keeps one plant");
  assert.equal(projectList(200, { projects: two }), null);
  assert.deepEqual(projectList(200, two), [
    { id: "acan", name: "Acan", default: true, status: "ready", head: 412 },
    { id: "shop", name: "shop", default: false, status: "degraded", head: 0 },
  ]);
  assert.equal(projectList(200, [two[0], { name: "no id" }]), null, "an entry without an id is dropped");
});

test("plants pack left to right in list order, one gap apart, tops in line", () => {
  const places = pack([
    { id: "a", bounds: [0, 0, 400, 300] },
    { id: "b", bounds: [10, 20, 210, 120] },
    { id: "c", bounds: [0, 0, 100, 100] },
  ]);
  assert.deepEqual(places.get("a"), [0, 0]);
  assert.deepEqual(places.get("b"), [400 + PACK_GAP - 10, -20]);
  assert.deepEqual(places.get("c"), [400 + PACK_GAP + 200 + PACK_GAP, 0]);
  const a = worldBounds([0, 0, 400, 300], places.get("a"));
  const b = worldBounds([10, 20, 210, 120], places.get("b"));
  assert.equal(b[0] - a[2], PACK_GAP, "the gap is clear between the two plants");
  assert.equal(b[1], 0);
  assert.deepEqual([...pack([{ id: "x", bounds: [0, 0, 50, 50] }], 10).values()], [[0, 0]]);
});

test("bounds from wasm arrive as a Float32Array and pack as the plant's own box", () => {
  const places = pack([
    { id: "a", bounds: Float32Array.from([-20, -10, 700, 380]) },
    { id: "b", bounds: Float32Array.from([0, 0, 200, 120]) },
  ]);
  assert.deepEqual(places.get("a"), [20, 10]);
  assert.deepEqual(places.get("b"), [720 + PACK_GAP, 0]);
  const a = worldBounds(Float32Array.from([-20, -10, 700, 380]), places.get("a"));
  assert.deepEqual(a, [0, 0, 720, 390]);
});

test("a dragged place is kept and does not shift the others; a missing plant takes a placeholder", () => {
  const entries = [
    { id: "a", bounds: [0, 0, 400, 300] },
    { id: "b", bounds: null },
    { id: "c", bounds: [0, 0, 100, 100] },
  ];
  const places = pack(entries, PACK_GAP, { a: [-900, 50], c: ["bad", 1] });
  assert.deepEqual(places.get("a"), [-900, 50]);
  assert.deepEqual(places.get("b"), [400 + PACK_GAP, 0]);
  assert.deepEqual(places.get("c"), [400 + PACK_GAP + PLACEHOLDER[2] + PACK_GAP, 0], "a broken saved place is ignored");
  assert.deepEqual(worldBounds(null, [5, 5]), [5, 5, PLACEHOLDER[2] + 5, PLACEHOLDER[3] + 5]);
  assert.deepEqual(placesFrom("not json"), {});
  assert.deepEqual(placesFrom(null), {});
  assert.deepEqual(placesFrom("[1]"), {});
  assert.deepEqual(placesFrom('{"a":[1,2]}'), { a: [1, 2] });
});

test("the subscription names each project's next seq", () => {
  const seqs = { acan: 413, shop: 0 };
  assert.deepEqual(subscription(["acan", "shop"], (id) => seqs[id]), { from: { acan: 413, shop: 0 } });
});

test("a frame is an event of one project, a resume, or nothing", () => {
  const event = { seq: 7, at: 1, actor: { kind: "system" }, event: { type: "order_posted" } };
  const read = readFrame(JSON.stringify({ project: "shop", event }));
  assert.equal(read.kind, "event");
  assert.equal(read.project, "shop");
  assert.deepEqual(read.event, event);
  assert.deepEqual(JSON.parse(read.json), event, "the app folds the event alone");
  assert.deepEqual(readFrame('{"control":"resume","project":"shop","from":12}'), { kind: "resume", project: "shop", from: 12 });
  assert.deepEqual(readFrame("not json"), { kind: "skip" });
  assert.deepEqual(readFrame('{"project":"shop"}'), { kind: "skip" });
  assert.deepEqual(readFrame('{"project":"shop","event":{"type":"x"}}'), { kind: "skip" }, "an event needs its seq");
});

test("cursors are per project: the same seq applies in one and is old in another", () => {
  const cursors = new Map([["acan", 413], ["shop", 5]]);
  assert.equal(stepCursor(cursors, "shop", 5), "apply");
  assert.equal(stepCursor(cursors, "acan", 5), "skip");
  assert.equal(stepCursor(cursors, "acan", 413), "apply");
  assert.equal(stepCursor(cursors, "acan", 415), "gap");
  assert.equal(stepCursor(cursors, "new", 0), "apply", "a project with no cursor starts at 0");
  assert.equal(resumeStep(cursors, { project: "shop", from: 5 }), "resume");
  assert.equal(resumeStep(cursors, { project: "shop", from: 3 }), "resume");
  assert.equal(resumeStep(cursors, { project: "shop", from: 9 }), "reload", "events past the cursor were never sent");
});

test("1008 asks for a token again, 4001 reconnects at once, and anything else after a pause", () => {
  assert.equal(closeAction(1008), "sign-in");
  assert.equal(closeAction(4001), "resume");
  assert.equal(closeAction(1001), "retry");
  assert.equal(closeAction(1006), "retry");
});

function view(order = 0, question = "Count failed logins per client address or per account?") {
  return {
    machines: [{ name: "src/api", abnormal: false }],
    agents: [],
    orders: [{ id: order, phase: "asking", machine: "src/api", sentence: "add a rate limit" }],
    tasks: [],
    clashes: [],
    lanes: [],
    questions: [{ id: 0, order, text: question }],
  };
}

test("the rail merges projects by (project, id), and each row starts with its project", () => {
  const acan = inboxItems(view());
  const shop = inboxItems(view(0, "keep the old header?"));
  assert.equal(acan[0].key, shop[0].key, "the two worlds share the id");
  const merged = mergeInbox([{ project: "acan", items: acan }, { project: "shop", items: shop }]);
  assert.deepEqual(merged.map((item) => item.key), ["acan/question:0", "shop/question:0"]);
  assert.equal(merged[1].line, "shop · q-0 · keep the old header?");
  assert.equal(merged[1].text, "shop: q-0 asks: keep the old header?");
  assert.equal(merged[1].detail, "keep the old header?", "the hover keeps the question alone");
  assert.equal(merged[1].project, "shop");
  assert.equal(merged[1].machine, "src/api");
  assert.equal(entityKey("shop", "question:0"), "shop/question:0");
  assert.deepEqual([...countsByProject(merged)], [["acan", 1], ["shop", 1]]);
  assert.deepEqual(mergeInbox([{ project: "acan", items: [] }]), []);
  assert.ok(!("rank" in merged[0]));
});

test("the merged rail sorts by kind first, then by the canvas order", () => {
  const clash = { key: "clash:0", kind: "clash", id: 0, line: "c-0 · overlap", detail: "d", text: "t", machine: null };
  const question = { key: "question:0", kind: "question", id: 0, line: "q-0", detail: "d", text: "t", machine: null };
  const merged = mergeInbox([{ project: "acan", items: [question] }, { project: "shop", items: [clash, question] }]);
  assert.deepEqual(merged.map((item) => item.key), ["shop/clash:0", "acan/question:0", "shop/question:0"]);
});

test("the target follows the camera onto a project that is on the canvas, else stays", () => {
  const ready = (id) => id === "shop" || id === "acan";
  assert.equal(chooseTarget({ active: "acan", under: "shop", ready }), "shop");
  assert.equal(chooseTarget({ active: "acan", under: "acan", ready }), "acan");
  assert.equal(chooseTarget({ active: "acan", under: "", ready }), "acan", "nothing under the camera");
  assert.equal(chooseTarget({ active: "acan", under: "down", ready }), "acan", "a project whose log did not load");
});

test("a switch keeps each project's own share of the page state", () => {
  const stash = new Map();
  const state = { ...freshStash(), view: "kept", postedOrder: 4 };
  state.plantViews.set("o-1", "acan's");
  swapState(state, stash, "acan", "shop");
  assert.equal(state.postedOrder, null, "shop starts fresh");
  assert.equal(state.plantViews.size, 0);
  assert.equal(state.view, "kept", "the view is the app's, not the stash's");
  state.postedOrder = 9;
  swapState(state, stash, "shop", "acan");
  assert.equal(state.postedOrder, 4);
  assert.equal(state.plantViews.get("o-1"), "acan's");
  assert.equal(stash.get("shop").postedOrder, 9);
  assert.ok(!stash.has("acan"));
  assert.deepEqual(Object.keys(freshStash()).sort(), [...STASHED].sort());
});

test("go <project> matches a project's id or name, and Enter flies there", () => {
  const list = [
    { id: "acan", name: "Acan", status: "ready" },
    { id: "shop", name: "Storefront", status: "degraded" },
  ];
  const [shop] = projectMatches(list, "go shop");
  assert.deepEqual(shop, {
    kind: "project", key: "project-shop", label: "Storefront", detail: "project shop · degraded",
    verb: "project", target: { project: "shop" },
  });
  assert.equal(projectMatches(list, "go store")[0].target.project, "shop", "by name");
  assert.equal(projectMatches(list, "fly ACAN")[0].target.project, "acan");
  assert.deepEqual(projectMatches(list, "go zzz"), []);
  assert.deepEqual(projectMatches(list, "shop"), [], "only the go verb finds projects");
  assert.deepEqual(projectMatches(list, "go "), []);
  const found = search(view(), "go shop", { projects: list });
  assert.equal(found[0].kind, "project", "a project leads the results");
  assert.ok(!search(view(), "go acan").some((result) => result.kind === "project"), "one plant lists no project");
  assert.deepEqual(search(view(), "go src", { projects: list }).map((r) => r.kind), ["machine"]);
});
