// Node tests for the chrome's own rules and for the page's resting chrome: what the shell, the
// key bar, the rail, the badges and the replay bar show. Run with:
//   node --test crates/acan-render-web/tests/js/chrome.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { linkQuiet, metaBadgeText } from "../../www/chrome.js";
import { keyAction } from "../../www/keys.js";

const www = (name) => readFileSync(new URL(`../../www/${name}`, import.meta.url), "utf8");

const NONE = { command: false, compose: false, viewer: false, question: false, clash: false };
const ctx = { armed: null, now: 1000, letters: new Map(), held: false, riding: false, open: NONE };
const press = (key, extra = {}) => ({ key, code: "", repeat: false, shift: false, ...extra });

test("the ? key toggles the key bar, and / still opens the command bar", () => {
  const help = keyAction(press("?", { shift: true }), ctx);
  assert.deepEqual(help.action, { kind: "toggle-help" });
  assert.equal(help.prevent, true);
  assert.equal(keyAction(press("?", { shift: true, repeat: true }), ctx).action, null, "a held ? does not flicker the bar");
  assert.deepEqual(keyAction(press("/"), ctx).action, { kind: "open-command" });
});

test("the key bar is hidden at rest, behind the shell's ? button, with its keys kept", () => {
  const html = www("index.html");
  const hint = html.match(/<div class="island" id="hint"([^>]*)>([\s\S]*?)<\/div>/);
  assert.ok(hint, "the key bar is on the page");
  assert.match(hint[1], /\bhidden\b/);
  for (const key of ["Alt", "0", "C", "A", "O", "/"]) assert.match(hint[2], new RegExp(`<kbd>${key.replace("/", "\\/")}</kbd>`));
  const shell = html.match(/<div class="island" id="shell">([\s\S]*?)<\/div>/)[1];
  assert.match(shell, /id="help-toggle"[^>]*aria-controls="hint"[^>]*>\?<\/button>/);
  assert.match(shell, /id="command-open"[^>]*>\/<\/button>/);
  assert.match(www("main.js"), /case "toggle-help":\s*toggleHelp\(\);/);
});

test("the shell hides healthy link words and keeps a problem in view", () => {
  for (const text of ["live · seq 5", "replay", "starting"]) assert.equal(linkQuiet(text), true, text);
  for (const text of ["failed", "sign in", "seq 12 · rewound", "replay stopped at seq 3", "", null]) {
    assert.equal(linkQuiet(text), false, String(text));
  }
});

test("the rail shows only what needs you at rest; Agents, Orders and Timeline are tabs", () => {
  const html = www("index.html");
  const rail = html.match(/<div id="watch">([\s\S]*?)<\/div>/)[1];
  const ids = [...rail.matchAll(/id="([\w-]+)"/g)].map((m) => m[1]);
  assert.deepEqual(ids.slice(0, 5), ["firstrun", "card", "slip", "inbox", "rail-tabs"]);
  const tabs = rail.match(/<aside id="rail-tabs"[^>]*aria-label="([^"]+)"/);
  assert.ok(tabs, "the tab bar sits before the tabbed panels");
  for (const [file, line] of [["lanes.js", "lanes"], ["history.js", "history"], ["scrub.js", "scrub"]]) {
    assert.match(www(file), new RegExp(`isRailOpen\\("${line}"\\)`), `${file} gates on its tab`);
  }
});

test("the checklist rests folded as one line and expands on click", () => {
  const panel = www("firstrun-panel.js");
  assert.match(panel, /collapsedByUser \?\? true/);
  assert.match(panel, /element\("b", undefined, "Setup"\)/);
  assert.match(panel, /hide.hidden = folded/);
});

test("the Meta badge names the count, from the server or the log", () => {
  assert.equal(metaBadgeText({ serverCalls: 6 }), "Meta · 6 calls");
  assert.equal(metaBadgeText({ serverCalls: 1 }), "Meta · 1 call");
  assert.equal(metaBadgeText({ logCalls: 3 }), "Meta · 3 calls", "no server rows: the log's count");
  assert.equal(metaBadgeText({ error: "404" }), "Meta · not read");
  assert.equal(metaBadgeText({}), "Meta · 0 calls");
  const html = www("index.html");
  assert.match(html, /<button type="button" id="meta-badge"[^>]*aria-controls="usage-meta"><\/button>/);
  assert.match(www("panels.js"), /metaBadge.addEventListener\("click"/);
});

test("the replay bar is a play or pause button and a progress bar, with no sentence", () => {
  const bar = www("index.html").match(/<div class="island" id="replay" hidden>([\s\S]*?)<\/div>/)[1];
  const words = bar.replace(/<[^>]+>/g, " ").replace(/&#\d+;/g, " ").trim();
  assert.equal(words, "", "no visible words");
  assert.match(bar, /<button[^>]*id="replay-toggle"[^>]*aria-label="/);
  assert.match(bar, /<progress[^>]*id="replay-progress"/);
  assert.doesNotMatch(bar, /Restart|speed|Replay of/);
  const js = www("replay.js");
  assert.match(js, /progressEl\.value = String\(playback\.next\)/);
  assert.doesNotMatch(js, /event \$\{playback\.next\} of/);
});
