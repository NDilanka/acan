// The "+" that adds a codebase while the server runs: the request it sends, the line it shows for
// each answer, and where the new plant goes beside the others. Run with:
//   node --test crates/acan-render-web/tests/js/addplant.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { ADD_PATH, addNote, addRequest } from "../../www/addplant-model.js";
import { PACK_GAP, PLACEHOLDER, placeAfter, worldBounds } from "../../www/projects.js";

test("the add posts the trimmed path with the owner's token to the settings route", () => {
  const { url, init } = addRequest("owner-token", "  /work/shop  ");
  assert.equal(url, ADD_PATH);
  assert.equal(url, "/api/settings/projects");
  assert.equal(init.method, "POST");
  assert.equal(init.headers.Authorization, "Bearer owner-token");
  assert.equal(init.headers["Content-Type"], "application/json");
  assert.deepEqual(JSON.parse(init.body), { path: "/work/shop" });
});

test("a missing path still posts a body, which the server refuses with 400", () => {
  assert.deepEqual(JSON.parse(addRequest("t", undefined).init.body), { path: "" });
});

test("each answer to an add says what happened, and only 202 adds", () => {
  assert.equal(addNote(202, { id: "shop", name: "shop", status: "mapping" }), "Added. Its first scan is running.");
  assert.match(addNote(0, null), /Could not reach the server/);
  assert.match(addNote(400, { error: "bad_request" }), /Type a folder path/);
  assert.match(addNote(401, null), /Only the owner/);
  assert.match(addNote(403, null), /Only the owner/);
  assert.match(addNote(409, { error: "conflict", what: "already_served" }), /already served/);
  assert.match(addNote(409, { error: "conflict", what: "live_add_unavailable" }), /not available on this server/);
  assert.match(addNote(500, null), /could not be opened/);
});

test("a refused path names the rule it broke, and the first field's rule is used", () => {
  const body = {
    error: "invalid",
    fields: [{ path: "path", why: "must be an existing folder" }],
  };
  assert.equal(addNote(422, body), "Not added: the path must be an existing folder. Nothing changed.");
  assert.equal(
    addNote(422, { error: "invalid", fields: [] }),
    "Not added: the path is not a folder this server can serve. Nothing changed.",
  );
});

test("a new plant sits one gap right of the others, its top on the origin line", () => {
  const origin = [-10, -4, 150, 90];
  const place = placeAfter(400, origin);
  assert.deepEqual(place, [400 + PACK_GAP + 10, 4]);
  const [left, top] = worldBounds(origin, place);
  assert.equal(left, 400 + PACK_GAP);
  assert.equal(top, 0);
});

test("a plant whose bounds do not read falls back to the placeholder box", () => {
  assert.deepEqual(placeAfter(0, null), [PACK_GAP - PLACEHOLDER[0], 0 - PLACEHOLDER[1]]);
});

test("a codebase added while the page is open gets its own plate, painted with its name", () => {
  const multi = readFileSync(new URL("../../www/multi.js", import.meta.url), "utf8");
  const plates = readFileSync(new URL("../../www/nameplates.js", import.meta.url), "utf8");
  assert.match(plates, /export function addPlate\(project\) \{\s*if \(layer === null \|\| plates\.has\(project\.id\)\) return;/);
  assert.match(plates, /for \(const project of canvasState\.list\) addPlate\(project\);/);
  assert.match(multi, /canvasState\.list\.push\(known\);\s*addPlate\(known\);\s*paintPlates\(\);/);
});
