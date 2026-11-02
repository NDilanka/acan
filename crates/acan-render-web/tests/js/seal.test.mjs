// Node tests for the Seal and the Strike: which route answer the slip shows, what the Seal and the
// Strike send. The route fixture is the origin's own. Run with:
//   node --test crates/acan-render-web/tests/js/seal.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  askRoute,
  confirmationFor,
  leaseWords,
  picksFor,
  sealKey,
  sealOf,
  strikeScope,
  syncSeals,
} from "../../www/seal.js";

const ROUTE = JSON.parse(
  readFileSync(new URL("../../../acan-intent/fixtures/scope-confirmation.json", import.meta.url), "utf8"),
);

const ORDER = 3;

// A plot of the lane as the view names it. `broad` marks one that needs its own acknowledgement.
function mayView(id, broad) {
  return {
    id,
    label: `${id} label`,
    obligations: [`${id} label`, `${id} retry`],
    selector: "POST /api/login",
    change: "add",
    provenance: "user",
    broad,
    machines: ["src/api"],
  };
}

// An unconfirmed lane on revision 2 with two plots (the second broad), one keep and one open
// choice, as the view carries them.
function lane(overrides = {}) {
  return {
    order: ORDER,
    rev: 2,
    scope_hash: ROUTE.scope_hash,
    scope: {
      version: "acan-scope/1",
      order: "o-3",
      rev: 2,
      base_commit: ROUTE.base_commit,
      body: {
        may: [
          { id: "i-limit", change: "add" },
          { id: "i-every-step", change: "modify" },
        ],
        must_keep: [{ id: "k-login", text: "login keeps working", machines: ["src/api"] }],
        open: [
          {
            id: "c-window",
            machine_id: "src/api",
            question: "Which window?",
            options: [
              { key: "client", label: "Client" },
              { key: "server", label: "Server" },
            ],
          },
        ],
      },
    },
    state: "unconfirmed",
    may: [mayView("i-limit", false), mayView("i-every-step", true)],
    must_keep: [{ id: "k-login", text: "login keeps working", ruling_id: null, machines: ["src/api"] }],
    open: [
      {
        id: "c-window",
        machine: "src/api",
        question: "Which window?",
        options: [
          { key: "client", label: "Client" },
          { key: "server", label: "Server" },
        ],
      },
    ],
    confirmation: null,
    live_leases: 0,
    ...overrides,
  };
}

// A page state with the given lanes and route answers. `check` is what the wasm side answers.
function pageState(lanes, { check = { verdict: "match" }, fail = false, views = new Map() } = {}) {
  return {
    view: { lanes },
    sealViews: views,
    sealPicks: { hash: null, choices: new Map(), acks: new Set() },
    app: {
      seal_check(order, text) {
        if (fail) throw new Error("the page holds no scope for this order");
        assert.equal(order, "o-3");
        assert.equal(JSON.parse(text).scope_hash, ROUTE.scope_hash);
        return JSON.stringify(check);
      },
    },
  };
}

function routeText() {
  return JSON.stringify(ROUTE);
}

// Lets the sync's fetch and its follow-up sync run to the end.
async function settle() {
  for (let i = 0; i < 5; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
}

function quietly(run) {
  const original = console.error;
  console.error = () => {};
  return Promise.resolve(run()).finally(() => {
    console.error = original;
  });
}

test("a lane's route key moves with its revision and hash, not with a confirmation", () => {
  const open = lane();
  assert.equal(sealKey(open), `2|${ROUTE.scope_hash}`);
  assert.equal(sealKey(lane({ rev: 3 })) === sealKey(open), false);
  assert.equal(sealKey(lane({ scope_hash: "sha256:00" })) === sealKey(open), false);
  assert.equal(
    sealKey(lane({ confirmation: { scope_hash: ROUTE.scope_hash } })),
    sealKey(open),
    "a confirmation does not ask the route again",
  );
});

test("the Seal shows nothing until the route answers for this revision", () => {
  const state = pageState([lane()]);
  assert.equal(sealOf(state, lane()).kind, "hidden", "no answer yet");

  state.sealViews.set(ORDER, { key: sealKey(lane({ rev: 1 })), body: ROUTE, check: { verdict: "match" }, error: null });
  assert.equal(sealOf(state, lane()).kind, "hidden", "an answer for an older revision does not count");

  state.sealViews.set(ORDER, { key: sealKey(lane()), body: null, check: null, error: "answered 404" });
  assert.equal(sealOf(state, lane()).kind, "hidden", "a failed route shows no Seal");
});

test("a route that names another scope withholds the Seal, even for a sealed lane", () => {
  const state = pageState([lane()], { check: { verdict: "mismatch", page_hash: "sha256:aa", origin_hash: ROUTE.scope_hash } });
  state.sealViews.set(ORDER, {
    key: sealKey(lane()),
    body: ROUTE,
    check: { verdict: "mismatch", page_rev: 2, origin_rev: 2, page_hash: "sha256:aa", origin_hash: ROUTE.scope_hash },
    error: null,
  });
  assert.equal(sealOf(state, lane()).kind, "withheld");

  const sealed = lane({ state: "confirmed", confirmation: ROUTE });
  state.sealViews.set(ORDER, {
    key: sealKey(sealed),
    body: ROUTE,
    check: { verdict: "match" },
    error: null,
  });
  assert.equal(sealOf(state, sealed).kind, "sealed", "a match with the view's confirmation shows the stamp");

  const other = lane({ state: "confirmed", confirmation: { ...ROUTE, scope_hash: "sha256:bb" } });
  const withheld = sealOf(state, other);
  assert.equal(withheld.kind, "withheld", "a confirmation for another hash is never painted solid");
  assert.equal(withheld.confirmed, "sha256:bb");
});

test("an unsealed revision shows the Seal once the route matches the page", () => {
  const state = pageState([lane()]);
  state.sealViews.set(ORDER, { key: sealKey(lane()), body: ROUTE, check: { verdict: "match" }, error: null });
  const seal = sealOf(state, lane());
  assert.equal(seal.kind, "unsealed");
  assert.equal(seal.body.scope_hash, ROUTE.scope_hash);
});

test("the sync asks for each lane's route once per key and repaints when an answer lands", async () => {
  const state = pageState([lane()]);
  const asked = [];
  let repaints = 0;
  const fetchText = async (path) => {
    asked.push(path);
    return routeText();
  };
  syncSeals(state, fetchText, () => {
    repaints += 1;
  });
  await settle();
  assert.deepEqual(asked, ["/api/orders/o-3/scope/confirmation"]);
  assert.equal(sealOf(state, lane()).kind, "unsealed");
  assert.ok(repaints >= 1, "the slip repaints when the answer is kept");

  syncSeals(state, fetchText, () => {});
  await settle();
  assert.equal(asked.length, 1, "the same revision is not asked again");

  const moved = lane({ rev: 3 });
  state.view = { lanes: [moved] };
  syncSeals(state, fetchText, () => {});
  await settle();
  assert.equal(asked.length, 2, "a new revision asks again");
});

test("a route that does not answer is kept as a failure, so the Seal stays hidden", async () => {
  const state = pageState([lane()], { fail: true });
  await quietly(async () => {
    syncSeals(state, async () => routeText(), () => {});
    await settle();
  });
  assert.equal(sealOf(state, lane()).kind, "hidden");
  assert.equal(state.sealViews.get(ORDER).error, "the page holds no scope for this order");
});

test("askRoute returns the body and the wasm check of it", async () => {
  const state = pageState([lane()]);
  const { body, check } = await askRoute(state, async () => routeText(), ORDER);
  assert.equal(body.scope_rev, ROUTE.scope_rev);
  assert.equal(check.verdict, "match");
});

test("the Seal sends the route body with the human's answers and acknowledgements", () => {
  const body = ROUTE;
  const open = lane();
  const picks = { hash: null, choices: new Map(), acks: new Set() };

  const none = confirmationFor(body, open, picks, 5);
  assert.equal(none.ok, false);
  assert.deepEqual(none.missing, [
    { kind: "choice", id: "c-window" },
    { kind: "broad", id: "i-every-step" },
  ]);

  picks.choices.set("c-window", "client");
  const onlyChoice = confirmationFor(body, open, picks, 5);
  assert.deepEqual(onlyChoice.missing, [{ kind: "broad", id: "i-every-step" }]);

  picks.acks.add("i-every-step");
  const built = confirmationFor(body, open, picks, 5);
  assert.equal(built.ok, true);
  assert.deepEqual(Object.keys(built.confirmation).sort(), Object.keys(ROUTE).sort());
  assert.deepEqual(built.confirmation.answers, [{ choice_id: "c-window", option: "client" }]);
  assert.deepEqual(built.confirmation.broad_grants, [
    { item_id: "i-every-step", selector_hash: ROUTE.selector_hashes[1], by: ROUTE.by, at: 5 },
  ]);
  assert.deepEqual(built.confirmation.budget, ROUTE.budget, "the route's budget is kept");
  assert.equal(built.confirmation.scope_hash, ROUTE.scope_hash);
});

test("the picks belong to the scope they were made on", () => {
  const state = pageState([lane()]);
  const first = picksFor(state, lane());
  first.choices.set("c-window", "client");
  first.acks.add("i-every-step");
  assert.equal(picksFor(state, lane()).choices.get("c-window"), "client", "kept while the scope is the same");

  const next = picksFor(state, lane({ scope_hash: "sha256:cc" }));
  assert.equal(next.choices.size, 0, "a new scope starts with no answers");
  assert.equal(next.acks.size, 0);
});

test("a Strike removes one plot, one revision on, and keeps the constraints and choices", () => {
  const scope = lane().scope;
  const struck = strikeScope(scope, "i-limit");
  assert.equal(struck.rev, 3);
  assert.deepEqual(struck.body.may.map((item) => item.id), ["i-every-step"]);
  assert.deepEqual(struck.body.must_keep, scope.body.must_keep);
  assert.deepEqual(struck.body.open, scope.body.open);
  assert.equal(scope.rev, 2, "the lane's own scope is not changed");
  assert.equal(scope.body.may.length, 2);
  assert.throws(() => strikeScope(scope, "i-missing"), /no plot i-missing/);
});

test("the lease count reads as the Strike warns it", () => {
  assert.equal(leaseWords(0), "ends no live leases");
  assert.equal(leaseWords(1), "ends 1 live lease");
  assert.equal(leaseWords(3), "ends 3 live leases");
});
