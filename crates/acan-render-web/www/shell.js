// The shell's alpha facts: the build's version, a quiet notice when a newer build exists, the
// Report a bug dialog, and the model key banner and setup dialog. The data comes from GET /version,
// GET /api/update, GET /api/bug-report, GET /api/setup and POST /api/setup/keys; what each answer
// means is decided in release.js and setup.js. These calls do not use stream.js's `get`, because a
// refused report or key (403 for an agent token) must not sign the tester out.
import { $, state } from "./state.js";
import { bugReportOutcome, stillChecking, updateNotice, versionLabel, versionLine } from "./release.js";
import {
  bannerText,
  providerChoices,
  providerLabel,
  routesFor,
  saveOutcome,
  showBanner,
  singleProvider,
  statusText,
} from "./setup.js";

// The server says an update check ends within about 8 seconds, so the page asks again while it
// reads "checking", a few times, then stops.
const CHECK_EVERY_MS = 2000;
const CHECK_TRIES = 6;

let build = null;
let checkRun = 0;
let reportBody = "";
// The answer to GET /api/setup: null when there is no token or no answer.
let setupStatus = null;

export function bindShell() {
  $("report-bug").addEventListener("click", reportBug);
  $("bug-open").addEventListener("click", openReport);
  $("bug-copy").addEventListener("click", copyReport);
  $("bug-cancel").addEventListener("click", () => $("bug-dialog").close());
  $("setup-open").addEventListener("click", openSetup);
  $("setup-form").addEventListener("submit", saveKey);
  $("setup-cancel").addEventListener("click", () => $("setup-dialog").close());
  // The key leaves the page when the dialog closes, whatever the answer was.
  $("setup-dialog").addEventListener("close", () => {
    $("setup-key").value = "";
  });
}

// Shows what the page can show for the token it holds, and checks for a newer build and the model
// key. Called at boot, and again when a token arrives.
export async function startShell() {
  $("alpha").hidden = !state.token;
  if (build === null) build = await loadVersion();
  paintVersion();
  checkUpdate(++checkRun, 1);
  loadSetup();
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

// Fetches the pre-filled report and shows it in full in a dialog. Nothing opens until the tester
// chooses "Open on GitHub", and nothing is posted: the issue is submitted on GitHub.
async function reportBug() {
  if (!state.token) return;
  const button = $("report-bug");
  button.disabled = true;
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
    showNote(outcome.message);
    return;
  }
  showBugDialog(outcome);
}

function showBugDialog(outcome) {
  reportBody = outcome.body;
  $("bug-note").hidden = true;
  $("bug-title").textContent = outcome.title;
  $("bug-body").textContent = outcome.body;
  $("bug-open").href = outcome.url;
  $("bug-truncated").hidden = !outcome.truncated;
  $("bug-copy").hidden = !outcome.truncated;
  $("bug-copied").hidden = true;
  $("bug-dialog").showModal();
}

// The tester chose the link, so the browser opens it in a new tab. The dialog closes with it.
function openReport() {
  $("bug-dialog").close();
  showNote("The bug report opened in a new tab. Submit the issue there.");
}

async function copyReport() {
  try {
    await navigator.clipboard.writeText(reportBody);
    $("bug-copied").textContent = "The full report is copied.";
  } catch {
    $("bug-copied").textContent = "The page could not copy the full report.";
  }
  $("bug-copied").hidden = false;
}

function showNote(text) {
  const note = $("bug-note");
  note.textContent = text;
  note.hidden = false;
}

// Reads GET /api/setup and shows the banner when no key is in use. Any principal may read it.
async function loadSetup() {
  setupStatus = null;
  if (state.token) {
    try {
      const res = await fetch("/api/setup", { headers: { Authorization: `Bearer ${state.token}` } });
      if (res.ok) setupStatus = await res.json();
    } catch {
      setupStatus = null;
    }
  }
  const banner = showBanner(setupStatus);
  $("setup-banner").hidden = !banner;
  $("setup-banner-text").textContent = bannerText(setupStatus);
  $("setup-open").hidden = setupStatus?.can_save !== true;
  if ($("setup-dialog").open) paintSetupText();
}

// Opens the setup dialog with the providers the server offers, then reads the status again.
function openSetup() {
  if (!state.token) return;
  paintSetupChoices(setupStatus);
  paintSetupText();
  setResult("", "");
  $("setup-dialog").showModal();
  loadSetup();
}

// One radio per provider the server offers. The saved provider is chosen when a key is set; with no
// key, none is chosen, so the tester picks one on purpose. Setup routes every role to the chosen
// provider; see setup.js for the per-role structure.
function paintSetupChoices(status) {
  const choices = providerChoices(status);
  const current = status?.provider ?? null;
  $("setup-choices").replaceChildren(
    ...choices.map(({ value, label }) => {
      const row = document.createElement("label");
      row.className = "choice";
      const input = document.createElement("input");
      input.type = "radio";
      input.name = "provider";
      input.value = value;
      input.checked = value === current;
      const text = document.createElement("span");
      text.textContent = label;
      row.append(input, text);
      return row;
    }),
  );
}

function paintSetupText() {
  const status = setupStatus;
  const line = $("setup-status");
  if (status === null) {
    line.textContent = "Could not read the setup status. Check that acan is running, then try again.";
    line.classList.remove("set");
    $("setup-form").hidden = true;
    $("setup-blocked").hidden = true;
    return;
  }
  const view = statusText(status);
  line.textContent = view.text;
  line.classList.toggle("set", view.set);
  const canSave = status.can_save === true;
  $("setup-form").hidden = !canSave;
  $("setup-blocked").hidden = canSave;
  $("setup-blocked").textContent = "Keys for this repository are set in its .env file, not here.";
}

function chosenProvider() {
  return $("setup-choices").querySelector('input[name="provider"]:checked')?.value ?? null;
}

function setResult(text, tone) {
  const result = $("setup-result");
  result.textContent = text;
  result.className = tone || "";
}

// Sends the key to the server, which checks it with the provider before it saves. The key leaves
// the field at once, and the page keeps no copy of it: the request carries it, nothing else does.
async function saveKey(event) {
  event.preventDefault();
  const key = $("setup-key").value;
  if (key.trim() === "") {
    setResult("Paste the key first.", "bad");
    return;
  }
  const provider = singleProvider(routesFor(chosenProvider()));
  if (provider === null) {
    setResult("Choose a provider first.", "bad");
    return;
  }
  $("setup-key").value = "";
  $("setup-save").disabled = true;
  setResult(`Checking the key with ${providerLabel(provider)}...`, "");
  let outcome;
  let saved = false;
  try {
    const res = await fetch("/api/setup/keys", {
      method: "POST",
      headers: { Authorization: `Bearer ${state.token}`, "Content-Type": "application/json" },
      body: JSON.stringify({ provider, key }),
    });
    const body = await res.json().catch(() => ({}));
    outcome = saveOutcome(res.status, body);
    saved = res.status === 200;
  } catch {
    outcome = { tone: "bad", text: "Cannot reach acan. Nothing was saved." };
  }
  $("setup-save").disabled = false;
  setResult(outcome.text, outcome.tone);
  if (saved) loadSetup();
}
