// Node tests for the settings drawer's data: the sections, the rows, the budget fields, the patch a
// save sends, the 422 field errors, and the mock API the fixture mode runs on. Run with:
//   node --test crates/acan-render-web/tests/js/settings.test.mjs
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

import { providerLabel as setupProviderLabel } from "../../www/setup.js";
import {
  BUDGET_FIELDS,
  SECTIONS,
  THEME_IDS,
  appliedNote,
  clearNote,
  conflictNote,
  hasTokens,
  exportNote,
  fieldErrors,
  getAt,
  harnessRows,
  keyNotes,
  keyRows,
  parseField,
  patchFor,
  providerLabel,
  roleRows,
  sectionsFor,
  setAt,
  testText,
  themeLabel,
  tokenFromRotate,
  updateView,
} from "../../www/settings-model.js";
import { createMockApi } from "../../www/settings-mock.js";

const fixture = JSON.parse(
  await readFile(new URL("../../www/fixtures/settings.json", import.meta.url), "utf8"),
);
const field = (path) => BUDGET_FIELDS.find((entry) => entry.path === path);

const ADAPTERS = [
  { id: "claude_code", name: "Claude Code", installed: true, version: "1.0.42", auth: "logged_in", hint: "" },
  {
    id: "codex",
    name: "Codex CLI",
    installed: true,
    version: "0.31.0",
    auth: "logged_out",
    hint: "Run `codex login`, then Recheck.",
  },
  { id: "grok", name: "Grok Build", installed: true, version: "0.4.1", auth: "unknown", hint: "" },
  {
    id: "cursor",
    name: "Cursor CLI",
    installed: false,
    version: null,
    auth: "not_installed",
    hint: "Install it, then Recheck.",
  },
];

// The sections

test("the nine sections come in the order the drawer shows them", () => {
  assert.deepEqual(
    SECTIONS.map((section) => section.title),
    [
      "Models",
      "Harness",
      "Workers and projects",
      "Budgets and rules",
      "Look and feel",
      "Access",
      "Notifications",
      "Data",
      "About and update",
    ],
  );
});

test("the owner sees the Access section, and a member does not", () => {
  assert.equal(sectionsFor({ you: "owner" }).length, 9);
  assert.ok(sectionsFor(fixture).some((section) => section.id === "access"));
  const memberIds = sectionsFor({ you: "member" }).map((section) => section.id);
  assert.deepEqual(memberIds, [
    "models",
    "harness",
    "projects",
    "budgets",
    "look",
    "notifications",
    "data",
    "about",
  ]);
  assert.deepEqual(sectionsFor(undefined).map((section) => section.id), memberIds);
});

// Themes and labels

test("each theme id has its label, and an unknown id is shown as itself", () => {
  assert.equal(themeLabel("paper"), "Light paper");
  assert.equal(themeLabel("dark"), "Dark");
  assert.equal(themeLabel("blueprint"), "Blueprint");
  assert.equal(themeLabel("high_contrast"), "High contrast");
  assert.equal(themeLabel("sepia"), "sepia");
});

test("the theme ids are the four themes, in order", () => {
  assert.deepEqual(THEME_IDS, ["paper", "dark", "blueprint", "high_contrast"]);
});

test("the provider label is the setup page's own function", () => {
  assert.equal(providerLabel, setupProviderLabel);
  assert.equal(providerLabel("baseten"), "Baseten");
});

// Key and role rows

test("a key row says whether a key is set and where it came from", () => {
  const rows = keyRows(fixture.models);
  assert.deepEqual(rows[0], {
    provider: "meta",
    label: "Meta Model API",
    set: true,
    text: "set",
    source: "saved",
  });
  assert.deepEqual(rows[1], {
    provider: "baseten",
    label: "Baseten",
    set: false,
    text: "not set",
    source: "",
  });
});

test("a key from the environment is named as such", () => {
  const rows = keyRows({ keys: [{ provider: "fireworks", set: true, source: "environment" }] });
  assert.equal(rows[0].source, "from environment");
  assert.equal(rows[0].text, "set");
});

test("no key list gives no key rows", () => {
  assert.deepEqual(keyRows(undefined), []);
});

test("a role row names its provider, and an off role gives the reason", () => {
  const rows = roleRows(fixture.models);
  assert.deepEqual(rows[0], {
    role: "foreman",
    provider: "meta",
    picked: null,
    offReason: null,
    metaOnly: false,
    providerLabel: "Meta Model API",
    text: "Meta Model API",
  });
  const director = rows.find((row) => row.role === "director");
  assert.equal(director.provider, null);
  assert.equal(director.providerLabel, null);
  assert.equal(director.text, "off · needs a key");
});

test("a role row keeps a pick, and marks a meta-only role", () => {
  const rows = roleRows({
    roles: [
      { role: "worker", provider: "baseten", picked: "baseten", off_reason: null, meta_only: false },
      { role: "vision", provider: "meta", picked: null, off_reason: null, meta_only: true },
    ],
  });
  assert.equal(rows[0].picked, "baseten");
  assert.equal(rows[0].text, "Baseten");
  assert.equal(rows[1].metaOnly, true);
});

test("an off role with no reason says only off", () => {
  const [row] = roleRows({
    roles: [{ role: "wonder", provider: null, picked: null, off_reason: null, meta_only: false }],
  });
  assert.equal(row.text, "off");
  assert.equal(row.offReason, null);
});

// Harness rows

test("a harness row's status follows whether it is installed and logged in", () => {
  const statuses = harnessRows(ADAPTERS, {}).map((row) => row.status);
  assert.deepEqual(statuses, ["ready", "log in", "installed", "not installed"]);
});

test("every harness row is quiet, since v1 has no harness failure", () => {
  for (const row of harnessRows(ADAPTERS, {})) assert.equal(row.tone, "quiet");
});

test("a harness row lists the roles that run on it", () => {
  const rows = harnessRows(ADAPTERS, {
    roles: { foreman: "claude_code", worker: "claude_code", director: "builtin" },
  });
  assert.deepEqual(rows[0].selectedRoles, ["foreman", "worker"]);
  assert.deepEqual(rows[1].selectedRoles, []);
});

test("a harness row keeps its details, with the hint as given", () => {
  const [, codex] = harnessRows(ADAPTERS, {});
  assert.deepEqual(codex, {
    id: "codex",
    name: "Codex CLI",
    installed: true,
    auth: "logged_out",
    version: "0.31.0",
    hint: "Run `codex login`, then Recheck.",
    status: "log in",
    tone: "quiet",
    selectedRoles: [],
  });
});

// Budget fields, paths and parsing

test("the budget fields are the ones the page shows, in order", () => {
  assert.deepEqual(
    BUDGET_FIELDS.map((entry) => entry.path),
    [
      "repair_rounds",
      "max_episodes",
      "wall_time_ms",
      "model_cost_micros",
      "call_limits.foreman",
      "call_limits.worker",
      "call_limits.fast",
      "call_limits.vision",
      "call_limits.director",
      "call_limits.wonder",
      "call_limits.statemap",
      "stuck_declare_ms",
      "stuck_build_ms",
      "drift_threshold",
    ],
  );
  assert.deepEqual(field("model_cost_micros"), {
    path: "model_cost_micros",
    label: "Cost cap",
    unit: "$",
    scale: 1000000,
    min: 0,
    max: 1000,
    step: 0.5,
  });
});

test("getAt reads a dotted path, and a missing path gives undefined", () => {
  assert.equal(getAt(fixture.budgets, "call_limits.worker"), 400);
  assert.equal(getAt(fixture.budgets, "call_limits.nope.deeper"), undefined);
});

test("setAt returns a new object with the path set, and leaves the old one alone", () => {
  const before = structuredClone(fixture.budgets);
  const after = setAt(fixture.budgets, "call_limits.worker", 500);
  assert.equal(getAt(after, "call_limits.worker"), 500);
  assert.equal(getAt(after, "call_limits.foreman"), 200);
  assert.deepEqual(fixture.budgets, before);
  assert.notEqual(after, fixture.budgets);
});

test("setAt creates the nested objects a path needs", () => {
  assert.deepEqual(setAt({}, "a.b.c", 1), { a: { b: { c: 1 } } });
});

test("a typed value converts to the stored value by the field's scale", () => {
  assert.deepEqual(parseField(field("wall_time_ms"), "30"), { ok: true, value: 1800000 });
  assert.deepEqual(parseField(field("model_cost_micros"), "5"), { ok: true, value: 5000000 });
  assert.deepEqual(parseField(field("model_cost_micros"), "0.5"), { ok: true, value: 500000 });
  assert.deepEqual(parseField(field("stuck_build_ms"), "15"), { ok: true, value: 900000 });
  assert.deepEqual(parseField(field("drift_threshold"), "0.35"), { ok: true, value: 0.35 });
});

test("a scaled value has no float noise, and the field's spaces are ignored", () => {
  assert.deepEqual(parseField(field("model_cost_micros"), "1.1"), { ok: true, value: 1100000 });
  assert.deepEqual(parseField(field("repair_rounds"), " 7 "), { ok: true, value: 7 });
});

test("a value outside the field's range is refused, with the range as the reason", () => {
  assert.deepEqual(parseField(field("repair_rounds"), "11"), { ok: false, why: "must be 0 to 10" });
  assert.deepEqual(parseField(field("repair_rounds"), "-1"), { ok: false, why: "must be 0 to 10" });
  assert.deepEqual(parseField(field("max_episodes"), "0"), { ok: false, why: "must be 1 to 1000" });
  assert.deepEqual(parseField(field("model_cost_micros"), "1001"), {
    ok: false,
    why: "must be 0 to 1000",
  });
  assert.deepEqual(parseField(field("drift_threshold"), "1.5"), { ok: false, why: "must be 0 to 1" });
});

test("the range ends are inclusive, in the shown unit", () => {
  assert.equal(parseField(field("repair_rounds"), "0").ok, true);
  assert.equal(parseField(field("repair_rounds"), "10").ok, true);
  assert.deepEqual(parseField(field("wall_time_ms"), "1440"), { ok: true, value: 86400000 });
  assert.equal(parseField(field("call_limits.worker"), "10000").ok, true);
  assert.equal(parseField(field("call_limits.worker"), "10001").ok, false);
});

test("a whole-number field refuses a fraction, and a fractional step accepts one", () => {
  assert.deepEqual(parseField(field("repair_rounds"), "2.5"), {
    ok: false,
    why: "must be a whole number",
  });
  assert.deepEqual(parseField(field("wall_time_ms"), "1.5"), {
    ok: false,
    why: "must be a whole number",
  });
  assert.deepEqual(parseField(field("call_limits.fast"), "3.5"), {
    ok: false,
    why: "must be a whole number",
  });
  assert.deepEqual(parseField(field("model_cost_micros"), "2.5"), { ok: true, value: 2500000 });
  assert.equal(parseField(field("drift_threshold"), "0.05").ok, true);
});

test("text that is not a number is refused", () => {
  for (const text of ["", "   ", "abc", "NaN", "Infinity"]) {
    assert.deepEqual(parseField(field("repair_rounds"), text), {
      ok: false,
      why: "must be a number",
    });
  }
});

// Save patches

test("a budget patch holds only the changed leaf, nested the way the budgets are", () => {
  const after = setAt(fixture.budgets, "call_limits.worker", 500);
  assert.deepEqual(patchFor("budgets", fixture.budgets, after), {
    budgets: { call_limits: { worker: 500 } },
  });
});

test("a budget patch with several changes lists each one", () => {
  const after = setAt(setAt(fixture.budgets, "repair_rounds", 4), "call_limits.director", 80);
  assert.deepEqual(patchFor("budgets", fixture.budgets, after), {
    budgets: { repair_rounds: 4, call_limits: { director: 80 } },
  });
});

test("a patch is null when nothing changed", () => {
  const copy = structuredClone(fixture.budgets);
  assert.equal(patchFor("budgets", fixture.budgets, copy), null);
  assert.equal(patchFor("look", fixture.look, structuredClone(fixture.look)), null);
});

test("a look patch holds the changed key under look", () => {
  assert.deepEqual(patchFor("look", fixture.look, { ...fixture.look, theme: "dark" }), {
    look: { theme: "dark" },
  });
});

test("a harness patch holds a changed role, and a changed command as a whole list", () => {
  const before = { roles: { worker: "builtin" }, custom_command: [] };
  const after = { roles: { worker: "claude_code" }, custom_command: ["mcp", "--flag"] };
  assert.deepEqual(patchFor("harness", before, after), {
    harness: { roles: { worker: "claude_code" }, custom_command: ["mcp", "--flag"] },
  });
});

test("a patch also accepts the whole settings object", () => {
  const after = { ...fixture, look: { ...fixture.look, label_density: "dense" } };
  assert.deepEqual(patchFor("look", fixture, after), { look: { label_density: "dense" } });
});

// Answers and notes

test("a 422's fields become a map from path to reason", () => {
  const errors = fieldErrors([
    { path: "budgets.repair_rounds", why: "must be 0 to 10" },
    { path: "look.theme", why: "unknown theme" },
  ]);
  assert.ok(errors instanceof Map);
  assert.equal(errors.size, 2);
  assert.equal(errors.get("budgets.repair_rounds"), "must be 0 to 10");
  assert.equal(errors.get("look.theme"), "unknown theme");
  assert.equal(fieldErrors(undefined).size, 0);
});

test("a rotate answer gives its token, and nothing when the token is missing or empty", () => {
  assert.equal(tokenFromRotate({ token: "mock-owner-1" }), "mock-owner-1");
  assert.equal(tokenFromRotate({}), null);
  assert.equal(tokenFromRotate({ token: "" }), null);
  assert.equal(tokenFromRotate({ token: "  " }), null);
  assert.equal(tokenFromRotate(undefined), null);
});

test("a passing Test reads as its latency", () => {
  const result = { role: "foreman", ok: true, provider: "meta", model: "m", ms: 412, error: null };
  assert.deepEqual(testText(result), { ok: true, text: "ok · 412 ms" });
});

test("a failed Test reads as its error, or as failed when it gave none", () => {
  assert.deepEqual(testText({ ok: false, error: "no key for baseten" }), {
    ok: false,
    text: "no key for baseten",
  });
  assert.deepEqual(testText({ ok: false, error: null }), { ok: false, text: "failed" });
});

test("each key state has its sentence, and an unknown state has none", () => {
  assert.deepEqual(
    keyNotes({ meta: "stored", baseten: "rejected", fireworks: "unreachable" }),
    ["Key saved.", "Key rejected by the provider.", "Provider unreachable; key kept."],
  );
  assert.deepEqual(keyNotes({ meta: "mystery" }), [""]);
  assert.deepEqual(keyNotes(undefined), []);
});

test("a change that needs a restart says so, and live or client changes say nothing", () => {
  assert.equal(appliedNote({ budgets: "live", look: "client" }), "");
  assert.equal(appliedNote({ projects: "restart", budgets: "live" }), "Saved; takes effect after restart.");
  assert.equal(appliedNote({ budgets: "stored" }), "Saved.");
  assert.equal(appliedNote({ harness: "stored", projects: "restart" }), "Saved; takes effect after restart.");
  assert.equal(appliedNote({}), "");
});

test("the update view reads up to date when latest is the current version", () => {
  assert.deepEqual(updateView({ current: "0.9.3-pre.4", latest: "0.9.3-pre.4" }), {
    state: "current",
    text: "Up to date · 0.9.3-pre.4",
  });
});

test("the update view offers a newer latest version", () => {
  assert.deepEqual(updateView({ current: "0.9.3-pre.4", latest: "0.9.4-pre.1" }), {
    state: "newer",
    text: "0.9.4-pre.1 available",
  });
});

// The mock API

test("the owner's read is the fixture's settings, with the tokens", async () => {
  const { status, body } = await createMockApi(fixture).request("GET", "/api/settings");
  assert.equal(status, 200);
  assert.equal(body.you, "owner");
  assert.deepEqual(body.tokens, fixture.tokens);
  assert.deepEqual(body.budgets, fixture.budgets);
});

test("the mock keeps its own copy, so a caller changing an answer does not reach it", async () => {
  const api = createMockApi(fixture);
  const first = await api.request("GET", "/api/settings");
  first.body.budgets.repair_rounds = 99;
  const again = await api.request("GET", "/api/settings");
  assert.equal(again.body.budgets.repair_rounds, 3);
  assert.equal(fixture.budgets.repair_rounds, 3);
});

test("a valid budget write is saved, reported live, and read back", async () => {
  const api = createMockApi(fixture);
  const put = await api.request("PUT", "/api/settings", {
    budgets: { call_limits: { worker: 500 } },
  });
  assert.equal(put.status, 200);
  assert.deepEqual(put.body.applied, { budgets: "stored" });
  assert.deepEqual(put.body.keys, {});
  assert.equal(put.body.settings.budgets.call_limits.worker, 500);
  assert.equal(put.body.settings.budgets.call_limits.foreman, 200);
  const read = await api.request("GET", "/api/settings");
  assert.equal(read.body.budgets.call_limits.worker, 500);
});

test("a budget out of range is a 422 that names the field, and changes nothing", async () => {
  const api = createMockApi(fixture);
  const bad = await api.request("PUT", "/api/settings", { budgets: { repair_rounds: 11 } });
  assert.equal(bad.status, 422);
  assert.deepEqual(bad.body, {
    error: "invalid",
    fields: [{ path: "budgets.repair_rounds", why: "must be 0 to 10" }],
  });
  assert.deepEqual((await api.request("GET", "/api/settings")).body.budgets, fixture.budgets);
});

test("a whole-number budget with a fraction is a 422", async () => {
  const bad = await createMockApi(fixture).request("PUT", "/api/settings", {
    budgets: { repair_rounds: 2.5 },
  });
  assert.equal(bad.status, 422);
  assert.deepEqual(bad.body.fields, [
    { path: "budgets.repair_rounds", why: "must be a whole number" },
  ]);
});

test("a 422 changes nothing, not even the valid parts of the same write", async () => {
  const api = createMockApi(fixture);
  const bad = await api.request("PUT", "/api/settings", {
    look: { theme: "dark" },
    budgets: { max_episodes: 0 },
    models: { keys: { baseten: { key: "sk-test" } } },
  });
  assert.equal(bad.status, 422);
  const read = (await api.request("GET", "/api/settings")).body;
  assert.equal(read.look.theme, "paper");
  assert.equal(read.budgets.max_episodes, 12);
  assert.equal(read.models.keys.find((key) => key.provider === "baseten").set, false);
});

test("an unknown theme or label density is a 422 that names the field", async () => {
  const api = createMockApi(fixture);
  const theme = await api.request("PUT", "/api/settings", { look: { theme: "sepia" } });
  assert.deepEqual(theme.body.fields, [{ path: "look.theme", why: "unknown theme" }]);
  const density = await api.request("PUT", "/api/settings", { look: { label_density: "bogus" } });
  assert.equal(density.status, 422);
  assert.deepEqual(density.body.fields, [
    { path: "look.label_density", why: "unknown label density" },
  ]);
});

test("look and notification writes are client changes, and they persist", async () => {
  const api = createMockApi(fixture);
  const put = await api.request("PUT", "/api/settings", {
    look: { theme: "dark" },
    notifications: { sound: true },
  });
  assert.deepEqual(put.body.applied, { look: "client", notifications: "client" });
  const read = (await api.request("GET", "/api/settings")).body;
  assert.equal(read.look.theme, "dark");
  assert.equal(read.notifications.sound, true);
  assert.equal(read.notifications.needs_you, true);
});

test("a key is stored without being returned, and the answer says it was stored", async () => {
  const api = createMockApi(fixture);
  const put = await api.request("PUT", "/api/settings", {
    models: { keys: { baseten: { key: "sk-secret-123" } } },
  });
  assert.equal(put.status, 200);
  assert.deepEqual(put.body.keys, { baseten: "stored" });
  assert.deepEqual(put.body.applied, { models: "live" });
  assert.deepEqual(
    put.body.settings.models.keys.find((key) => key.provider === "baseten"),
    { provider: "baseten", set: true, source: "saved" },
  );
  assert.equal(JSON.stringify(put.body).includes("sk-secret-123"), false);
  const read = await api.request("GET", "/api/settings");
  assert.equal(JSON.stringify(read.body).includes("sk-secret-123"), false);
});

test("an empty key is a 422 on that provider", async () => {
  const bad = await createMockApi(fixture).request("PUT", "/api/settings", {
    models: { keys: { meta: { key: "  " } } },
  });
  assert.equal(bad.status, 422);
  assert.deepEqual(bad.body.fields, [{ path: "models.keys.meta", why: "empty key" }]);
});

test("a cleared key is unset and reports no stored key", async () => {
  const put = await createMockApi(fixture).request("PUT", "/api/settings", {
    models: { keys: { meta: { clear: true } } },
  });
  assert.equal(put.status, 200);
  assert.deepEqual(put.body.keys, {});
  assert.deepEqual(
    put.body.settings.models.keys.find((key) => key.provider === "meta"),
    { provider: "meta", set: false, source: null },
  );
});

test("a role may take a provider with a key, and off gives the reason off", async () => {
  const api = createMockApi(fixture);
  const refused = await api.request("PUT", "/api/settings", { models: { roles: { foreman: "baseten" } } });
  assert.equal(refused.status, 409);
  assert.equal(refused.body.message, "foreman needs a baseten key. Add one first.");
  const picked = await api.request("PUT", "/api/settings", {
    models: { keys: { baseten: { key: "k" } }, roles: { foreman: "baseten" } },
  });
  assert.equal(picked.status, 200);
  const foreman = picked.body.settings.models.roles.find((role) => role.role === "foreman");
  assert.equal(foreman.provider, "baseten");
  assert.equal(foreman.picked, "baseten");
  assert.equal(foreman.off_reason, null);

  const off = await api.request("PUT", "/api/settings", { models: { roles: { foreman: null } } });
  const cleared = off.body.settings.models.roles.find((role) => role.role === "foreman");
  assert.equal(cleared.provider, null);
  assert.equal(cleared.off_reason, "off");
});

test("a provider added in the same write as a role counts as on the keys list", async () => {
  const put = await createMockApi(fixture).request("PUT", "/api/settings", {
    models: { keys: { openrouter: { key: "k" } }, roles: { worker: "openrouter" } },
  });
  assert.equal(put.status, 200);
  assert.equal(put.body.settings.models.roles.find((role) => role.role === "worker").provider, "openrouter");
});

test("a role naming a provider off the keys list is a 422, and the key in the write is not kept", async () => {
  const api = createMockApi(fixture);
  const bad = await api.request("PUT", "/api/settings", {
    models: { keys: { baseten: { key: "k" } }, roles: { foreman: "nope" } },
  });
  assert.equal(bad.status, 422);
  assert.deepEqual(bad.body.fields, [{ path: "models.roles.foreman", why: "unknown provider" }]);
  const read = (await api.request("GET", "/api/settings")).body;
  assert.equal(read.models.keys.find((key) => key.provider === "baseten").set, false);
  assert.equal(read.models.roles.find((role) => role.role === "foreman").provider, "meta");
});

test("a harness role is stored, and an unknown harness is a 422", async () => {
  const api = createMockApi(fixture);
  const put = await api.request("PUT", "/api/settings", {
    harness: { roles: { worker: "claude_code" } },
  });
  assert.equal(put.status, 200);
  assert.deepEqual(put.body.applied, { harness: "stored" });
  assert.equal(put.body.settings.harness.roles.worker, "claude_code");
  assert.equal(put.body.settings.harness.roles.foreman, "builtin");
  const bad = await api.request("PUT", "/api/settings", { harness: { roles: { worker: "vim" } } });
  assert.deepEqual(bad.body.fields, [{ path: "harness.roles.worker", why: "unknown harness" }]);
});

test("projects are added and removed, and the answer says they apply on restart", async () => {
  const put = await createMockApi(fixture).request("PUT", "/api/settings", {
    projects: { add: ["repos/new-thing/"], remove: ["acan-web"] },
  });
  assert.equal(put.status, 200);
  assert.deepEqual(put.body.applied, { projects: "restart" });
  assert.deepEqual(put.body.settings.projects.map((project) => project.id), ["acan", "new-thing"]);
});

test("a member sees no tokens, reads everything as not editable, and cannot write", async () => {
  const api = createMockApi({ ...fixture, you: "member" });
  const read = await api.request("GET", "/api/settings");
  assert.equal(read.status, 200);
  assert.equal(read.body.you, "member");
  assert.equal(read.body.editable, false);
  assert.equal("tokens" in read.body, false);

  assert.equal((await api.request("PUT", "/api/settings", { look: { theme: "dark" } })).status, 403);
  assert.equal((await api.request("GET", "/api/settings/tokens/owner")).status, 403);
  assert.equal((await api.request("POST", "/api/settings/tokens/owner/rotate")).status, 403);
  assert.equal((await api.request("POST", "/api/settings/tokens/agents/rotate")).status, 403);
  assert.equal((await api.request("POST", "/api/settings/models/roles/foreman/test")).status, 403);
  assert.equal((await api.request("POST", "/api/update")).status, 403);
  assert.equal((await api.request("GET", "/api/settings")).body.look.theme, "paper");
});

test("rotating the owner token changes it and its masked form", async () => {
  const api = createMockApi(fixture);
  const before = await api.request("GET", "/api/settings/tokens/owner");
  assert.equal(before.body.token, "ac_own_mock7d2e");

  const rotated = await api.request("POST", "/api/settings/tokens/owner/rotate");
  assert.equal(rotated.status, 200);
  assert.equal(rotated.body.token, "mock-owner-1");
  assert.equal((await api.request("GET", "/api/settings/tokens/owner")).body.token, "mock-owner-1");

  const settings = (await api.request("GET", "/api/settings")).body;
  assert.equal(settings.tokens.owner, "ac_own_••••••er-1");
  assert.notEqual(settings.tokens.owner, fixture.tokens.owner);

  const again = await api.request("POST", "/api/settings/tokens/owner/rotate");
  assert.equal(again.body.token, "mock-owner-2");
});

test("rotating the agent tokens changes each masked form", async () => {
  const api = createMockApi(fixture);
  const rotated = await api.request("POST", "/api/settings/tokens/agents/rotate");
  assert.equal(rotated.status, 200);
  assert.equal(rotated.body.rotated, 2);
  const { tokens } = (await api.request("GET", "/api/settings")).body;
  assert.deepEqual(tokens.agents.map((agent) => agent.id), ["agent-a", "agent-b"]);
  for (const agent of tokens.agents) assert.match(agent.masked, /^ac_agt_••••••.{4}$/u);
  assert.notEqual(tokens.agents[0].masked, fixture.tokens.agents[0].masked);
});

test("a role test answers ok with its latency, and gives the reason when it cannot run", async () => {
  const api = createMockApi(fixture);
  const ok = await api.request("POST", "/api/settings/models/roles/foreman/test");
  assert.equal(ok.status, 200);
  assert.equal(ok.body.ok, true);
  assert.equal(ok.body.provider, "meta");
  assert.deepEqual(testText(ok.body), { ok: true, text: "ok · 412 ms" });

  const off = await api.request("POST", "/api/settings/models/roles/director/test");
  assert.deepEqual(testText(off.body), { ok: false, text: "needs a key" });

  // A role may not point at a provider with no key, so the key is saved, the role picked, then cleared.
  await api.request("PUT", "/api/settings", { models: { keys: { baseten: { key: "k" } }, roles: { foreman: "baseten" } } });
  await api.request("PUT", "/api/settings", { models: { keys: { baseten: { clear: true } } } });
  const unkeyed = await api.request("POST", "/api/settings/models/roles/foreman/test");
  assert.deepEqual(testText(unkeyed.body), { ok: false, text: "no key for baseten" });
});

test("the adapter list holds the nine harnesses, and its rows read as the drawer shows them", async () => {
  const { status, body } = await createMockApi(fixture).request(
    "GET",
    "/api/settings/harness/adapters",
  );
  assert.equal(status, 200);
  assert.deepEqual(body.adapters.map((adapter) => adapter.id), [
    "builtin",
    "claude_code",
    "codex",
    "cursor",
    "grok",
    "opencode",
    "antigravity",
    "fx",
    "custom",
  ]);
  assert.match(body.adapters.find((adapter) => adapter.id === "codex").hint, /codex login/);
  const statuses = harnessRows(body.adapters, {}).map((row) => row.status);
  assert.deepEqual(statuses, [
    "ready",
    "ready",
    "log in",
    "not installed",
    "installed",
    "installed",
    "not installed",
    "ready",
    "installed",
  ]);
});

test("the update check compares the versions, and an update starts by verifying", async () => {
  const api = createMockApi(fixture);
  const check = await api.request("GET", "/api/update");
  assert.equal(check.body.current, "0.9.3-pre.4");
  assert.deepEqual(updateView(check.body), { state: "newer", text: "0.9.4-pre.1 available" });
  assert.deepEqual(await api.request("POST", "/api/update"), {
    status: 200,
    body: { state: "verifying" },
  });
});

test("a route the mock does not know is a 404", async () => {
  const { status } = await createMockApi(fixture).request("GET", "/api/nowhere");
  assert.equal(status, 404);
});

test("an export that downloads says so, and a 409 with no run gives the reason", () => {
  assert.deepEqual(exportNote(200, null), { bad: false, text: "Export downloaded." });
  assert.deepEqual(exportNote(409, { error: "conflict", what: "no_recorded_run" }), {
    bad: false,
    text: "No recorded run yet. Receipts still export.",
  });
});

test("an export refused for an owner or a network reason says nothing was changed", () => {
  assert.deepEqual(exportNote(403, null), { bad: true, text: "Only the owner can do this. Nothing changed." });
  assert.deepEqual(exportNote(0, null), { bad: true, text: "Could not reach acan. Nothing changed." });
  assert.deepEqual(exportNote(500, null), { bad: true, text: "Not exported. Nothing changed." });
});

test("an export on a server without the route is neutral, not a failure", () => {
  assert.deepEqual(exportNote(404, null), { bad: false, text: "Not on this server yet." });
});

test("a clear says what was cleared and what was kept", () => {
  assert.deepEqual(clearNote(200, { cleared: ["receipts"], kept: ["keys", "settings"] }), {
    bad: false,
    text: "Cleared: receipts. Kept: keys, settings.",
  });
  assert.deepEqual(clearNote(200, {}), { bad: false, text: "Cleared: nothing. Kept: nothing." });
  assert.deepEqual(clearNote(400, null), { bad: true, text: "Not cleared. Nothing changed." });
});

test("the mock's export is a zip on 200, and 409 for a replay before any run", async () => {
  const api = createMockApi(fixture);
  const receipts = await api.download("/api/settings/data/export?what=receipts");
  assert.equal(receipts.status, 200);
  assert.equal(receipts.blob.type, "application/zip");
  const replay = await api.download("/api/settings/data/export?what=replay");
  assert.deepEqual(replay, { status: 409, body: { error: "conflict", what: "no_recorded_run" } });
  assert.equal((await api.download("/api/settings/data/export?what=nope")).status, 400);
  assert.equal((await api.download("/api/p/acan/settings/data/export?what=all")).status, 409);
});

test("the mock's clear needs confirm true and says what it kept", async () => {
  const api = createMockApi(fixture);
  assert.equal((await api.request("POST", "/api/settings/data/clear", { what: "local" })).status, 400);
  assert.equal((await api.request("POST", "/api/settings/data/clear", { what: "local", confirm: "yes" })).status, 400);
  const done = await api.request("POST", "/api/p/acan/settings/data/clear", { what: "local", confirm: true });
  assert.equal(done.status, 200);
  assert.deepEqual(done.body.cleared, ["receipts", "replays"]);
  assert.ok(done.body.kept.includes("keys"));
});

test("a member cannot export or clear", async () => {
  const api = createMockApi({ ...fixture, you: "member" });
  assert.equal((await api.download("/api/settings/data/export?what=receipts")).status, 403);
  assert.equal((await api.request("POST", "/api/settings/data/clear", { what: "local", confirm: true })).status, 403);
});

test("a 409 shows the server's message, or a plain line when it sends none", () => {
  assert.equal(conflictNote({ error: "conflict", message: "meta comes from the environment. Unset it there." }), "meta comes from the environment. Unset it there.");
  assert.equal(conflictNote({ message: "  " }), "Not saved: a model setting conflicts. Nothing changed.");
  assert.equal(conflictNote(null), "Not saved: a model setting conflicts. Nothing changed.");
});

test("tokens count as there only once the owner token is in the answer", () => {
  assert.equal(hasTokens({}), false);
  assert.equal(hasTokens(undefined), false);
  assert.equal(hasTokens({ owner: "" }), false);
  assert.equal(hasTokens(fixture.tokens), true);
});

test("the fixture has the statemap role, the statemap call limit and the project flag, all with defaults", () => {
  assert.ok(fixture.models.roles.some((role) => role.role === "statemap"));
  assert.equal(fixture.harness.roles.statemap, "builtin");
  assert.equal(getAt(fixture.budgets, "call_limits.statemap"), 30);
  assert.ok(fixture.projects.every((project) => project.statemap_llm === false));
});

test("a statemap call limit is saved as a budget and reported stored", async () => {
  const put = await createMockApi(fixture).request("PUT", "/api/settings", {
    budgets: { call_limits: { statemap: 40 } },
  });
  assert.equal(put.status, 200);
  assert.deepEqual(put.body.applied, { budgets: "stored" });
  assert.equal(getAt(put.body.settings.budgets, "call_limits.statemap"), 40);
});

test("a project's statemap flag is saved on or off, and reported as a restart", async () => {
  const api = createMockApi(fixture);
  const on = await api.request("PUT", "/api/settings", { projects: { statemap_llm: { acan: true } } });
  assert.equal(on.status, 200);
  assert.deepEqual(on.body.applied, { projects: "restart" });
  assert.equal(on.body.settings.projects.find((project) => project.id === "acan").statemap_llm, true);
  assert.equal(on.body.settings.projects.find((project) => project.id === "acan-web").statemap_llm, false);
});

test("a statemap flag for an unknown project, or not on or off, is a 422 on that field", async () => {
  const api = createMockApi(fixture);
  const unknown = await api.request("PUT", "/api/settings", { projects: { statemap_llm: { nope: true } } });
  assert.equal(unknown.status, 422);
  assert.deepEqual(unknown.body.fields, [{ path: "projects.nope", why: "unknown project" }]);
  const text = await api.request("PUT", "/api/settings", { projects: { statemap_llm: { acan: "yes" } } });
  assert.deepEqual(text.body.fields, [{ path: "projects.acan.statemap_llm", why: "must be on or off" }]);
});

test("clearing an environment key is a 409 with the reason, and changes nothing", async () => {
  const fromEnv = {
    ...fixture,
    models: {
      ...fixture.models,
      keys: fixture.models.keys.map((key) => (key.provider === "meta" ? { ...key, source: "environment" } : key)),
    },
  };
  const api = createMockApi(fromEnv);
  const refused = await api.request("PUT", "/api/settings", { models: { keys: { meta: { clear: true } } } });
  assert.equal(refused.status, 409);
  assert.equal(refused.body.message, "meta comes from the environment. Unset it there.");
  const read = (await api.request("GET", "/api/settings")).body;
  assert.equal(read.models.keys.find((key) => key.provider === "meta").set, true);
});
