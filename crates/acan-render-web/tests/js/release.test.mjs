// Node tests for the alpha's shell facts: the update notice, the version line and the bug report.
// Run with:
//   node --test crates/acan-render-web/tests/js/release.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  bugReportOutcome,
  safeLink,
  stillChecking,
  updateNotice,
  versionLabel,
  versionLine,
} from "../../www/release.js";

const RELEASE = "https://github.com/NDilanka/acan/releases/tag/v0.1.0-alpha.2";

// The answer to GET /api/update, one per state, as the contract in status/alpha.md describes it.
const UPDATE = {
  off: { state: "off", current: "0.1.0-alpha.1", latest: null, url: null, checked_at: null },
  checking: { state: "checking", current: "0.1.0-alpha.1", latest: null, url: null, checked_at: null },
  current: {
    state: "current",
    current: "0.1.0-alpha.1",
    latest: "0.1.0-alpha.1",
    url: "https://github.com/NDilanka/acan/releases/tag/v0.1.0-alpha.1",
    checked_at: 1762090000,
  },
  available: {
    state: "available",
    current: "0.1.0-alpha.1",
    latest: "0.1.0-alpha.2",
    url: RELEASE,
    checked_at: 1762090000,
  },
  offline: { state: "offline", current: "0.1.0-alpha.1", latest: null, url: null, checked_at: 1762090000 },
};

test("the notice shows only for an available newer release, with its version and link", () => {
  assert.deepEqual(updateNotice(UPDATE.available), { latest: "0.1.0-alpha.2", href: RELEASE });
});

test("off, checking, current and offline show no notice", () => {
  for (const state of ["off", "checking", "current", "offline"]) {
    assert.equal(updateNotice(UPDATE[state]), null, state);
  }
});

test("a missing answer shows no notice", () => {
  assert.equal(updateNotice(null), null);
  assert.equal(updateNotice(undefined), null);
  assert.equal(updateNotice({}), null);
});

test("an available release without a usable https link or a version shows no notice", () => {
  assert.equal(updateNotice({ ...UPDATE.available, url: "http://github.com/x" }), null);
  assert.equal(updateNotice({ ...UPDATE.available, url: "javascript:alert(1)" }), null);
  assert.equal(updateNotice({ ...UPDATE.available, url: null }), null);
  assert.equal(updateNotice({ ...UPDATE.available, latest: "" }), null);
});

test("only a checking answer asks the page to look again", () => {
  assert.equal(stillChecking(UPDATE.checking), true);
  for (const state of ["off", "current", "available", "offline"]) {
    assert.equal(stillChecking(UPDATE[state]), false, state);
  }
  assert.equal(stillChecking(null), false);
});

test("a link is kept only when it is https", () => {
  assert.equal(safeLink(RELEASE), RELEASE);
  assert.equal(safeLink("http://example.com/"), null);
  assert.equal(safeLink("javascript:alert(1)"), null);
  assert.equal(safeLink("not a url"), null);
  assert.equal(safeLink(42), null);
});

test("the version line names the build, its platform and its commit when it has one", () => {
  const info = {
    name: "acan",
    version: "0.1.0-alpha.1",
    commit: "4f2c9a1d0b7e",
    os: "linux",
    arch: "x86_64",
  };
  assert.equal(versionLine(info), "acan 0.1.0-alpha.1 · linux x86_64 · 4f2c9a1");
});

test("a local build with no commit shows no commit", () => {
  const info = { name: "acan", version: "0.1.0", commit: "unknown", os: "macos", arch: "aarch64" };
  assert.equal(versionLine(info), "acan 0.1.0 · macos aarch64");
});

test("no version answer shows no version line", () => {
  assert.equal(versionLine(null), "");
  assert.equal(versionLine({}), "");
});

test("the shell bar shows the short version, and nothing when there is no answer", () => {
  assert.equal(versionLabel({ version: "0.1.0-alpha.1", commit: "4f2c9a1d0b7e" }), "v0.1.0-alpha.1");
  assert.equal(versionLabel(null), "");
  assert.equal(versionLabel({}), "");
});

test("a bug report returns its title, link, full body, and whether the link left log lines out", () => {
  const url = "https://github.com/NDilanka/acan/issues/new?title=Alpha";
  assert.deepEqual(
    bugReportOutcome(200, { url, title: "t", body: "## What happened", truncated: false }),
    { ok: true, url, title: "t", body: "## What happened", truncated: false },
  );
  assert.deepEqual(
    bugReportOutcome(200, { url, title: "t", body: "full text", truncated: true }),
    { ok: true, url, title: "t", body: "full text", truncated: true },
  );
});

test("a report with no title or body text still shows, with empty text", () => {
  const url = "https://github.com/NDilanka/acan/issues/new";
  assert.deepEqual(
    bugReportOutcome(200, { url }),
    { ok: true, url, title: "", body: "", truncated: false },
  );
});

test("a report whose link is not https is not opened", () => {
  const outcome = bugReportOutcome(200, { url: "http://example.com/issue", body: "" });
  assert.equal(outcome.ok, false);
});

test("an agent's refused report says plainly that only members can send one", () => {
  const outcome = bugReportOutcome(403, { error: "forbidden", why: "only members may do this over HTTP" });
  assert.equal(outcome.ok, false);
  assert.match(outcome.message, /Only members can send a bug report/);
});

test("a session that is not signed in is told so in one sentence", () => {
  const outcome = bugReportOutcome(401, { error: "unauthorized" });
  assert.equal(outcome.ok, false);
  assert.match(outcome.message, /not signed in/);
});

test("any other failure is one plain sentence, with no code or JSON shown", () => {
  for (const status of [0, 500, 502]) {
    const outcome = bugReportOutcome(status, null);
    assert.equal(outcome.ok, false, String(status));
    assert.equal(outcome.message, "The bug report could not be prepared. Try again in a moment.");
  }
  for (const message of [
    bugReportOutcome(403, null).message,
    bugReportOutcome(401, null).message,
  ]) {
    assert.doesNotMatch(message, /[{}]|HTTP|\b40\d\b/);
  }
});
