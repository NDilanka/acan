// Node tests for the Set kind menu's words and requests: the kinds it lists, the body it sends,
// what each refusal says, when it may be offered, and where it opens. Run with:
//   node --test crates/acan-render-web/tests/js/kinds.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  KIND_NAMES,
  canSetKind,
  kindBody,
  kindWords,
  menuPlace,
  menuRows,
  setKindRefusal,
} from "../../www/kinds.js";

test("the menu lists the seventeen kinds in the server's order, the fallback last", () => {
  assert.equal(KIND_NAMES.length, 17);
  assert.equal(new Set(KIND_NAMES).size, 17, "no kind is listed twice");
  assert.equal(KIND_NAMES[0], "intake");
  assert.equal(KIND_NAMES.at(-1), "assembler");
  assert.equal(kindWords("switchboard"), "Switchboard");
});

test("the menu marks the machine's kind now, and ends with the clearing row", () => {
  const rows = menuRows("gate");
  assert.equal(rows.length, 18);
  assert.deepEqual(rows.filter((row) => row.current).map((row) => row.kind), ["gate"]);
  assert.equal(rows.at(-1).kind, null, "the last row clears the choice");
  assert.equal(rows.at(-1).current, false);
});

test("a machine whose plant names no kind marks no row", () => {
  assert.equal(menuRows("").some((row) => row.current), false);
});

test("a choice sends the machine's name with its kind, and a clear sends null", () => {
  assert.deepEqual(kindBody("auth", "gate"), { auth: "gate" });
  assert.deepEqual(kindBody("auth", null), { auth: null });
});

test("a refusal is named in plain words, and a taken kind is no refusal", () => {
  assert.equal(setKindRefusal(200), null);
  assert.match(setKindRefusal(401), /owner token/);
  assert.match(setKindRefusal(403), /Only the owner/);
  assert.match(setKindRefusal(404), /not in the plant/);
  assert.match(setKindRefusal(400), /does not know that kind/);
  assert.match(setKindRefusal(500), /may not be set/, "a failure may have recorded the kind");
  assert.match(setKindRefusal(0), /may not be set/, "no answer at all is a failure too");
  assert.match(setKindRefusal(422), /refused the change/);
});

test("the menu is offered only on a live page with the owner's token", () => {
  assert.equal(canSetKind({ replay: false, held: false, token: "t" }), true);
  assert.equal(canSetKind({ replay: false, held: false, token: null }), false, "no token, no menu");
  assert.equal(canSetKind({ replay: true, held: false, token: "t" }), false, "a replay writes nothing");
  assert.equal(canSetKind({ replay: false, held: true, token: "t" }), false, "a held map shows the past");
});

test("the menu opens where the pointer is, and moves in to stay on screen", () => {
  const bounds = { width: 800, height: 600 };
  assert.deepEqual(menuPlace(100, 120, 180, 380, bounds), { left: 100, top: 120 });
  assert.deepEqual(menuPlace(790, 500, 180, 380, bounds), { left: 616, top: 216 });
  assert.deepEqual(menuPlace(0, 0, 180, 380, bounds), { left: 4, top: 4 });
});
