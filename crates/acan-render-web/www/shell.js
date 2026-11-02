// The shell's alpha facts: the build's version, a quiet notice when a newer build exists, and the
// Report a bug button. The data comes from GET /version, GET /api/update and GET /api/bug-report;
// what each answer means is decided in release.js. These calls do not use stream.js's `get`,
// because a refused bug report (403 for an agent token) must not sign the tester out.
import { $, state } from "./state.js";
import { bugReportOutcome, stillChecking, updateNotice, versionLabel, versionLine } from "./release.js";

// The server says an update check ends within about 8 seconds, so the page asks again while it
// reads "checking", a few times, then stops.
const CHECK_EVERY_MS = 2000;
const CHECK_TRIES = 6;

let build = null;
let checkRun = 0;
let reportBody = "";

export function bindShell() {
  $("report-bug").addEventListener("click", reportBug);
  $("bug-copy").addEventListener("click", copyReport);
}

// Shows what the page can show for the token it holds, and checks for a newer build. Called at
// boot, and again when a token arrives.
export async function startShell() {
  $("alpha").hidden = !state.token;
  if (build === null) build = await loadVersion();
  paintVersion();
  checkUpdate(++checkRun, 1);
}

async function loadVersion() {
  try {
    const res = await fetch("/version");
    return res.ok ? await res.json() : null;
  } catch {
    return null;
  }
}

function paintVersion() {
  const label = versionLabel(build);
  $("version").textContent = label;
  $("version").title = versionLine(build);
  $("version").hidden = label === "";
}

async function checkUpdate(run, attempt) {
  if (!state.token) {
    paintUpdate(null);
    return;
  }
  let update = null;
  try {
    const res = await fetch("/api/update", { headers: { Authorization: `Bearer ${state.token}` } });
    if (res.ok) update = await res.json();
  } catch {
    update = null;
  }
  // A newer run of the check has started, so this answer is out of date.
  if (run !== checkRun) return;
  paintUpdate(update);
  if (stillChecking(update) && attempt < CHECK_TRIES) {
    setTimeout(() => checkUpdate(run, attempt + 1), CHECK_EVERY_MS);
  }
}

function paintUpdate(update) {
  const notice = updateNotice(update);
  $("update").hidden = notice === null;
  if (notice === null) return;
  $("update-text").textContent = `New build available: v${notice.latest}`;
  $("update-link").href = notice.href;
}

// Opens the bug report in a new tab. The tab opens at once, because a browser blocks a tab that
// opens only after the answer arrives. If the browser blocks it anyway, the note keeps the link.
async function reportBug() {
  if (!state.token) return;
  const button = $("report-bug");
  button.disabled = true;
  $("bug-copy").hidden = true;
  const tab = window.open("about:blank", "_blank");
  if (tab) tab.opener = null;
  showNote("Preparing the bug report...");
  let outcome;
  try {
    const res = await fetch("/api/bug-report", {
      headers: { Authorization: `Bearer ${state.token}` },
    });
    const payload = res.status === 200 ? await res.json() : null;
    outcome = bugReportOutcome(res.status, payload);
  } catch {
    outcome = bugReportOutcome(0, null);
  }
  button.disabled = false;
  if (!outcome.ok) {
    tab?.close();
    showNote(outcome.message);
    return;
  }
  if (tab) tab.location.href = outcome.url;
  reportBody = outcome.body;
  $("bug-copy").hidden = !outcome.truncated;
  if (tab) showNote("The bug report opened in a new tab.");
  else showNote("The browser blocked the new tab. Open the report here:", outcome.url);
}

async function copyReport() {
  try {
    await navigator.clipboard.writeText(reportBody);
    showNote("The full report is copied.");
  } catch {
    showNote("The page could not copy the full report.");
  }
}

function showNote(text, href) {
  const note = $("bug-note");
  note.replaceChildren(text);
  if (href) {
    const link = document.createElement("a");
    link.href = href;
    link.target = "_blank";
    link.rel = "noopener";
    link.textContent = "bug report";
    note.append(" ", link);
  }
  note.hidden = false;
}
