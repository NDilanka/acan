// Node tests for the model key setup's page logic. Run with:
//   node --test crates/acan-render-web/tests/js/setup.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  ROLES,
  bannerText,
  providerChoices,
  providerLabel,
  routesFor,
  saveOutcome,
  showBanner,
  singleProvider,
  statusText,
} from "../../www/setup.js";

const SAVED = {
  configured: true,
  provider: "meta",
  source: "saved_key",
  can_save: true,
  providers: ["baseten", "fireworks", "meta"],
};
const NONE = { configured: false, provider: null, source: null, can_save: true, providers: ["baseten", "fireworks", "meta"] };

test("the roles are the three a model call can do", () => {
  assert.deepEqual(ROLES, ["foreman", "worker", "fast"]);
});

test("one provider routes every role to it, and the save takes that one provider", () => {
  const routes = routesFor("meta");
  assert.deepEqual(routes, { foreman: "meta", worker: "meta", fast: "meta" });
  assert.equal(singleProvider(routes), "meta");
});

test("roles routed to different providers cannot be saved as one provider", () => {
  assert.equal(singleProvider({ foreman: "meta", worker: "baseten", fast: "meta" }), null);
  assert.equal(singleProvider({ foreman: "meta", worker: "meta" }), null);
  assert.equal(singleProvider({}), null);
  assert.equal(singleProvider(null), null);
});

test("a provider has a label, and an unknown name is shown as its own name", () => {
  assert.equal(providerLabel("meta"), "Meta Model API");
  assert.equal(providerLabel("baseten"), "Baseten");
  assert.equal(providerLabel("someday"), "someday");
});

test("the choices follow the server's list and order", () => {
  assert.deepEqual(providerChoices(SAVED), [
    { value: "baseten", label: "Baseten" },
    { value: "fireworks", label: "Fireworks" },
    { value: "meta", label: "Meta Model API" },
  ]);
  assert.deepEqual(providerChoices(null), []);
});

test("a saved key names its provider, and no key says so in one sentence", () => {
  assert.deepEqual(statusText(SAVED), {
    text: "Model calls use Meta Model API. Save a new key to replace it.",
    set: true,
  });
  assert.deepEqual(statusText(NONE), {
    text: "No model key is set. Model jobs get built-in replies until you add one.",
    set: false,
  });
});

test("a routing file's status says the repository decides, and whether its keys are set", () => {
  const routing = { configured: false, provider: null, source: "routing_file", can_save: false, providers: [] };
  assert.equal(statusText(routing).set, false);
  assert.match(statusText(routing).text, /\.env/);
  assert.equal(statusText({ ...routing, configured: true }).set, true);
});

test("the banner shows whenever no key is in use, and says why when a routing file decides", () => {
  assert.equal(showBanner(NONE), true);
  assert.equal(showBanner(SAVED), false);
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
});

test("a routing file that decides the providers is named as the reason", () => {
  assert.match(saveOutcome(409, { error: "conflict", what: "routing_file" }).text, /\.acan\/llm\.toml/);
  assert.match(saveOutcome(409, { error: "conflict", what: "unavailable" }).text, /cannot save a key/);
});

test("a malformed request and any other failure are one plain sentence, with no code shown", () => {
  for (const status of [400, 401, 500, 0]) {
    const outcome = saveOutcome(status, null);
    assert.equal(outcome.tone, "bad", String(status));
    assert.doesNotMatch(outcome.text, /[{}]|HTTP|\b[45]\d\d\b/, String(status));
  }
});

test("no save message echoes an answer's fields beyond the provider's name", () => {
  const outcome = saveOutcome(422, { result: "rejected", provider: "meta", key: "placeholder-not-shown" });
  assert.doesNotMatch(outcome.text, /placeholder-not-shown/);
});
