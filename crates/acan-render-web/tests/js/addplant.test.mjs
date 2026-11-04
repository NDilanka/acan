// The "+" that adds a codebase while the server runs: the request it sends, the line it shows for
// each answer, and where the new plant goes beside the others. Run with:
//   node --test crates/acan-render-web/tests/js/addplant.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { ADD_PATH, addNote, addRequest } from "../../www/addplant-model.js";
import {
  PACK_GAP,
  PLACEHOLDER,
  placeAfter,
  projectEntries,
  projectList,
  upgradesOnAdd,
  worldBounds,
} from "../../www/projects.js";

const read = (path) => readFileSync(new URL(path, import.meta.url), "utf8");

// The body of the function `name` in `source`, up to the first line that closes it.
function fnBody(source, name) {
  const start = source.search(new RegExp(`(async )?function ${name}\\(`));
  assert.notEqual(start, -1, `${name} is defined`);
  const end = source.indexOf("\n}\n", start);
  return source.slice(start, end + 2);
}

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

test("a plant read again after its scan changes in place on a canvas, and the one-plant page is as it was", () => {
  const stream = read("../../www/stream.js");
  const multi = read("../../www/multi.js");
  const reload = fnBody(stream, "reloadPlant");
  assert.match(multi, /export function placeOf\(id\) \{\s*return multi\.places\.get\(id\) \?\? \[0, 0\];/);
  assert.match(stream, /import \{ listProjects, placeOf, replaceProjectPlant, startMulti \} from "\.\/multi\.js";/);
  assert.match(reload, /if \(target\(\) !== id\) return;/);
  assert.match(
    reload,
    /if \(canvasState\.multi\) \{[^}]*replaceProjectPlant\(id, text\);\s*\} else \{\s*const \[x, y\] = placeOf\(id\);\s*state\.app\.set_plant_at\(text, x, y\);\s*\}\s*scanner\(\)\.applyPlant\(text\);\s*requestRender\(\);/,
  );
  assert.doesNotMatch(reload, /state\.app\.set_plant\(text\)/);
  // The canvas path keeps the place and the log: add_project, never a fresh set_plant.
  assert.match(
    fnBody(multi, "replaceProjectPlant"),
    /const \[x, y\] = multi\.places\.get\(id\) \?\? \[0, 0\];\s*state\.app\.add_project\(id, text, x, y\);/,
  );
});

test("a plant that replaces the one on screen keeps the camera; only a plant on an empty canvas is fitted", () => {
  const app = read("../../src/app.rs");
  const add = app.slice(app.indexOf("pub fn add_project("), app.indexOf("pub fn move_project("));
  assert.match(add, /let in_place = self\.plant\.is_some\(\) && \(self\.project == id \|\| self\.order\.is_empty\(\)\);/);
  // The order is read before the id joins it, so a one-plant page's plant is taken in place.
  assert.ok(add.indexOf("let in_place") < add.indexOf("self.order.push(id.to_owned())"));
  assert.match(add, /if self\.plant\.is_none\(\) \|\| in_place \{\s*self\.project = id\.to_owned\(\);\s*self\.show\(plant, at_origin, \[x, y\], !in_place\);/);
  const show = app.slice(app.indexOf("fn show("), app.indexOf("fn build("));
  assert.match(show, /fn show\(&mut self, plant: Plant, at_origin: Layout, offset: \[f32; 2\], fit: bool\)/);
  assert.match(show, /if fit \{\s*\/\/[^\n]*\n\s*self\.fit_view\(\);\s*\}/);
  assert.match(show, /if fit \{\s*self\.motion = self\.new_motion\(\);\s*\} else \{\s*self\.motion\.cut\(\);\s*\}/);
  // The log is the mirror, which show leaves alone; the view folds from it again on the next frame.
  assert.doesNotMatch(show, /self\.mirror\s*=/);
  assert.match(show, /self\.dirty = true;/);
  // set_plant, set_plant_at and the load of a single plant still fit.
  assert.match(app, /self\.show\(plant, laid, \[x, y\], true\);/);
  assert.match(app, /self\.show\(plant, laid, \[0\.0; 2\], true\);/);
  assert.equal((app.match(/self\.show\(/g) ?? []).length, 3);
});

test("the kind menu changes a canvas plant in place too", () => {
  const kinds = read("../../www/kindmenu.js");
  assert.match(kinds, /if \(canvasState\.multi\) replaceProjectPlant\(canvasState\.active, text\);\s*else state\.app\.set_plant\(text\);/);
});

test("the list is read however many projects it holds, and the one-plant rule still needs two", () => {
  const one = [{ id: "shop", name: "Shop", default: true, status: "ready", head: { seq: 4 } }];
  assert.deepEqual(projectEntries(200, one), [{ id: "shop", name: "Shop", default: true, status: "ready", head: 4 }]);
  assert.equal(projectList(200, one), null);
  assert.deepEqual(projectEntries(200, []), []);
  assert.equal(projectEntries(404, one), null);
  assert.equal(projectEntries(200, { projects: one }), null);
  assert.deepEqual(projectEntries(200, [{ name: "no id" }, ...one]).map((p) => p.id), ["shop"]);
});

test("only a live one-plant page with a token turns into a canvas on an add", () => {
  const page = { multi: false, empty: false, replay: false, plant: null, token: "t" };
  assert.equal(upgradesOnAdd(page), true);
  assert.equal(upgradesOnAdd({ ...page, multi: true }), false, "a canvas adds beside the others");
  assert.equal(upgradesOnAdd({ ...page, empty: true }), false, "an empty page attaches its first");
  assert.equal(upgradesOnAdd({ ...page, replay: true }), false);
  assert.equal(upgradesOnAdd({ ...page, plant: "fixture" }), false);
  assert.equal(upgradesOnAdd({ ...page, plant: "stress" }), false);
  assert.equal(upgradesOnAdd({ ...page, token: null }), false);
  assert.equal(upgradesOnAdd({ ...page, token: "" }), false);
});

test("an add on a one-plant page turns it into a canvas and flies to the new plant", () => {
  const multi = read("../../www/multi.js");
  const attach = fnBody(multi, "attachProject");
  assert.match(attach, /if \(state\.empty\) return attachFirst\(project\);/);
  assert.match(attach, /plant: params\.get\("plant"\), token: state\.token/);
  assert.match(attach, /return upgradesOnAdd\(page\) \? upgradeToCanvas\(project\) : false;/);
  const upgrade = fnBody(multi, "upgradeToCanvas");
  const order = [
    "await readProjects(state.token)",
    "leaveOnePlant();",
    "texts = await startMulti(list);",
    'window.dispatchEvent(new Event("acan:attached"));',
    "flyProject(project.id);",
    "applyPlantScan(text);",
    "requestRender();",
    "return true;",
  ];
  let at = -1;
  for (const step of order) {
    const next = upgrade.indexOf(step, at + 1);
    assert.ok(next > at, `${step} comes in order`);
    at = next;
  }
  // The new codebase is on the list the canvas starts from, even when the list was read before it.
  assert.match(upgrade, /: \[\.\.\.listed, fresh\];/);
  // A canvas that cannot start leaves the one-plant page as it was, with its stream again.
  assert.match(upgrade, /canvasState\.multi = false;\s*canvasState\.list = \[\];\s*canvasState\.active = null;\s*resumeOnePlant\(\);\s*return false;/);
  assert.match(fnBody(multi, "readProjects"), /return projectEntries\(res\.status, await res\.json\(\)\);/);
  assert.match(fnBody(multi, "startMulti"), /return texts;\s*\}$/);
});

test("the one-plant stream is let go before it closes, so it never reconnects over the canvas", () => {
  const stream = read("../../www/stream.js");
  assert.match(fnBody(stream, "leaveOnePlant"), /const old = state\.socket;\s*state\.socket = null;\s*old\?\.close\(\);/);
  const connect = fnBody(stream, "connect");
  assert.match(connect, /ws\.onclose = \(event\) => \{\s*if \(state\.socket !== ws\) return;/);
  assert.match(connect, /ws\.onmessage = \(message\) => \{\s*if \(state\.socket !== ws\) return;/);
  const resync = fnBody(stream, "resync");
  assert.match(resync, /if \(!state\.token \|\| canvasState\.multi\) return;/);
  assert.match(resync, /await reload\(\);\s*if \(canvasState\.multi\) return;\s*connect\(\);/);
  assert.match(fnBody(stream, "reload"), /if \(canvasState\.multi\) return;/);
});

test("each add frames the new plant, beside the others, filling in with its scan", () => {
  const multi = read("../../www/multi.js");
  const attach = fnBody(multi, "attachProject");
  assert.match(attach, /const place = placeAfter\(rightEdge, origin\);/);
  assert.match(attach, /multi\.origin\.set\(project\.id, origin\);\s*multi\.places\.set\(project\.id, place\);\s*state\.app\.add_project\(project\.id, text, place\[0\], place\[1\]\);/);
  assert.match(attach, /flyProject\(project\.id\);\s*applyPlantScan\(text\);\s*requestRender\(\);\s*return true;/);
  const first = fnBody(multi, "attachFirst");
  assert.match(first, /await startMulti\(\[/);
  assert.match(first, /flyProject\(project\.id\);\s*requestRender\(\);\s*return true;/);
  // A canvas packs every plant one gap apart, keeps their bounds for boundsOf, and follows the scan
  // of the plant on screen.
  const start = fnBody(multi, "startMulti");
  assert.match(start, /multi\.origin\.set\(project\.id, Array\.from\(state\.app\.plant_bounds\(result\.value\)\)\);/);
  assert.match(start, /multi\.places = pack\(entries, PACK_GAP, saved\);/);
  assert.match(start, /state\.app\.add_project\(project\.id, texts\.get\(project\.id\), x, y\);/);
  assert.match(start, /applyPlantScan\(texts\.get\(first\.id\)\);/);
  assert.match(start, /requestRender\(\);/);
  assert.match(multi, /export function boundsOf\(id\) \{\s*const place = multi\.places\.get\(id\);\s*return place === undefined \? null : worldBounds\(multi\.origin\.get\(id\), place\);/);
});

test("the + sits in the shell at the top left, never in the bottom-right corner", () => {
  const html = read("../../www/index.html");
  const open = html.indexOf('<div class="island" id="shell">');
  assert.notEqual(open, -1);
  const shell = html.slice(open, html.indexOf("</div>", open));
  assert.match(shell, /<button type="button" id="add-plant"/);
  assert.equal((html.match(/id="add-plant"/g) ?? []).length, 1);
  const shellRule = /\n\s*#shell \{([^}]*)\}/.exec(html);
  assert.ok(shellRule, "#shell has its own rule");
  assert.match(shellRule[1], /\btop: 12px;/);
  assert.match(shellRule[1], /\bleft: 12px;/);
  assert.doesNotMatch(shellRule[1], /\b(right|bottom)\s*:/);
  const island = /\n\s*\.island \{([^}]*)\}/.exec(html);
  assert.doesNotMatch(island[1], /\b(right|bottom)\s*:/);
  const rules = [...html.matchAll(/([^{}]*#add-plant[^{}]*)\{([^}]*)\}/g)];
  for (const [, selector, body] of rules) {
    assert.doesNotMatch(body, /\b(right|bottom|inset)\s*:/, `${selector.trim()} does not pin the + right or bottom`);
    assert.doesNotMatch(body, /position\s*:\s*(fixed|absolute)/, `${selector.trim()} keeps the + in the shell`);
  }
});

test("Settings' Add a project posts live through the same request and attach as the +", () => {
  const settings = readFileSync(new URL("../../www/settings.js", import.meta.url), "utf8");
  const live = readFileSync(new URL("../../www/addplant.js", import.meta.url), "utf8");
  assert.match(settings, /import \{ addCodebase, addedNote \} from "\.\/addplant\.js";/);
  assert.match(settings, /async function addProject\(\) \{[\s\S]*?const answer = await addCodebase\(path\);/);
  assert.doesNotMatch(settings, /projects: \{ add: \[path\] \}/);
  assert.match(live, /export async function addCodebase\(path\) \{\s*const \{ url, init \} = addRequest\(state\.token, path\);/);
  assert.match(live, /const answer = await addCodebase\(input\.value\);/);
  assert.match(live, /live = await attach\(\{ id: body\.id, name: body\.name \}\);/);
});

test("a codebase added while the page is open gets its own plate, painted with its name", () => {
  const multi = readFileSync(new URL("../../www/multi.js", import.meta.url), "utf8");
  const plates = readFileSync(new URL("../../www/nameplates.js", import.meta.url), "utf8");
  assert.match(plates, /export function addPlate\(project\) \{\s*if \(layer === null \|\| plates\.has\(project\.id\)\) return;/);
  assert.match(plates, /for \(const project of canvasState\.list\) addPlate\(project\);/);
  assert.match(multi, /canvasState\.list\.push\(known\);\s*addPlate\(known\);\s*paintPlates\(\);/);
});

test("a 1008 close on the multiplexed stream asks the token again before the owner is signed out", () => {
  const multi = readFileSync(new URL("../../www/multi.js", import.meta.url), "utf8");
  assert.match(multi, /if \(action === "sign-in"\) \{\s*void recheckRefusal\(\);\s*return;\s*\}/);
  assert.doesNotMatch(multi, /action === "sign-in"\) \{\s*forgetToken\(\);/);
  assert.match(
    multi,
    /async function recheckRefusal\(\) \{\s*const res = await fetch\("\/api\/projects", \{ headers: \{ Authorization: `Bearer \$\{state\.token\}` \} \}\)\.catch\(\(\) => null\);\s*if \(res === null \|\| \(res\.status !== 401 && res\.status !== 403\)\) \{\s*setLink\("reconnecting"\);\s*setTimeout\(resync, RECONNECT_MS\);\s*return;\s*\}\s*forgetToken\(\);\s*\}/,
  );
});
