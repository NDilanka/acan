// The first run on an origin with no codebase: the sign-in link's fragment, its exchange, the empty
// world's detection, and the "Add a codebase with +" hint. Run with:
//   node --test crates/acan-render-web/tests/js/emptystart.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import {
  SIGNIN_EXPIRED,
  addressWithout,
  createEmptyWorld,
  exchangeRequest,
  exchangeSignIn,
  isEmptyWorld,
  paintEmptyHint,
  showSignInExpired,
  signInCodeOf,
} from "../../www/emptystart.js";

// A stand-in document: elements by id.
function fakeDoc(elements) {
  const byId = new Map(Object.entries(elements));
  return { getElementById: (id) => byId.get(id) ?? null };
}

// A stand-in element with a class list.
function classed() {
  const names = new Set();
  return {
    classList: {
      toggle: (name, on) => (on ? names.add(name) : names.delete(name), on),
      contains: (name) => names.has(name),
    },
  };
}

test("the sign-in fragment yields its code, and no other fragment does", () => {
  assert.equal(signInCodeOf("#signin=abc123_-X"), "abc123_-X");
  assert.equal(signInCodeOf("#signin="), null, "an empty code is no code");
  assert.equal(signInCodeOf("#signin=a&token=b"), null, "a code never carries a second field");
  assert.equal(signInCodeOf("#token=owner"), null, "the owner token fragment is left to the token path");
  assert.equal(signInCodeOf(""), null);
  assert.equal(signInCodeOf(undefined), null);
});

test("the address keeps its path and query once the fragment is gone", () => {
  assert.equal(addressWithout("/", ""), "/");
  assert.equal(addressWithout("/app/", "?view=1"), "/app/?view=1");
});

test("the exchange posts the code with no auth header, and the token is never in the URL", () => {
  const { url, init } = exchangeRequest("abc123");
  assert.equal(url, "/api/signin/exchange");
  assert.equal(init.method, "POST");
  assert.equal(init.headers.Authorization, undefined);
  assert.deepEqual(JSON.parse(init.body), { code: "abc123" });
  assert.equal(url.includes("?"), false);
});

test("a 200 exchange stores the returned token and resolves ok", async () => {
  const stored = [];
  const calls = [];
  const fetchMock = async (url, init) => {
    calls.push({ url, init });
    return { status: 200, json: async () => ({ token: "owner-token" }) };
  };
  const answer = await exchangeSignIn("abc123", { fetch: fetchMock, store: (token) => stored.push(token) });
  assert.deepEqual(answer, { ok: true });
  assert.deepEqual(stored, ["owner-token"]);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, "/api/signin/exchange");
});

test("a 401 exchange stores nothing and says the link expired", async () => {
  const stored = [];
  const fetchMock = async () => ({ status: 401, json: async () => ({ error: "unauthorized" }) });
  const answer = await exchangeSignIn("used", { fetch: fetchMock, store: (token) => stored.push(token) });
  assert.equal(answer.ok, false);
  assert.equal(answer.message, "This sign-in link has expired or was already used. Run acan open again.");
  assert.equal(answer.message, SIGNIN_EXPIRED);
  assert.deepEqual(stored, []);
});

test("a failed request or a 200 without a token is the same expired-link failure", async () => {
  const stored = [];
  const broken = await exchangeSignIn("x", {
    fetch: async () => { throw new TypeError("offline"); },
    store: (token) => stored.push(token),
  });
  assert.equal(broken.message, SIGNIN_EXPIRED);
  const empty = await exchangeSignIn("x", {
    fetch: async () => ({ status: 200, json: async () => ({}) }),
    store: (token) => stored.push(token),
  });
  assert.equal(empty.ok, false);
  assert.deepEqual(stored, []);
});

test("the expired-link line shows in the sign-in gate and opens it", () => {
  const gate = { hidden: true };
  const note = { hidden: true, textContent: "" };
  showSignInExpired(fakeDoc({ gate, "gate-note": note }));
  assert.equal(gate.hidden, false);
  assert.equal(note.hidden, false);
  assert.equal(note.textContent, "This sign-in link has expired or was already used. Run acan open again.");
});

test("an empty list is an empty world; a list with a codebase, a 404 or a bad body is not", () => {
  assert.equal(isEmptyWorld(200, []), true);
  assert.equal(isEmptyWorld(200, [{ id: "shop" }]), false);
  assert.equal(isEmptyWorld(404, { error: "no_codebase" }), false, "a 404 is not an empty list");
  assert.equal(isEmptyWorld(200, null), false);
  assert.equal(isEmptyWorld(200, { projects: [] }), false);
});

test("the hint shows for an empty list and is hidden after the first plant attaches", () => {
  const world = createEmptyWorld();
  assert.equal(world.listed([]), true, "an empty origin shows the hint");
  assert.equal(world.showing, true);
  world.attach();
  assert.equal(world.showing, false, "the first plant hides it");
  assert.equal(world.listed([]), false, "a list read empty again does not bring it back");
  assert.equal(world.showing, false);
});

test("a list with a codebase never shows the hint", () => {
  const world = createEmptyWorld();
  assert.equal(world.listed([{ id: "shop" }]), false);
  assert.equal(world.showing, false);
});

test("painting the hint shows it with the + lit, and hiding it puts the + out", () => {
  const hint = { hidden: true };
  const add = classed();
  const doc = fakeDoc({ "empty-hint": hint, "add-plant": add });
  paintEmptyHint(doc, true);
  assert.equal(hint.hidden, false);
  assert.equal(add.classList.contains("beckon"), true);
  paintEmptyHint(doc, false);
  assert.equal(hint.hidden, true);
  assert.equal(add.classList.contains("beckon"), false, "the first plant puts the + out");
});

test("the hint names the + in the middle of the page, with no line drawn to it", () => {
  const html = readFileSync(new URL("../../www/index.html", import.meta.url), "utf8");
  const source = readFileSync(new URL("../../www/emptystart.js", import.meta.url), "utf8");
  assert.doesNotMatch(html, /empty-hint-line/);
  assert.doesNotMatch(source, /empty-hint-line|pointAtAddButton|rotate\(/);
  assert.match(html, /<div id="empty-hint" hidden aria-hidden="true"><p id="empty-hint-text">Add a codebase with <kbd>\+<\/kbd><\/p><\/div>/);
  assert.match(html, /#empty-hint \{ position: fixed; inset: 0; display: grid; place-items: center;/);
  // The lit + is an ink fill from the theme, as a pressed key in the shell is, in every theme.
  const lit = /#shell #add-plant\.beckon \{([^}]*)\}/.exec(html);
  assert.ok(lit, "the + has a lit rule");
  assert.match(lit[1], /background: var\(--ink\);/);
  assert.match(lit[1], /color: var\(--on-ink\);/);
  assert.match(lit[1], /border-color: var\(--ink\);/);
  assert.doesNotMatch(lit[1], /#[0-9a-f]{3,8}\b|animation|red|amber/i);
  // noteAttached paints the hint hidden, which puts the + out.
  assert.match(source, /export function noteAttached\(\) \{\s*world\.attach\(\);\s*paintEmptyHint\(document, false\);/);
  assert.match(source, /classList\?\.toggle\("beckon", showing\)/);
});

test("the page takes the sign-in fragment before any other work, and exchanges it on boot", () => {
  const main = readFileSync(new URL("../../www/main.js", import.meta.url), "utf8");
  assert.match(main, /const signInCode = replay \? null : takeSignInCode\(\);\s*const started = performance\.now\(\);\s*await init\(\);/);
  assert.match(main, /history\.replaceState\(null, "", addressWithout\(location\.pathname, location\.search\)\);/);
  assert.match(main, /const answer = await exchangeSignIn\(signInCode, \{\s*store: \(token\) => sessionStorage\.setItem\(TOKEN_KEY, token\),\s*\}\);/);
  assert.match(main, /if \(!answer\.ok\) \{\s*showSignInExpired\(document\);/);
});

test("an origin with no codebase opens no stream and calls no project route", () => {
  const stream = readFileSync(new URL("../../www/stream.js", import.meta.url), "utf8");
  const multi = readFileSync(new URL("../../www/multi.js", import.meta.url), "utf8");
  assert.match(stream, /if \(projects !== null && projects\.length === 0\) \{\s*enterEmpty\(\);\s*return;\s*\}/);
  assert.match(multi, /if \(isEmptyWorld\(res\.status, body\)\) return \[\];/);
  assert.match(multi, /const body = await res\.json\(\);/);
  for (const file of ["shell.js", "firstrun-panel.js", "workers.js"]) {
    const source = readFileSync(new URL(`../../www/${file}`, import.meta.url), "utf8");
    assert.match(source, /state\.empty/, `${file} reads state.empty before a project route`);
  }
});

test("the first codebase added to an empty origin turns the page on as a canvas of one", () => {
  const multi = readFileSync(new URL("../../www/multi.js", import.meta.url), "utf8");
  assert.match(multi, /if \(!canvasState\.multi\) \{\s*if \(state\.empty\) return attachFirst\(project\);/);
  assert.match(multi, /await startMulti\(\[\{ id: project\.id, name: project\.name, default: false, status: "mapping", head: null \}\]\);/);
  assert.match(multi, /state\.empty = false;\s*noteAttached\(\);/);
});
