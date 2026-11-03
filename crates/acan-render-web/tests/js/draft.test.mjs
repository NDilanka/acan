// Node tests for the scope draft's adoption: the ReviseScope the slip's Adopt draft sends, built
// from the GET /api/orders/{order}/scope-draft answer. Run with:
//   node --test crates/acan-render-web/tests/js/draft.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { adoptCommand, draftSummary, wantsDraft } from "../../www/draft.js";

const fixture = (name) => JSON.parse(readFileSync(new URL(`../../fixtures/d02/${name}`, import.meta.url), "utf8"));

test("adopting a draft sends revise_scope with the proposal at the draft's revision and base commit", () => {
  const answer = fixture("scope-draft-drafted.json");
  const command = adoptCommand(3, answer);
  assert.deepEqual(Object.keys(command), ["cmd", "order", "scope"]);
  assert.equal(command.cmd, "revise_scope");
  // The command's order is the numeric id the server decodes; the scope's order is the "o-N" ref.
  assert.equal(command.order, 3);
  assert.deepEqual(command.scope, {
    version: "acan-scope/1",
    order: "o-3",
    rev: answer.draft.rev,
    base_commit: answer.draft.base_commit,
    body: answer.draft.draft.proposal,
  });
  assert.deepEqual(Object.keys(command.scope.body).sort(), ["may", "must_keep", "open"]);
});

test("the scope's revision is the draft's own, not the order revision the answer names", () => {
  const answer = structuredClone(fixture("scope-draft-drafted.json"));
  answer.rev = 0;
  answer.draft.rev = 1;
  assert.equal(adoptCommand(3, answer).scope.rev, 1);
});

test("nothing is built from a failed draft, another order's draft or a draft of another version", () => {
  assert.throws(() => adoptCommand(3, fixture("scope-draft-failed.json")), /no draft/);
  assert.throws(() => adoptCommand(4, fixture("scope-draft-drafted.json")), /for o-3, not o-4/);
  const other = structuredClone(fixture("scope-draft-drafted.json"));
  other.draft.version = "acan-scope-draft/2";
  assert.throws(() => adoptCommand(3, other), /acan-scope-draft\/1/);
  const bodiless = structuredClone(fixture("scope-draft-drafted.json"));
  delete bodiless.draft.draft.proposal;
  assert.throws(() => adoptCommand(3, bodiless), /no scope body/);
});

test("the slip asks for a draft only for a live order with no lane", () => {
  const order = { id: 3, phase: "splitting" };
  assert.equal(wantsDraft(order, []), true);
  assert.equal(wantsDraft(order, [{ order: 3 }]), false);
  assert.equal(wantsDraft(order, [{ order: 2 }]), true);
  assert.equal(wantsDraft({ id: 3, phase: "landed" }, []), false);
  assert.equal(wantsDraft({ id: 3, phase: "cancelled" }, []), false);
  assert.equal(wantsDraft(undefined, []), false);
});

test("the slip's summary counts the draft and names its model, or says why it failed", () => {
  assert.deepEqual(draftSummary(fixture("scope-draft-drafted.json")), {
    kind: "drafted",
    may: 3,
    keep: 2,
    open: 1,
    model: "muse-spark",
    items: ["i-login-limit", "i-retry-after", "i-login-body"],
  });
  assert.deepEqual(draftSummary(fixture("scope-draft-failed.json")), {
    kind: "failed",
    reason: "the reply did not parse as a scope draft",
  });
});
