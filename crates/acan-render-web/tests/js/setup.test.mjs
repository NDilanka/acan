// Node tests for the model key setup's page logic, against the shapes the server sends for
// GET /api/setup and its key and role routes. Run with:
//   node --test crates/acan-render-web/tests/js/setup.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  bannerText,
  keyRows,
  pickChoices,
  providerChoices,
  providerLabel,
  removeOutcome,
  roleOutcome,
  roleRows,
  saveOutcome,
  showBanner,
  sourceText,
  statusText,
} from "../../www/setup.js";

// The example answer for GET /api/setup: one saved key, one from the environment, vision off.
const SET = {
  configured: true,
  source: "setup",
  can_save: true,
  providers: [
    { name: "meta", saved: false, environment: false, source: null },
    { name: "baseten", saved: true, environment: false, source: "saved" },
    { name: "fireworks", saved: false, environment: true, source: "environment" },
  ],
  roles: [
    { role: "foreman", provider: "baseten", picked: false, off_reason: null },
    { role: "worker", provider: "fireworks", picked: true, off_reason: null },
    { role: "fast", provider: "baseten", picked: false, off_reason: null },
    { role: "vision", provider: null, picked: false, off_reason: "it needs a Meta key" },
  ],
};

const NONE = {
  configured: false,
  source: null,
  can_save: true,
  providers: [
    { name: "meta", saved: false, environment: false, source: null },
    { name: "baseten", saved: false, environment: false, source: null },
    { name: "fireworks", saved: false, environment: false, source: null },
  ],
  roles: ["foreman", "worker", "fast", "vision"].map((role) => ({
    role,
    provider: null,
    picked: false,
    off_reason: role === "vision" ? "it needs a Meta key" : "no model key is set",
  })),
};

const ROUTING = {
  configured: true,
  source: "routing_file",
  can_save: false,
  providers: [
    { name: "meta", saved: false, environment: true, source: "environment" },
    { name: "baseten", saved: false, environment: true, source: "environment" },
    { name: "fireworks", saved: false, environment: false, source: null },
  ],
  roles: [
    { role: "foreman", provider: "baseten", picked: false, off_reason: null },
    { role: "vision", provider: null, picked: false, off_reason: "it needs a Meta Muse Spark route" },
  ],
};

// Roles the page has never heard of still get a row each, in the server's order.
const LATER = {
  ...SET,
  roles: [
    ...SET.roles,
    { role: "director", provider: null, picked: false, off_reason: "it needs a Meta key" },
    { role: "wonder", provider: "meta", picked: true, off_reason: null },
  ],
};

const SHAPES = /[{}[\]"]|HTTP|\b[45]\d\d\b|_/;

test("a provider has a label, and an unknown name is shown as its own name", () => {
  assert.equal(providerLabel("meta"), "Meta Model API");
  assert.equal(providerLabel("baseten"), "Baseten");
  assert.equal(providerLabel("someday"), "someday");
});

test("the key form offers every provider the server lists, in its order", () => {
  assert.deepEqual(providerChoices(SET), [
    { value: "meta", label: "Meta Model API" },
    { value: "baseten", label: "Baseten" },
    { value: "fireworks", label: "Fireworks" },
  ]);
  assert.deepEqual(providerChoices(null), []);
  assert.deepEqual(providerChoices({ providers: [{}, { name: "" }, null] }), []);
});

test("each provider shows where its key comes from", () => {
  assert.deepEqual(
    keyRows(SET).map(({ provider, source, text }) => [provider, source, text]),
    [
      ["meta", null, "No key."],
      ["baseten", "saved", "Saved on this page."],
      ["fireworks", "environment", "Set in the environment. Unset it there to remove it."],
    ],
  );
  assert.equal(sourceText("something else"), "No key.");
});

test("only a key saved on this page can be removed, and never with a routing file", () => {
  assert.deepEqual(
    keyRows(SET).map(({ provider, removable }) => [provider, removable]),
    [
      ["meta", false],
      ["baseten", true],
      ["fireworks", false],
    ],
  );
  const savedInRouting = { ...SET, can_save: false };
  assert.ok(keyRows(savedInRouting).every((row) => row.removable === false));
  assert.ok(keyRows(ROUTING).every((row) => row.removable === false));
});

test("a role can be given any provider with a key, from either source", () => {
  assert.deepEqual(pickChoices(SET), [
    { value: "baseten", label: "Baseten" },
    { value: "fireworks", label: "Fireworks" },
  ]);
  assert.deepEqual(pickChoices(NONE), []);
});

test("the roles are the ones the server lists, in its order", () => {
  assert.deepEqual(
    roleRows(SET).map((row) => row.role),
    ["foreman", "worker", "fast", "vision"],
  );
  assert.deepEqual(
    roleRows(LATER).map((row) => row.role),
    ["foreman", "worker", "fast", "vision", "director", "wonder"],
  );
  assert.deepEqual(roleRows({ ...SET, roles: [] }), []);
  assert.deepEqual(roleRows(null), []);
});

test("a role says what serves it now, and whether that is its default or a pick", () => {
  const rows = roleRows(SET);
  assert.equal(rows[0].now, "Baseten, by default");
  assert.equal(rows[0].value, "");
  assert.equal(rows[0].off, false);
  assert.equal(rows[1].now, "Fireworks, picked");
  assert.equal(rows[1].value, "fireworks");
  assert.deepEqual(
    rows[0].options.map((option) => option.value),
    ["", "baseten", "fireworks"],
  );
  assert.equal(rows[0].options[0].label, "Default");
});

test("an off role shows the server's reason as given", () => {
  const vision = roleRows(SET)[3];
  assert.equal(vision.now, "Off: it needs a Meta key");
  assert.equal(vision.off, true);
  assert.equal(roleRows(ROUTING)[1].now, "Off: it needs a Meta Muse Spark route");
  assert.equal(roleRows(NONE)[0].now, "Off: no model key is set");
  assert.equal(roleRows(LATER)[4].now, "Off: it needs a Meta key");
  assert.equal(roleRows({ roles: [{ role: "fast", provider: null, picked: false, off_reason: null }] })[0].now, "Off");
});

test("a pick the server holds stays in the picker, even for a provider with no key listed", () => {
  const wonder = roleRows(LATER)[5];
  assert.equal(wonder.value, "meta");
  assert.ok(wonder.options.some((option) => option.value === "meta" && option.label === "Meta Model API"));
});

test("the status line names the providers in use, and no key says so in one sentence", () => {
  assert.deepEqual(statusText(SET), {
    text: "Model calls use Baseten and Fireworks. Each role's provider is below.",
    set: true,
  });
  assert.deepEqual(statusText(NONE), {
    text: "No model key is set. Model jobs get built-in replies until you add one.",
    set: false,
  });
  assert.deepEqual(statusText({ configured: true, source: "setup", roles: [] }), { text: "Model calls are set up.", set: true });
});

test("a routing file's status says the repository decides, and whether its keys are set", () => {
  assert.equal(statusText(ROUTING).set, true);
  assert.match(statusText(ROUTING).text, /\.acan\/llm\.toml/);
  const noKey = { ...ROUTING, configured: false };
  assert.equal(statusText(noKey).set, false);
  assert.match(statusText(noKey).text, /\.env/);
});

test("the banner shows whenever no key is in use, and says why when a routing file decides", () => {
  assert.equal(showBanner(NONE), true);
  assert.equal(showBanner(SET), false);
  assert.equal(showBanner({ ...NONE, can_save: false }), true);
  assert.equal(showBanner(null), false);
  assert.match(bannerText(NONE), /No model key is set/);
  assert.match(bannerText({ ...NONE, can_save: false }), /\.acan\/llm\.toml.*\.env/);
});

test("a saved key is confirmed in one sentence, and an unconfirmed one warns", () => {
  assert.deepEqual(saveOutcome(200, { result: "stored", provider: "meta", checked: true }), {
    tone: "ok",
    text: "Saved. Meta Model API accepted the key, and acan will use it from now on.",
  });
  assert.equal(saveOutcome(200, { result: "stored", provider: "meta", checked: false }).tone, "warn");
});

test("a refused key, an unreachable provider and a refused session each say nothing was saved", () => {
  const refused = saveOutcome(422, { result: "rejected", provider: "baseten" });
  assert.equal(refused.tone, "bad");
  assert.match(refused.text, /did not accept this key.*Nothing was saved/);
  assert.match(saveOutcome(503, { result: "unreachable", provider: "meta" }).text, /Could not reach Meta Model API.*Nothing was saved/);
  assert.match(saveOutcome(403, { error: "forbidden" }).text, /Only members can set a key/);
  assert.match(saveOutcome(409, { error: "conflict", what: "routing_file" }).text, /\.acan\/llm\.toml/);
  assert.match(saveOutcome(409, { error: "conflict", what: "unavailable" }).text, /cannot save a key/);
});

test("a malformed save and any other failure are one plain sentence, with no code shown", () => {
  for (const status of [400, 401, 500, 0]) {
    const outcome = saveOutcome(status, null);
    assert.equal(outcome.tone, "bad", String(status));
    assert.doesNotMatch(outcome.text, SHAPES, String(status));
  }
});

test("a remove names the provider, and says when there was nothing to remove", () => {
  assert.deepEqual(removeOutcome(200, { provider: "baseten", removed: true }, "baseten"), {
    tone: "ok",
    text: "The Baseten key is removed. Roles that were picked for it take their default again.",
  });
  assert.deepEqual(removeOutcome(200, { provider: "baseten", removed: false }, "baseten"), {
    tone: "ok",
    text: "Baseten had no saved key, so nothing was removed.",
  });
});

test("a remove that the environment blocks shows the server's own words, as one sentence", () => {
  const outcome = removeOutcome(409, { error: "conflict", what: "this key comes from the environment" }, "fireworks");
  assert.deepEqual(outcome, { tone: "bad", text: "This key comes from the environment." });
});

test("a remove refused for another reason is one plain sentence, never a code", () => {
  assert.match(removeOutcome(409, { error: "conflict", what: "routing_file" }, "meta").text, /\.acan\/llm\.toml/);
  assert.equal(removeOutcome(409, { error: "conflict", what: "unavailable" }, "meta").text, "This acan cannot remove a key from this page.");
  assert.equal(removeOutcome(409, { error: "conflict", what: '{"raw":"json"}' }, "meta").text, "This acan cannot remove a key from this page.");
  assert.equal(removeOutcome(409, { error: "conflict", what: "x".repeat(200) + " y" }, "meta").text, "This acan cannot remove a key from this page.");
  assert.match(removeOutcome(403, { error: "forbidden" }, "meta").text, /Only members can remove a key/);
  for (const status of [400, 401, 403, 409, 500, 0]) {
    const outcome = removeOutcome(status, null, "meta");
    assert.equal(outcome.tone, "bad", String(status));
    assert.doesNotMatch(outcome.text, SHAPES, String(status));
  }
});

test("a role pick is confirmed in one sentence, and clearing it says the role takes its default", () => {
  assert.deepEqual(roleOutcome(200, { role: "worker", provider: "baseten" }, "worker", "baseten"), {
    tone: "ok",
    text: "Worker now uses Baseten.",
  });
  assert.deepEqual(roleOutcome(200, { role: "worker", provider: null }, "worker", null), {
    tone: "ok",
    text: "Worker takes its default again.",
  });
  assert.equal(roleOutcome(200, { role: "director", provider: "meta" }, "director", "meta").text, "Director now uses Meta Model API.");
});

test("a refused pick says why in one sentence, naming the provider asked for", () => {
  assert.equal(
    roleOutcome(400, { error: "bad_request", what: "needs_meta" }, "vision", "baseten").text,
    "Vision runs on Meta Model API only, so pick it or leave the default.",
  );
  assert.equal(
    roleOutcome(409, { error: "conflict", what: "no_key" }, "worker", "fireworks").text,
    "Fireworks has no key yet. Save a key for it first.",
  );
  assert.match(roleOutcome(409, { error: "conflict", what: "routing_file" }, "worker", "meta").text, /\.acan\/llm\.toml/);
  assert.match(roleOutcome(404, { error: "not_found", what: "role" }, "someday", "meta").text, /no role called someday/);
  assert.match(roleOutcome(403, { error: "forbidden" }, "worker", "meta").text, /Only members can pick a provider/);
  for (const status of [400, 401, 403, 409, 500, 0]) {
    const outcome = roleOutcome(status, null, "worker", "meta");
    assert.equal(outcome.tone, "bad", String(status));
    assert.doesNotMatch(outcome.text, SHAPES, String(status));
  }
});

test("no message echoes an answer's fields beyond the provider's name, or a key", () => {
  const extra = { key: "placeholder-not-shown", result: "rejected-raw", detail: "detail-not-shown" };
  const texts = [
    saveOutcome(422, { ...extra, provider: "meta" }).text,
    saveOutcome(200, { ...extra, provider: "meta", checked: true }).text,
    removeOutcome(200, { ...extra, provider: "meta", removed: true }, "meta").text,
    removeOutcome(409, { ...extra, error: "conflict-raw", what: "unavailable" }, "meta").text,
    roleOutcome(200, { ...extra, role: "role-not-shown", provider: "meta" }, "worker", "meta").text,
    roleOutcome(409, { ...extra, error: "conflict-raw", what: "no_key" }, "worker", "meta").text,
  ];
  for (const text of texts) {
    assert.doesNotMatch(text, /placeholder-not-shown|rejected-raw|detail-not-shown|conflict-raw|role-not-shown/);
  }
  for (const row of [...keyRows(SET), ...roleRows(SET)]) {
    assert.doesNotMatch(JSON.stringify(row), /placeholder-not-shown/);
  }
});

// The same answer from a server that marks the roles that may use only Meta.
const MARKED = {
  ...SET,
  roles: SET.roles.map((role) => ({ ...role, meta_only: role.role === "vision" })),
};
const MARKED_META = {
  ...MARKED,
  providers: MARKED.providers.map((provider) =>
    provider.name === "meta" ? { ...provider, saved: true, source: "saved" } : provider,
  ),
};

test("a meta_only role with a Meta key is offered Default and Meta only", () => {
  const vision = roleRows(MARKED_META).find((row) => row.role === "vision");
  assert.equal(vision.picker, true);
  assert.equal(vision.note, null);
  assert.deepEqual(vision.options, [
    { value: "", label: "Default" },
    { value: "meta", label: "Meta Model API" },
  ]);
});

test("a meta_only role with no Meta key gets one sentence and no picker", () => {
  const vision = roleRows(MARKED).find((row) => row.role === "vision");
  assert.equal(vision.picker, false);
  assert.deepEqual(vision.options, []);
  assert.equal(vision.note, "Vision needs a Meta key.");
  assert.equal(vision.now, "Off: it needs a Meta key");
  const director = roleRows({ ...MARKED, roles: [{ role: "director", provider: null, picked: false, off_reason: "it needs a Meta key", meta_only: true }] })[0];
  assert.equal(director.note, "Director needs a Meta key.");
});

test("a role that is not meta_only is offered every provider with a key", () => {
  for (const status of [MARKED, MARKED_META]) {
    const worker = roleRows(status).find((row) => row.role === "worker");
    assert.equal(worker.picker, true);
    assert.deepEqual(
      worker.options.map((option) => option.value),
      ["", ...pickChoices(status).map((choice) => choice.value)],
    );
  }
  assert.deepEqual(
    roleRows(MARKED_META).find((row) => row.role === "worker").options.map((option) => option.value),
    ["", "meta", "baseten", "fireworks"],
  );
});

test("with no meta_only field the page does not guess, and the server's needs_meta answer explains a wrong pick", () => {
  const vision = roleRows(SET).find((row) => row.role === "vision");
  assert.equal(vision.picker, true);
  assert.deepEqual(
    vision.options.map((option) => option.value),
    ["", "baseten", "fireworks"],
  );
  assert.match(roleOutcome(400, { error: "bad_request", what: "needs_meta" }, "vision", "baseten").text, /Meta Model API only/);
});
