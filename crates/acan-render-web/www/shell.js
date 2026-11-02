// The shell's alpha facts: the build's version, a quiet notice when a newer build exists, the
// Report a bug dialog, and the model key banner and setup dialog. The data comes from GET /version,
// GET /api/update, GET /api/bug-report and the /api/setup routes; what each answer means is decided
// in release.js and setup.js. These calls do not use stream.js's `get`, because a refused report,
// key or pick (403 for an agent token) must not sign the tester out.
import { $, state } from "./state.js";
import { bugReportOutcome, stillChecking, updateNotice, versionLabel, versionLine } from "./release.js";
import {
  bannerText,
  focusAfterRemove,
  keyRows,
  providerChoices,
  providerLabel,
  removeOutcome,
  roleOutcome,
  roleRows,
  saveOutcome,
  showBanner,
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
// True while a pick or a remove is in flight, so a second one waits for the first answer.
let setupBusy = false;

export function bindShell() {
  $("report-bug").addEventListener("click", reportBug);
  $("bug-open").addEventListener("click", openReport);
  $("bug-copy").addEventListener("click", copyReport);
  $("bug-cancel").addEventListener("click", () => $("bug-dialog").close());
  $("setup-open").addEventListener("click", openSetup);
  $("setup-manage").addEventListener("click", openSetup);
  $("setup-form").addEventListener("submit", saveKey);
  $("setup-roles").addEventListener("change", (event) => {
    const select = event.target;
    if (!(select instanceof HTMLSelectElement)) return;
    // While a pick or remove is in flight the picker is only aria-disabled, so a change is undone.
    if (setupBusy) {
      select.value = select.dataset.value ?? "";
      return;
    }
    pickRole(select.dataset.role, select.value);
  });
  $("setup-keys").addEventListener("click", (event) => {
    const button = event.target;
    if (button instanceof HTMLButtonElement && button.dataset.provider) removeKey(button.dataset.provider);
  });
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
  // Once the banner's "Set a key" is gone, this is the way back to the keys and the role picks.
  $("setup-manage").hidden = setupStatus === null || (banner && setupStatus.can_save === true);
  if ($("setup-dialog").open) paintSetupText();
}

// Opens the setup dialog with the providers the server offers, then reads the status again.
function openSetup() {
  if (!state.token) return;
  paintSetupChoices(setupStatus);
  paintSetupText();
  setResult("", "");
  setChanges("", "");
  $("setup-dialog").showModal();
  loadSetup();
}

// One radio per provider the server offers, for the key to save. None is chosen, so the tester
// picks one on purpose. Saving a key for a provider that has one replaces it.
function paintSetupChoices(status) {
  const choices = providerChoices(status);
  $("setup-choices").replaceChildren(
    ...choices.map(({ value, label }) => {
      const row = document.createElement("label");
      row.className = "choice";
      const input = document.createElement("input");
      input.type = "radio";
      input.name = "provider";
      input.value = value;
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
    $("setup-roles-section").hidden = true;
    $("setup-keys-section").hidden = true;
    return;
  }
  const view = statusText(status);
  line.textContent = view.text;
  line.classList.toggle("set", view.set);
  const canSave = status.can_save === true;
  $("setup-form").hidden = !canSave;
  $("setup-blocked").hidden = canSave;
  $("setup-blocked").textContent = "Keys for this repository are set in its .env file, not here.";
  paintSetupLists(status, canSave);
}

// The lists are painted again after every answer, so focus goes back to the control that had it.
function paintSetupLists(status, canSave) {
  const focused = focusKey();
  const roles = roleRows(status);
  $("setup-roles-section").hidden = roles.length === 0;
  $("setup-roles").replaceChildren(...roles.map((row, index) => roleItem(row, index, canSave)));
  const keys = keyRows(status);
  $("setup-keys-section").hidden = keys.length === 0;
  $("setup-keys").replaceChildren(...keys.map(keyItem));
  refocus(focused);
}

function focusKey() {
  return document.activeElement?.dataset?.focus ?? null;
}

function refocus(key) {
  if (key === null) return;
  const again = [...$("setup-dialog").querySelectorAll("[data-focus]")].find((el) => el.dataset.focus === key);
  if (again) again.focus();
}

// A role's row: its name, what serves it now, and a picker when this page may change it. The ids
// use the row's place, because a role name comes from the server.
function roleItem(row, index, canSave) {
  const item = document.createElement("li");
  item.className = "setup-row";
  const id = `setup-role-${index}`;
  const name = document.createElement(canSave && row.picker ? "label" : "span");
  name.className = "setup-name";
  name.textContent = row.role;
  const now = document.createElement("span");
  now.className = row.off ? "setup-now off" : "setup-now";
  now.id = `${id}-now`;
  now.textContent = row.now;
  item.append(name, now);
  if (!canSave) return item;
  if (!row.picker) {
    if (row.note === null) return item;
    const note = document.createElement("span");
    note.className = "setup-note";
    note.textContent = row.note;
    item.append(note);
    return item;
  }
  name.htmlFor = id;
  const select = document.createElement("select");
  select.id = id;
  select.dataset.role = row.role;
  select.dataset.focus = `role:${row.role}`;
  select.dataset.value = row.value;
  select.setAttribute("aria-describedby", now.id);
  markBusy(select);
  for (const { value, label } of row.options) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = label;
    option.selected = value === row.value;
    select.append(option);
  }
  item.append(select);
  return item;
}

// A provider's row: its name, where its key comes from, and Remove only for a key saved here.
function keyItem(row) {
  const item = document.createElement("li");
  item.className = "setup-row";
  const name = document.createElement("span");
  name.className = "setup-name";
  name.textContent = row.label;
  const from = document.createElement("span");
  from.className = row.source === null ? "setup-now off" : "setup-now";
  from.textContent = row.text;
  item.append(name, from);
  if (row.removable) {
    const remove = document.createElement("button");
    remove.type = "button";
    remove.textContent = "Remove";
    remove.dataset.provider = row.provider;
    remove.dataset.focus = `key:${row.provider}`;
    remove.setAttribute("aria-label", `Remove the ${row.label} key`);
    markBusy(remove);
    item.append(remove);
  }
  return item;
}

function chosenProvider() {
  return $("setup-choices").querySelector('input[name="provider"]:checked')?.value ?? null;
}

function setResult(text, tone) {
  const result = $("setup-result");
  result.textContent = text;
  result.className = tone || "";
}

function setChanges(text, tone) {
  const changes = $("setup-changes");
  changes.textContent = text;
  changes.className = tone || "";
}

// Busy controls are aria-disabled, not disabled: a disabled control drops focus to the page body.
// The handlers refuse a second request while setupBusy is set.
function markBusy(control) {
  if (setupBusy) control.setAttribute("aria-disabled", "true");
  else control.removeAttribute("aria-disabled");
}

function setBusy(busy) {
  setupBusy = busy;
  for (const control of $("setup-dialog").querySelectorAll("#setup-roles select, #setup-keys button")) {
    markBusy(control);
  }
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
  const provider = chosenProvider();
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

// Picks a provider for one role, or clears the pick when the tester chooses Default. The status is
// read again after any answer, so the picker shows what the server holds.
async function pickRole(role, value) {
  if (!state.token || typeof role !== "string" || setupBusy) return;
  const provider = value === "" ? null : value;
  const focused = focusKey();
  setBusy(true);
  setChanges(`Changing ${role}...`, "");
  let outcome;
  try {
    const res = await fetch(`/api/setup/roles/${encodeURIComponent(role)}`, {
      method: "PUT",
      headers: { Authorization: `Bearer ${state.token}`, "Content-Type": "application/json" },
      body: JSON.stringify({ provider }),
    });
    const body = await res.json().catch(() => ({}));
    outcome = roleOutcome(res.status, body, role, provider);
  } catch {
    outcome = { tone: "bad", text: "Cannot reach acan. The pick did not change." };
  }
  setChanges(outcome.text, outcome.tone);
  await loadSetup();
  setBusy(false);
  refocus(focused);
}

// Removes a key saved on this page. A key the environment sets has no Remove button; if the server
// still refuses, its own words are shown.
async function removeKey(provider) {
  if (!state.token || typeof provider !== "string" || setupBusy) return;
  const focused = focusKey();
  setBusy(true);
  setChanges(`Removing the ${providerLabel(provider)} key...`, "");
  let outcome;
  let target = null;
  try {
    const res = await fetch(`/api/setup/keys/${encodeURIComponent(provider)}`, {
      method: "DELETE",
      headers: { Authorization: `Bearer ${state.token}` },
    });
    const body = await res.json().catch(() => ({}));
    outcome = removeOutcome(res.status, body, provider);
    target = focusAfterRemove(res.status);
  } catch {
    outcome = { tone: "bad", text: "Cannot reach acan. Nothing was removed." };
  }
  setChanges(outcome.text, outcome.tone);
  await loadSetup();
  setBusy(false);
  // A removed key's button is gone, so focus moves to the sentence that says what happened.
  if (target !== null) $(target).focus();
  else refocus(focused);
}
