// Node tests for the command bar's queries and verbs, over a small view. Run with:
//   node --test crates/acan-render-web/tests/js/command.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { MAX_RESULTS, idScore, parseQuery, receiptItems, score, search } from "../../www/command.js";

const VIEW = {
  machines: [
    { name: "auth", min: [0, 0], size: [1, 1], abnormal: true },
    { name: "auth/session", min: [0, 0], size: [1, 1], abnormal: false },
    { name: "store", min: [0, 0], size: [1, 1], abnormal: false },
    { name: "billing", min: [0, 0], size: [1, 1], abnormal: false },
  ],
  orders: [
    { id: 3, machine: "store", sentence: "Keep signup answers the same", phase: "split", tasks: 1, at: null },
    { id: 4, machine: "auth", sentence: "Rotate the session key", phase: "splitting", tasks: 0, at: null },
  ],
  tasks: [
    { id: 7, order: 3, title: "a", state: "editing", agent: "worker-2", machine: "auth", change: null, check: null,
      paths: ["src/auth/login.rs", "src/store/db.rs"] },
  ],
  agents: [
    { id: "worker-1", task: null, activity: "idle", machine: null, body: null },
    { id: "worker-2", task: 7, activity: "editing", machine: "auth", body: [0, 0, 1, 1] },
  ],
  questions: [{ id: 1, order: 3, text: "Keep the old cookie name?", options: ["keep", "rename"], at: null }],
  clashes: [
    { id: 2, state: "settled", label: "c-2 · settled", options: [] },
    { id: 5, state: "open", label: "c-5 · breaks w-1", options: [] },
  ],
  lanes: [{ order: 3, rev: 2, confirmation: null }, { order: 4, rev: 1, confirmation: { scope_rev: 1 } }],
};

const RECEIPTS = [{ order: 3, item_id: "keep-auth-cookie", machine_id: "auth", class: "pass" }];

const labels = (results) => results.map((result) => result.label);

test("a query is an optional verb, then a term", () => {
  assert.deepEqual(parseQuery("  go   auth "), { verb: "go", term: "auth" });
  assert.deepEqual(parseQuery("fly store"), { verb: "go", term: "store" });
  assert.deepEqual(parseQuery("Agent w2"), { verb: "agent", term: "w2" });
  assert.deepEqual(parseQuery("order here"), { verb: "order", term: "here" });
  assert.deepEqual(parseQuery("auth"), { verb: null, term: "auth" });
  assert.deepEqual(parseQuery(""), { verb: null, term: "" });
});

test("a match ranks the same name, a prefix, a part, a substring, then letters in order", () => {
  assert.equal(score("auth", "auth"), 0);
  assert.equal(score("au", "auth"), 1);
  assert.equal(score("sess", "auth/session"), 2);
  assert.equal(score("uth", "auth"), 3);
  assert.equal(score("w2", "worker-2"), 4);
  assert.equal(score("zz", "auth"), null);
  assert.equal(idScore("o-3", "o", 3), 0);
  assert.equal(idScore("o3", "o", 3), 0);
  assert.equal(idScore("3", "o", 3), 0);
  assert.equal(idScore("o-33", "o", 3), null);
});

test("a machine name finds the machine first, and Enter goes there", () => {
  const results = search(VIEW, "auth");
  assert.equal(results[0].label, "auth");
  assert.equal(results[0].verb, "go");
  assert.deepEqual(results[0].target, { machine: "auth" });
  assert.match(results[0].detail, /a clash stands here/);
  assert.ok(labels(results).includes("auth/session"));
});

test("an order id finds the order, and going to it flies to its machine", () => {
  const [first] = search(VIEW, "o-4");
  assert.equal(first.kind, "order");
  assert.equal(first.label, "o-4 · Rotate the session key");
  assert.deepEqual(first.target, { machine: "auth" });
});

test("agent w2 finds the agent to ride, and says where it works", () => {
  const results = search(VIEW, "agent w2");
  assert.equal(results[0].label, "worker-2");
  assert.equal(results[0].verb, "ride");
  assert.deepEqual(results[0].target, { agent: "worker-2" });
  assert.equal(results[0].detail, "agent · editing on auth");
  assert.ok(results.every((result) => result.kind === "agent"));
});

test("go to an agent flies to its machine, and an idle agent with no machine is left out", () => {
  const results = search(VIEW, "go worker");
  assert.deepEqual(labels(results), ["worker-2"]);
  assert.deepEqual(results[0].target, { machine: "auth" });
});

test("touched names each declared path and each receipt item, and says which it is", () => {
  const results = search(VIEW, "touched auth", { receipts: RECEIPTS });
  const path = results.find((result) => result.kind === "path");
  const item = results.find((result) => result.kind === "item");
  assert.equal(path.label, "src/auth/login.rs");
  assert.equal(path.detail, "declared path · t-7 of o-3 on auth");
  assert.equal(item.label, "keep-auth-cookie");
  assert.equal(item.detail, "receipt item · o-3 · pass on auth");
  assert.ok(!labels(results).includes("src/store/db.rs"), "a path that does not name the term is left out");
});

test("order here opens the compose panel where the page looks; order auth names the machine", () => {
  assert.deepEqual(search(VIEW, "order here")[0].target, { machine: null });
  assert.deepEqual(search(VIEW, "order")[0].target, { machine: null });
  const [first] = search(VIEW, "order store");
  assert.equal(first.label, "order on store");
  assert.equal(first.verb, "order");
  assert.deepEqual(first.target, { machine: "store" });
});

test("seal lists the lanes, the slip's own order first, and says when the slip shows another", () => {
  const results = search(VIEW, "seal", { shownOrder: 3 });
  assert.deepEqual(labels(results), ["seal o-3", "seal o-4"]);
  assert.equal(results[0].detail, "scope · unsealed rev 2");
  assert.equal(results[1].detail, "scope · sealed rev 1 · the slip shows o-3 now");
});

test("an empty bar shows what waits on you first, then the verbs", () => {
  const results = search(VIEW, "", { shownOrder: 3 });
  assert.equal(results[0].kind, "question");
  assert.equal(results[1].label, "c-5 · breaks w-1", "a settled clash does not wait on you");
  assert.ok(labels(results).includes("order here"));
  assert.ok(labels(results).includes("seal"));
  assert.ok(results.length <= MAX_RESULTS);
});

test("a question and a clash are found by id, and open their cards", () => {
  assert.deepEqual(search(VIEW, "q-1")[0].target, { question: 1 });
  assert.deepEqual(search(VIEW, "c-5")[0].target, { clash: 1 }, "the clash index in the view");
  assert.deepEqual(search(VIEW, "c-2"), [], "a settled clash is not offered");
});

test("a term that matches nothing finds nothing, and results are capped", () => {
  assert.deepEqual(search(VIEW, "zzzz"), []);
  const many = { ...VIEW, machines: Array.from({ length: 20 }, (_, i) => ({ name: `m${i}`, min: [0, 0], size: [1, 1], abnormal: false })) };
  assert.equal(search(many, "m").length, MAX_RESULTS);
});

test("the receipt items come from each plant view, admitted play or not", () => {
  const views = new Map([
    [3, { play: { items: [{ item_id: "a", machine_id: "auth", class: "pass" }] }, items: [] }],
    [4, { play: null, items: [{ item_id: "b", machine_id: "store", class: "fail" }] }],
  ]);
  assert.deepEqual(receiptItems(views), [
    { order: 3, item_id: "a", machine_id: "auth", class: "pass" },
    { order: 4, item_id: "b", machine_id: "store", class: "fail" },
  ]);
});
