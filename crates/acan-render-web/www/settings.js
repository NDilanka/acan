// The settings drawer: the cog, the drawer that slides in from the left, and its nine sections.
// settings-model.js and themes.js decide what the rows say and how a theme applies; this file
// builds the nodes and makes the calls. Text from the server or the user goes in as textContent.
import {
  SECTIONS,
  sectionsFor,
  themeLabel,
  keyRows,
  roleRows,
  harnessRows,
  BUDGET_FIELDS,
  getAt,
  setAt,
  parseField,
  patchFor,
  fieldErrors,
  tokenFromRotate,
  testText,
  keyNotes,
  appliedNote,
  changedPaths,
  updateView,
  installNote,
  exportNote,
  clearNote,
  conflictNote,
  hasTokens,
  stepWorkers,
  statemapHint,
  providerLabel,
} from "./settings-model.js";
import { THEMES, applyTheme } from "./themes.js";
import { createMockApi } from "./settings-mock.js";
import { apiPath, target } from "./projects.js";

const ARM_MS = 6000;
const FLASH_MS = 1500;
const HIDE_MS = 150;
const SVG_NS = "http://www.w3.org/2000/svg";
const FIXTURE_URL = new URL("fixtures/settings.json", import.meta.url);
const GEAR_D =
  "M12.16 6.93L14.02 7.05L14.02 8.95L12.16 9.07L11.7 10.19L12.94 11.59L11.59 12.94L10.19 11.7L9.07 12.16L8.95 14.02L7.05 14.02L6.93 12.16L5.81 11.7L4.41 12.94L3.06 11.59L4.3 10.19L3.84 9.07L1.98 8.95L1.98 7.05L3.84 6.93L4.3 5.81L3.06 4.41L4.41 3.06L5.81 4.3L6.93 3.84L7.05 1.98L8.95 1.98L9.07 3.84L10.19 4.3L11.59 3.06L12.94 4.41L11.7 5.81Z";

// Budget rows, grouped as the drawer shows them.
const BUDGET_GROUPS = [
  {
    title: "Caps",
    note: "Work stops at a cap. Nothing is lost.",
    paths: ["repair_rounds", "max_episodes", "wall_time_ms", "model_cost_micros"],
  },
  {
    title: "Calls per role",
    note: "Per episode.",
    paths: [
      "call_limits.foreman",
      "call_limits.worker",
      "call_limits.fast",
      "call_limits.vision",
      "call_limits.director",
      "call_limits.wonder",
      "call_limits.statemap",
    ],
  },
  {
    title: "Stuck and drift",
    note: "When the foreman flags a drone.",
    paths: ["stuck_declare_ms", "stuck_build_ms", "drift_threshold"],
  },
];

let deps = { app: null, requestRender: () => {}, token: () => null, onToken: () => {} };
let api = null;
let wired = false;
let cog = null;
let drawer = null;
let titleEl = null;
let whoEl = null;
let closeEl = null;
let noteEl = null;
let bodyEl = null;

let settings = null;
let loadFailed = false;
let adapters = null;
let adaptersBusy = false;
let update = null;
let updateBusy = false;
let updateStatus = null;

let isOpen = false;
let openId = null;
let armed = null;
let armTimer = 0;
let hideTimer = 0;
let revealed = false;
let rendering = false;
let pendingFocus = null;
let editingKey = null;
const keyFields = new Map();
const drafts = new Map();
const invalid = new Map();
const notes = new Map();
const keyNoteMap = new Map();
const tests = new Map();
const flashes = new Map();
const saving = new Set();

// Members see every control as read only, with no actions.
const ro = () => settings?.you === "member";

// Builds a node. Text goes in as textContent; null, undefined and false children are skipped.
function el(tag, props = {}, ...kids) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    if (name === "text") node.textContent = String(value);
    else if (name === "value") node.value = String(value);
    else if (value === true) node.setAttribute(name, "");
    else node.setAttribute(name, String(value));
  }
  for (const kid of kids.flat(Infinity)) {
    if (kid === undefined || kid === null || kid === false) continue;
    node.append(kid instanceof Node ? kid : document.createTextNode(String(kid)));
  }
  return node;
}

// Wires a control. For members it stays in view as read only: aria-disabled, and no handler.
function wireTo(node, type, handler) {
  if (ro()) {
    node.setAttribute("aria-disabled", "true");
    if (node instanceof HTMLInputElement || node instanceof HTMLSelectElement) node.disabled = true;
    return node;
  }
  node.addEventListener(type, handler);
  return node;
}

// An action button. Members get none, so this returns null for them.
function act(text, ctl, handler, extra = {}) {
  if (ro()) return null;
  const node = el("button", { type: "button", "data-ctl": ctl, ...extra }, text);
  node.addEventListener("click", handler);
  return node;
}

function chip(kind, text) {
  return el("span", { class: `chip ${kind}` }, el("i", { "aria-hidden": "true" }), text ? el("span", { text }) : null);
}

function fieldNote(path) {
  const why = invalid.get(path);
  return why ? el("p", { class: "fieldnote", text: why }) : null;
}

// Hint text, with the parts between backticks set as code.
function withCode(text) {
  return String(text ?? "")
    .split("`")
    .map((part, i) => (i % 2 === 1 ? el("code", { text: part }) : part));
}

function gearIcon() {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  const path = document.createElementNS(SVG_NS, "path");
  path.setAttribute("d", GEAR_D);
  const hub = document.createElementNS(SVG_NS, "circle");
  hub.setAttribute("cx", "8");
  hub.setAttribute("cy", "8");
  hub.setAttribute("r", "2");
  svg.append(path, hub);
  return svg;
}

function fetchApi() {
  return {
    async request(method, path, body) {
      const headers = {};
      const token = deps.token();
      if (token) headers.Authorization = `Bearer ${token}`;
      const init = { method, headers };
      if (body !== undefined) {
        headers["Content-Type"] = "application/json";
        init.body = JSON.stringify(body);
      }
      const res = await fetch(path, init);
      let json = null;
      try {
        json = await res.json();
      } catch {
        json = null;
      }
      return { status: res.status, body: json };
    },
    // A file answer comes back as a blob on 200; any other answer is JSON, as in request.
    async download(path) {
      const headers = {};
      const token = deps.token();
      if (token) headers.Authorization = `Bearer ${token}`;
      const res = await fetch(path, { headers });
      if (res.status === 200) return { status: 200, blob: await res.blob() };
      let json = null;
      try {
        json = await res.json();
      } catch {
        json = null;
      }
      return { status: res.status, body: json };
    },
  };
}

async function mockApi() {
  const res = await fetch(FIXTURE_URL);
  return createMockApi(await res.json());
}

// Installs the drawer on the page. It reads the settings once, applies the look, and unhides the
// cog. The map does not wait on it.
export async function installSettings({
  app = null,
  requestRender = () => {},
  token = () => null,
  onToken = () => {},
  fixture = false,
} = {}) {
  deps = { app, requestRender, token, onToken };
  cog = document.getElementById("settings-cog");
  drawer = document.getElementById("settings");
  titleEl = document.getElementById("settings-title");
  whoEl = document.getElementById("settings-who");
  closeEl = document.getElementById("settings-close");
  noteEl = document.getElementById("settings-note");
  bodyEl = document.getElementById("settings-body");
  if (!cog || !drawer || !bodyEl) return;
  if (!wired) wire();
  api = fixture ? await mockApi() : fetchApi();
  cog.hidden = false;
  await load();
  deepLink();
}

function wire() {
  wired = true;
  cog.setAttribute("aria-controls", "settings");
  cog.setAttribute("aria-label", "Settings");
  cog.setAttribute("aria-expanded", "false");
  cog.title = "Settings (,)";
  if (!cog.querySelector("svg")) cog.append(gearIcon());
  cog.addEventListener("click", toggleSettings);
  closeEl.addEventListener("click", hideDrawer);
  drawer.addEventListener("focusout", onFocusOut);
  window.addEventListener("hashchange", deepLink);
  if (!drawer.querySelector("kbd")) closeEl.before(el("kbd", { text: "Esc" }));
}

// `#settings/<section>` opens the drawer at that section.
function deepLink() {
  const match = /^#settings\/([a-z]+)$/.exec(location.hash);
  if (match && SECTIONS.some((section) => section.id === match[1])) openSettings(match[1]);
}

async function load() {
  if (!api) return;
  try {
    const res = await api.request("GET", "/api/settings");
    loadFailed = res.status !== 200 || !res.body;
    if (!loadFailed) {
      settings = res.body;
      applyLook(settings.look);
    }
  } catch {
    loadFailed = true;
  }
  render();
}

// Applies the theme at once: the page, then the wasm app, then a redraw. Reduced motion adds the
// class that the stylesheet reads.
function applyLook(look) {
  if (look) {
    const root = document.documentElement;
    const id = applyTheme(root, look.theme);
    if (typeof deps.app?.set_look === "function") deps.app.set_look(id);
    root.classList.toggle("reduce-motion", look.reduced_motion === true);
  }
  deps.requestRender();
}

function failureText(status) {
  if (status === 0) return "Could not reach acan. Nothing changed.";
  if (status === 401) return "This session is not signed in. Nothing changed.";
  if (status === 403) return "Only the owner can change this. Nothing changed.";
  return "Not saved. Nothing changed.";
}

// Sends one PUT. A 200 takes the server's settings and its applied and key notes. A 422 marks the
// fields it names and changes nothing else. Other answers say so in the section.
async function put(section, body, onOk = null) {
  let res = { status: 0, body: null };
  try {
    res = await api.request("PUT", "/api/settings", body);
  } catch {
    res = { status: 0, body: null };
  }
  if (res.status === 200 && res.body?.settings) {
    settings = res.body.settings;
    invalid.clear();
    for (const [provider, state] of Object.entries(res.body.keys ?? {})) {
      keyNoteMap.set(provider, { bad: state === "rejected", text: keyNotes({ [provider]: state })[0] ?? "" });
    }
    notes.set(section, { bad: false, text: appliedNote(res.body.applied, changedPaths(body)) });
    onOk?.();
    flash(section);
    return true;
  }
  // A 422 or a 409 names the fields it refused; each message sits beside its field.
  if (res.status === 422 || res.status === 409) {
    invalid.clear();
    for (const [path, why] of fieldErrors(res.body?.fields)) invalid.set(path, why);
  }
  notes.set(section, { bad: true, text: res.status === 409 ? conflictNote(res.body) : failureText(res.status) });
  render();
  return false;
}

// A change that shows at once and is undone if the server refuses it.
async function change(section, after, onOk = null) {
  const before = settings[section];
  const body = patchFor(section, before, after);
  if (!body) return;
  settings = { ...settings, [section]: after };
  if (section === "look") applyLook(after);
  render();
  const ok = await put(section, body, onOk);
  if (!ok) {
    settings = { ...settings, [section]: before };
    if (section === "look") applyLook(before);
    render();
  }
}

// Shows a short word in place of a summary or a button for a moment.
function flash(key) {
  clearTimeout(flashes.get(key));
  flashes.set(
    key,
    setTimeout(() => {
      flashes.delete(key);
      render();
    }, FLASH_MS),
  );
  render();
}

function motionOff() {
  return document.documentElement.classList.contains("reduce-motion") || matchMedia("(prefers-reduced-motion: reduce)").matches;
}

// The usage panel expands over the drawer, so it is folded when the drawer opens.
function collapseUsage() {
  const badge = document.getElementById("meta-badge");
  if (badge?.getAttribute("aria-expanded") === "true") badge.click();
}

export function settingsOpen() {
  return isOpen;
}

// Opens the drawer, all sections collapsed, or the named section. Focus moves to the title.
export function openSettings(section) {
  if (!drawer) return;
  const named = SECTIONS.some((entry) => entry.id === section);
  const wasOpen = isOpen;
  if (!wasOpen) {
    openId = named ? section : null;
    clearTimeout(hideTimer);
    collapseUsage();
    isOpen = true;
    drawer.hidden = false;
    void drawer.offsetWidth; // start the slide from the closed place
    drawer.classList.add("open");
    cog.setAttribute("aria-expanded", "true");
  } else if (named) {
    openId = section;
    pendingFocus = `head:${section}`;
  }
  if (openId) sectionOpened(openId);
  if (!settings) load();
  render();
  if (!wasOpen) titleEl.focus({ preventScroll: true });
}

// Closes the drawer, and focus returns to the cog.
function hideDrawer() {
  if (!isOpen) return;
  isOpen = false;
  armed = null;
  clearTimeout(armTimer);
  revealed = false;
  editingKey = null;
  for (const field of keyFields.values()) field.value = "";
  keyFields.clear();
  drafts.clear();
  invalid.clear();
  cog.setAttribute("aria-expanded", "false");
  const active = document.activeElement;
  if (!active || active === document.body || drawer.contains(active)) cog.focus({ preventScroll: true });
  drawer.classList.remove("open");
  clearTimeout(hideTimer);
  hideTimer = setTimeout(
    () => {
      if (isOpen) return;
      drawer.hidden = true;
      bodyEl.replaceChildren();
    },
    motionOff() ? 0 : HIDE_MS,
  );
}

export function toggleSettings() {
  if (isOpen) hideDrawer();
  else openSettings();
}

// Esc steps back one level: it disarms an armed confirm, then cancels an open key field, then
// closes the drawer.
export function closeSettings() {
  if (!isOpen) return;
  if (armed !== null) {
    disarmNow(armed);
    return;
  }
  if (editingKey !== null) {
    cancelKey();
    return;
  }
  hideDrawer();
}

// A confirm disarms when focus leaves its control, as the history cancel does.
function onFocusOut(event) {
  if (rendering || armed === null) return;
  const own = [armed, `cancel:${armed}`];
  const from = event.target?.dataset?.ctl;
  const to = event.relatedTarget?.dataset?.ctl;
  if (!own.includes(from) || own.includes(to)) return;
  setTimeout(() => disarmNow(), 0);
}

// The first click arms a confirm, and it disarms itself after six seconds.
function armKey(key) {
  armed = key;
  clearTimeout(armTimer);
  armTimer = setTimeout(() => {
    armed = null;
    render();
  }, ARM_MS);
  pendingFocus = key;
  render();
}

function disarmNow(focusKey = null) {
  if (armed === null) return;
  armed = null;
  clearTimeout(armTimer);
  pendingFocus = focusKey;
  render();
}

// A two-step action in place. Armed, the button reads `run` and a cancel sits beside it.
function confirmParts(key, { arm, run, line, onRun }) {
  if (ro()) return { buttons: [], line: null };
  if (armed !== key) {
    return { buttons: [act(arm, key, () => armKey(key))], line: null };
  }
  const runButton = el("button", { type: "button", class: "armed", "data-ctl": key }, run);
  runButton.addEventListener("click", onRun);
  const cancel = el("button", { type: "button", class: "cancel", "data-ctl": `cancel:${key}` }, "cancel");
  cancel.addEventListener("click", () => disarmNow(key));
  return { buttons: [runButton, cancel], line: el("p", { class: "fill note", text: line }) };
}

// Opening a section starts the checks it needs.
function sectionOpened(id) {
  if (id === "harness" && adapters === null && !adaptersBusy) loadAdapters();
}

function toggleSection(id) {
  openId = openId === id ? null : id;
  pendingFocus = `head:${id}`;
  sectionOpened(openId);
  render();
}

function focusedCtl() {
  const active = document.activeElement;
  return active && drawer.contains(active) ? (active.dataset?.ctl ?? null) : null;
}

// Rebuilds the body from the state. Focus goes back to the same control when the rebuild removed it.
function render() {
  if (!isOpen || !bodyEl) return;
  const target = pendingFocus ?? focusedCtl();
  pendingFocus = null;
  rendering = true;
  try {
    bodyEl.replaceChildren(...(settings ? sectionsFor(settings).map(sectionNode) : []));
    whoEl.hidden = !ro();
    whoEl.textContent = "read only";
    noteEl.hidden = !loadFailed;
    noteEl.textContent = loadFailed ? "Settings could not load." : "";
    noteEl.classList.toggle("bad", loadFailed);
  } finally {
    rendering = false;
  }
  if (target) focusCtl(target);
}

function focusCtl(key) {
  const active = document.activeElement;
  if (active && active !== document.body && drawer.contains(active)) return;
  const node = [...bodyEl.querySelectorAll("[data-ctl]")].find((entry) => entry.dataset.ctl === key);
  node?.focus({ preventScroll: true });
}

function summaryOf(id) {
  if (!settings) return { text: "" };
  const s = settings;
  switch (id) {
    case "models": {
      const n = keyRows(s.models).filter((key) => key.set).length;
      return { text: `${n} ${n === 1 ? "key" : "keys"} set` };
    }
    case "harness": {
      const worker = s.harness?.roles?.worker;
      if (!worker || worker === "builtin") return { text: "Meta" };
      return { text: adapters?.find((entry) => entry.id === worker)?.name ?? worker };
    }
    case "projects": {
      const n = (s.projects ?? []).length;
      return { text: `${n} ${n === 1 ? "project" : "projects"}` };
    }
    case "budgets":
      return { text: `$${Number(((s.budgets?.model_cost_micros ?? 0) / 1000000).toFixed(2))} cap` };
    case "look":
      return { text: themeLabel(s.look?.theme) };
    case "access": {
      if (!hasTokens(s.tokens)) return { text: "" };
      const n = (s.tokens.agents ?? []).length;
      return { text: `${n} ${n === 1 ? "token" : "tokens"}` };
    }
    case "notifications":
      return { text: `alerts ${s.notifications?.needs_you ? "on" : "off"}` };
    case "data":
      return { text: "export" };
    case "about": {
      const newer = update !== null && updateView(update).state === "available";
      return { text: newer ? "update" : `v${s.about?.version ?? ""}`, newer };
    }
    default:
      return { text: "" };
  }
}

function sectionNode(section) {
  const isShown = section.id === openId;
  const summary = summaryOf(section.id);
  const flashing = flashes.has(section.id);
  const head = el(
    "button",
    {
      type: "button",
      class: "sec-head",
      "aria-expanded": String(isShown),
      "aria-controls": isShown ? `sec-${section.id}` : null,
      "data-ctl": `head:${section.id}`,
    },
    el("span", { class: "chev", "aria-hidden": "true", text: "›" }),
    el("span", { class: "sec-name", text: section.title }),
    el("span", {
      class: `sec-sum${summary.newer && !flashing ? " update" : ""}`,
      text: flashing ? "saved" : summary.text,
    }),
  );
  head.addEventListener("click", () => toggleSection(section.id));
  const parts = [el("h3", {}, head)];
  if (isShown) parts.push(el("div", { class: "sec-body", id: `sec-${section.id}` }, bodyFor(section.id)));
  return el("section", { class: "sec", "data-section": section.id }, parts);
}

function bodyFor(id) {
  switch (id) {
    case "models":
      return modelsBody();
    case "harness":
      return harnessBody();
    case "projects":
      return projectsBody();
    case "budgets":
      return budgetsBody();
    case "look":
      return lookBody();
    case "access":
      return accessBody();
    case "notifications":
      return notificationsBody();
    case "data":
      return dataBody();
    case "about":
      return aboutBody();
    default:
      return [];
  }
}

function statusLine(id) {
  const note = notes.get(id);
  return el("p", { class: `status${note?.bad ? " bad" : ""}`, role: "status", text: note?.text ?? "" });
}

function group(title, note, kids = [], side = null) {
  const head = title ? el("div", { class: "gh" }, el("h4", { text: title }), side) : null;
  return el("div", { class: "group" }, head, note ? el("p", { class: "gnote", text: note }) : null, kids);
}

function seg(options, current, onPick, ctl) {
  return el(
    "div",
    { class: "seg", role: "group" },
    options.map((option) => {
      const node = el(
        "button",
        { type: "button", "aria-pressed": String(option.value === current), "data-ctl": `${ctl}:${option.value}` },
        option.node ?? null,
        option.label,
      );
      return wireTo(node, "click", () => onPick(option.value));
    }),
  );
}

// A switch with its label on the left. `before` sits between the label and the switch.
function toggleRow(label, checked, onToggle, { disabled = false, path = null, before = null, after = null } = {}) {
  const node = el("button", {
    type: "button",
    role: "switch",
    class: "switch",
    "aria-checked": String(checked),
    "aria-label": label,
    "data-ctl": `switch:${path ?? label}`,
  });
  if (disabled) node.setAttribute("aria-disabled", "true");
  wireTo(node, "click", () => {
    if (!disabled) onToggle(!checked);
  });
  return el(
    "div",
    { class: before ? "row notif" : "row wide", "data-path": path },
    el("span", { class: "label", text: label }),
    before,
    node,
    path ? fieldNote(path) : null,
    after,
  );
}

// ---- Models ---------------------------------------------------------------------------------

function modelsBody() {
  const keys = keyRows(settings.models);
  return [
    group("Keys", "Stored on this machine, outside the repo.", keys.flatMap((row) => keyRowNodes(row))),
    group(
      "Roles",
      "Test makes one cheap call.",
      roleRows(settings.models).map((role) => roleRowNode(role, keys)),
    ),
    statusLine("models"),
  ];
}

function keyRowNodes(row) {
  const path = `models.keys.${row.provider}`;
  const note = keyNoteMap.get(row.provider);
  const fromEnv = row.source === "from environment";
  const stateNodes = [row.set ? chip("passed", "set") : chip("pending", "not set")];
  if (row.set && fromEnv) stateNodes.push(el("span", { class: "muted", text: "from env" }));
  const nodes = [
    el("span", { class: "name", text: row.label }),
    el("span", { class: "state" }, ...stateNodes),
    el(
      "span",
      { class: "acts" },
      act(row.set ? "Replace" : "Add", `key:${row.provider}`, () => editKey(row.provider)),
      row.set && !fromEnv ? act("Remove", `keyremove:${row.provider}`, () => removeKey(row.provider)) : null,
    ),
  ];
  if (note?.text) nodes.push(el("p", { class: `fill note${note.bad ? " bad" : ""}`, text: note.text }));
  if (invalid.has(path)) nodes.push(fieldNote(path));
  const out = [el("div", { class: "row", "data-path": path }, nodes)];
  if (editingKey === row.provider && !ro()) out.push(keyEditor(row.provider, path));
  return out;
}

// The key field is kept per provider, so a re-render does not lose what is typed. It is never
// read back into the page: Save sends it and clears the field.
function keyField(provider) {
  let input = keyFields.get(provider);
  if (!input) {
    input = el("input", {
      type: "password",
      autocomplete: "off",
      spellcheck: "false",
      placeholder: "paste key",
      "aria-label": `${providerLabel(provider)} key`,
      "data-ctl": `keyfield:${provider}`,
    });
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        saveKey(provider);
      }
    });
    keyFields.set(provider, input);
  }
  return input;
}

function keyEditor(provider, path) {
  const save = el("button", { type: "button", class: "primary", "data-ctl": `keysave:${provider}` }, "Save");
  save.addEventListener("click", () => saveKey(provider));
  const cancel = el("button", { type: "button", "data-ctl": `keycancel:${provider}` }, "Cancel");
  cancel.addEventListener("click", () => cancelKey());
  return el("div", { class: "row keyedit", "data-path": path }, keyField(provider), el("span", { class: "acts" }, save, cancel));
}

function closeKeyField(provider) {
  const field = keyFields.get(provider);
  if (field) field.value = "";
  keyFields.delete(provider);
  if (editingKey === provider) editingKey = null;
}

function editKey(provider) {
  if (editingKey !== null && editingKey !== provider) closeKeyField(editingKey);
  editingKey = provider;
  pendingFocus = `keyfield:${provider}`;
  render();
}

function cancelKey() {
  const provider = editingKey;
  if (provider !== null) closeKeyField(provider);
  pendingFocus = provider !== null ? `key:${provider}` : null;
  render();
}

// The field stays open when the server refuses the key, so the reason shows beside it.
async function saveKey(provider) {
  const field = keyFields.get(provider);
  const key = field ? field.value : "";
  keyNoteMap.delete(provider);
  await put("models", { models: { keys: { [provider]: { key } } } }, () => {
    closeKeyField(provider);
    pendingFocus = `key:${provider}`;
  });
}

async function removeKey(provider) {
  closeKeyField(provider);
  keyNoteMap.delete(provider);
  pendingFocus = `key:${provider}`;
  await put("models", { models: { keys: { [provider]: { clear: true } } } });
}

// The options: one per key's provider, the role's current provider marked as the default when none
// was picked. A provider with no key, or one a vision role may not use, is listed but disabled. An
// off role leads with its reason, disabled.
function roleOptions(role, keys) {
  const options = [];
  if (role.provider === null) options.push({ value: "", text: role.text, disabled: true });
  for (const key of keys) {
    const blocked = role.metaOnly && key.provider !== "meta";
    const tag = role.picked === null && key.provider === role.provider ? " · default" : "";
    const note = !key.set ? " · no key" : blocked ? " · Meta only" : "";
    options.push({ value: key.provider, text: `${key.label}${tag}${note}`, disabled: !key.set || blocked });
  }
  return options;
}

function roleRowNode(role, keys) {
  const path = `models.roles.${role.role}`;
  const test = tests.get(role.role);
  const select = el("select", {
    "aria-label": `${role.role} provider`,
    "data-ctl": `role:${role.role}`,
    "aria-invalid": invalid.has(path) ? "true" : null,
  });
  for (const option of roleOptions(role, keys)) {
    select.append(el("option", { value: option.value, text: option.text, disabled: option.disabled ? true : null }));
  }
  select.value = role.provider ?? "";
  wireTo(select, "change", (event) => pickRole(role.role, event.target.value));
  const nodes = [
    el("span", { class: "name", text: role.role }),
    select,
    act(test?.busy ? "testing…" : "Test", `test:${role.role}`, () => runTest(role.role), test?.busy ? { disabled: true } : {}),
  ];
  if (test && !test.busy) {
    nodes.push(
      el(
        "p",
        { class: "test" },
        test.ok ? chip("passed") : chip("failed"),
        el("span", { class: `msg${test.ok ? "" : " bad"}`, title: test.full, text: test.text }),
      ),
    );
  }
  if (invalid.has(path)) nodes.push(fieldNote(path));
  return el("div", { class: "row roles", "data-path": path }, nodes);
}

function pickRole(role, provider) {
  return put("models", { models: { roles: { [role]: provider } } }, () => tests.delete(role));
}

async function runTest(role) {
  tests.set(role, { busy: true });
  pendingFocus = `test:${role}`;
  render();
  let result = null;
  try {
    const res = await api.request("POST", `/api/settings/models/roles/${encodeURIComponent(role)}/test`);
    result = res.body;
  } catch {
    result = null;
  }
  const text = testText(result ?? { ok: false, error: "failed" });
  tests.set(role, { busy: false, ok: text.ok, text: text.ok ? text.text : text.text.split("\n")[0], full: text.text });
  render();
}

// ---- Harness --------------------------------------------------------------------------------

const HARNESS_CHIPS = {
  ready: ["passed", "ready"],
  "log in": ["warn", "not logged in"],
  installed: ["idle", "installed"],
  "not installed": ["pending", "not found"],
};

function roleWord(role) {
  return role === "worker" ? "workers" : role;
}

function harnessRowNode(row) {
  const [kind, label] = row.tone === "fail" ? ["failed", "error"] : (HARNESS_CHIPS[row.status] ?? ["idle", row.status]);
  const roles = row.selectedRoles.map(roleWord).join(", ");
  const showHint = row.hint && (row.status === "log in" || row.status === "not installed" || row.tone === "fail");
  return el(
    "div",
    { class: "harness", "data-ctl": `harness:${row.id}` },
    el(
      "div",
      { class: "h-line" },
      chip(kind, label),
      el("span", { class: "h-name", text: row.name }),
      row.version ? el("span", { class: "h-ver", text: row.version }) : null,
      roles ? el("span", { class: "h-roles", text: roles }) : null,
    ),
    showHint ? el("p", { class: "hint" }, withCode(row.hint)) : null,
  );
}

function setHarnessRole(role, value) {
  const h = settings.harness ?? {};
  return change("harness", { ...h, roles: { ...(h.roles ?? {}), [role]: value } });
}

// One role and the harness it runs on. Meta is first; a harness that is not ready is listed but
// disabled, and a custom command counts as ready once it has words.
function harnessRoleRow(role, h, rows, customReady) {
  const current = h.roles?.[role] ?? "builtin";
  const select = el("select", { "aria-label": `${role} runs on`, "data-ctl": `hrole:${role}` });
  select.append(el("option", { value: "builtin", text: "Meta · built in" }));
  for (const row of rows) {
    if (row.id === "builtin") continue;
    const ready = row.id === "custom" ? customReady : row.status === "ready";
    select.append(el("option", { value: row.id, text: row.name, disabled: ready ? null : true }));
  }
  if (current !== "builtin" && !rows.some((row) => row.id === current)) {
    select.append(el("option", { value: current, text: current }));
  }
  select.value = current;
  wireTo(select, "change", (event) => setHarnessRole(role, event.target.value));
  return el(
    "div",
    { class: "row wide", "data-path": `harness.roles.${role}` },
    el("span", { class: "label", text: role }),
    select,
    fieldNote(`harness.roles.${role}`),
  );
}

function saveCustom(text) {
  const trimmed = String(text).trim();
  const parts = trimmed === "" ? [] : trimmed.split(/\s+/);
  return change("harness", { ...(settings.harness ?? {}), custom_command: parts }, () => drafts.delete("custom"));
}

async function loadAdapters() {
  adaptersBusy = true;
  render();
  let ok = false;
  try {
    const res = await api.request("GET", "/api/settings/harness/adapters");
    ok = res.status === 200 && Array.isArray(res.body?.adapters);
    if (ok) adapters = res.body.adapters;
  } catch {
    ok = false;
  }
  adaptersBusy = false;
  if (!ok) notes.set("harness", { bad: true, text: "Could not check the harnesses." });
  render();
}

function harnessBody() {
  const h = settings.harness ?? {};
  const rows = harnessRows(adapters ?? [], h);
  const customReady = (h.custom_command ?? []).length > 0;

  const listed = rows.filter((row) => row.id !== "builtin" && row.id !== "custom");
  const recheck = act(adaptersBusy ? "checking…" : "Recheck", "recheck", () => loadAdapters(), adaptersBusy ? { disabled: true } : {});

  const custom = el("input", {
    type: "text",
    "aria-label": "Custom command",
    "data-ctl": "custom",
    spellcheck: "false",
    autocomplete: "off",
    value: drafts.has("custom") ? drafts.get("custom") : (h.custom_command ?? []).join(" "),
    "aria-invalid": invalid.has("harness.custom_command") ? "true" : null,
  });
  wireTo(custom, "input", () => drafts.set("custom", custom.value));
  wireTo(custom, "change", () => saveCustom(custom.value));
  const detect = act("Detect", "detect", () => loadAdapters());

  return [
    el("p", { class: "lead" }, el("strong", { text: "Meta recommended." }), " ", el("span", { class: "muted", text: "Others use your own subscription." })),
    group(
      "Roles",
      null,
      Object.keys(h.roles ?? {}).map((role) => harnessRoleRow(role, h, rows, customReady)),
    ),
    group(
      "Harnesses",
      null,
      adapters === null
        ? [el("p", { class: "note", text: adaptersBusy ? "Checking…" : "Not checked yet." })]
        : listed.map((row) => harnessRowNode(row)),
      recheck,
    ),
    statusLine("harness"),
    group(
      "Custom command",
      null,
      [
        el("div", { class: "detect" }, custom, detect),
        fieldNote("harness.custom_command"),
        rows.filter((row) => row.id === "custom").map((row) => harnessRowNode(row)),
      ],
    ),
    el("p", { class: "gnote" }, withCode("Any MCP agent. `{task}` and `{mcp}` are filled in.")),
    el("p", { class: "foot", text: "Receipts record the harness and model of each step." }),
  ];
}

// ---- Workers and projects -------------------------------------------------------------------

function projectRow(project) {
  const cf = confirmParts(`remove:${project.id}`, {
    arm: "Remove",
    run: "Remove now",
    line: "Leaves the map. Files stay.",
    onRun: () => removeProject(project.id),
  });
  const row = el(
    "div",
    { class: "row proj", "data-path": `projects.${project.id}` },
    el(
      "span",
      { class: "pinfo" },
      el("span", { class: "pname", text: project.name ?? project.id }),
      project.path ? el("span", { class: "ppath", text: project.path }) : null,
    ),
    el("span", { class: "acts" }, cf.buttons),
    cf.line,
    fieldNote(`projects.${project.id}`),
  );
  const statemap = toggleRow(
    "Refine state charts with an LLM (experimental, uses tokens)",
    project.statemap_llm === true,
    (value) => setStatemapLlm(project.id, value),
    { path: `projects.${project.id}.statemap_llm` },
  );
  const hint = statemapHint(settings.budgets);
  return el(
    "div",
    { class: "proj-block" },
    row,
    workersRow(project),
    statemap,
    hint ? el("p", { class: "note", text: hint }) : null,
  );
}

// The worker count, 0 to 8, with a minus and a plus. Each step is saved at once and takes effect
// after restart; a step past either end is disabled.
function workersRow(project) {
  const n = project.workers ?? 0;
  const path = `projects.${project.id}.workers`;
  const step = (delta, label) => {
    const next = stepWorkers(n, delta);
    const button = el(
      "button",
      { type: "button", "aria-label": label, "data-ctl": `workers:${project.id}:${delta}`, disabled: next === n ? true : null },
      delta < 0 ? "−" : "+",
    );
    return wireTo(button, "click", () => setWorkers(project.id, next));
  };
  return el(
    "div",
    { class: "row wide", "data-path": path },
    el("span", { class: "label", text: "Workers" }),
    el(
      "span",
      { class: "stepper", role: "group", "aria-label": `Workers for ${project.id}` },
      step(-1, `Fewer workers for ${project.id}`),
      el("span", { class: "count", "aria-live": "polite", text: String(n) }),
      step(1, `More workers for ${project.id}`),
    ),
    fieldNote(path),
  );
}

function setWorkers(id, n) {
  return put("projects", { projects: { workers: { [id]: n } } });
}

// The flag is saved, not sent on each click; the row shows the server's answer.
function setStatemapLlm(id, value) {
  return put("projects", { projects: { statemap_llm: { [id]: value } } });
}

// Projects are added and removed by path and id, and take effect on restart.
function removeProject(id) {
  armed = null;
  clearTimeout(armTimer);
  return put("projects", { projects: { remove: [id] } });
}

function addProject() {
  const path = (drafts.get("add") ?? "").trim();
  if (path === "") {
    invalid.set("projects.add", "Type a path.");
    render();
    return;
  }
  return put("projects", { projects: { add: [path] } }, () => drafts.delete("add"));
}

function projectsBody() {
  const projects = settings.projects ?? [];
  const addField = el("input", {
    type: "text",
    "aria-label": "Project path",
    "data-ctl": "add",
    placeholder: "path",
    spellcheck: "false",
    autocomplete: "off",
    value: drafts.get("add") ?? "",
    "aria-invalid": invalid.has("projects.add") ? "true" : null,
  });
  wireTo(addField, "input", () => drafts.set("add", addField.value));
  wireTo(addField, "keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      addProject();
    }
  });
  const addRow = el(
    "div",
    { class: "row addrow", "data-path": "projects.add" },
    addField,
    act("Add", "add", () => addProject()),
    fieldNote("projects.add"),
  );
  return [
    group("Projects", null, projects.length ? projects.map((project) => projectRow(project)) : [el("p", { class: "note", text: "No projects yet." })]),
    group("Add a project", "A git repo on this machine.", [addRow]),
    statusLine("projects"),
  ];
}

// ---- Budgets --------------------------------------------------------------------------------

function numberRow(field) {
  const path = `budgets.${field.path}`;
  const stored = getAt(settings.budgets, field.path);
  const shown = drafts.has(path)
    ? drafts.get(path)
    : stored === undefined
      ? ""
      : String(Number((stored / field.scale).toPrecision(12)));
  const why = invalid.get(path);
  const input = el("input", {
    type: "number",
    min: field.min,
    max: field.max,
    step: field.step,
    value: shown,
    "aria-label": field.label,
    "aria-invalid": why ? "true" : null,
    "data-ctl": path,
  });
  wireTo(input, "input", () => drafts.set(path, input.value));
  wireTo(input, "blur", () => commitNumber(field));
  wireTo(input, "keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      commitNumber(field);
    }
  });
  return el(
    "div",
    { class: "row wide", "data-path": path },
    el("span", { class: "label", text: field.label }),
    el("span", { class: "num" }, input, field.unit ? el("span", { class: "unit", text: field.unit }) : null),
    why ? fieldNote(path) : null,
  );
}

// Saves a number on Enter or blur, and only when it is valid and changed.
async function commitNumber(field) {
  const path = `budgets.${field.path}`;
  if (!drafts.has(path) || saving.has(path)) return;
  const parsed = parseField(field, drafts.get(path));
  if (!parsed.ok) {
    invalid.set(path, parsed.why);
    render();
    return;
  }
  invalid.delete(path);
  const before = settings.budgets;
  const body = patchFor("budgets", before, setAt(before, field.path, parsed.value));
  if (!body) {
    drafts.delete(path);
    render();
    return;
  }
  saving.add(path);
  await put("budgets", body, () => drafts.delete(path));
  saving.delete(path);
}

function budgetsBody() {
  return [
    ...BUDGET_GROUPS.map((entry) =>
      group(
        entry.title,
        entry.note,
        entry.paths.map((path) => numberRow(BUDGET_FIELDS.find((field) => field.path === path))),
      ),
    ),
    statusLine("budgets"),
  ];
}

// ---- Look and feel --------------------------------------------------------------------------

function lookBody() {
  const look = settings.look ?? {};
  const themes = seg(
    THEMES.map((theme) => ({
      value: theme.id,
      label: theme.label,
      node: el("span", { class: "sw", "data-swatch": theme.id, "aria-hidden": "true" }),
    })),
    look.theme,
    (value) => change("look", { ...settings.look, theme: value }),
    "theme",
  );
  // The stored values are the server's: sparse, normal and dense.
  const densities = seg(
    [
      { value: "sparse", label: "Few" },
      { value: "normal", label: "Some" },
      { value: "dense", label: "All" },
    ],
    look.label_density,
    (value) => change("look", { ...settings.look, label_density: value }),
    "density",
  );
  // Idle bob has nothing to do when motion is off or reduced.
  const idleOff = look.motion !== true || look.reduced_motion === true;
  return [
    group("Theme", null, [themes, fieldNote("look.theme")]),
    group("Map", null, [
      toggleRow("Top-down by default", look.top_down_default === true, (value) =>
        change("look", { ...settings.look, top_down_default: value }),
      { path: "look.top_down_default" },
      ),
      el("div", { class: "row wide", "data-path": "look.label_density" }, el("span", { class: "label", text: "Labels" }), densities, fieldNote("look.label_density")),
    ]),
    group("Motion", null, [
      toggleRow("Motion", look.motion === true, (value) => change("look", { ...settings.look, motion: value }), {
        path: "look.motion",
      }),
      toggleRow("Idle bob", look.idle_bob === true, (value) => change("look", { ...settings.look, idle_bob: value }), {
        disabled: idleOff,
        path: "look.idle_bob",
      }),
      toggleRow(
        "Reduced motion",
        look.reduced_motion === true,
        (value) => change("look", { ...settings.look, reduced_motion: value }),
        { path: "look.reduced_motion" },
      ),
    ]),
  ];
}

// ---- Access ---------------------------------------------------------------------------------

function copyText(key, text) {
  if (!text) return;
  try {
    navigator.clipboard?.writeText(text).catch(() => {});
  } catch {
    // The clipboard is not available here.
  }
  flash(`copy:${key}`);
}

async function rotateOwner() {
  armed = null;
  clearTimeout(armTimer);
  revealed = false;
  let token = null;
  try {
    const res = await api.request("POST", "/api/settings/tokens/owner/rotate");
    token = res.status === 200 ? tokenFromRotate(res.body) : null;
  } catch {
    token = null;
  }
  if (token === null) {
    notes.set("access", { bad: true, text: "Not rotated. The token is unchanged." });
    render();
    return;
  }
  deps.onToken(token);
  notes.set("access", { bad: false, text: "Owner token rotated." });
  await load();
}

async function rotateAgents() {
  armed = null;
  clearTimeout(armTimer);
  let ok = false;
  try {
    const res = await api.request("POST", "/api/settings/tokens/agents/rotate");
    ok = res.status === 200;
  } catch {
    ok = false;
  }
  notes.set("access", { bad: !ok, text: ok ? "Agent tokens rotated." : "Not rotated. The tokens are unchanged." });
  await load();
}

function accessBody() {
  const tokens = settings.tokens ?? {};
  if (!hasTokens(tokens)) {
    return [group("Tokens", null, [el("p", { class: "note", text: "Not available on this server yet." })]), statusLine("access")];
  }
  const current = deps.token();
  const shown = revealed && current ? current : (tokens.owner ?? "");
  const owner = confirmParts("rotate:owner", {
    arm: "Rotate",
    run: "Rotate now",
    line: "The old token stops working.",
    onRun: rotateOwner,
  });
  const ownerRow = el(
    "div",
    { class: "row tokrow" },
    el("span", { class: "name", text: "Owner token" }),
    el(
      "span",
      { class: "acts" },
      act(revealed ? "Hide" : "Show", "show:owner", () => {
        revealed = !revealed;
        render();
      }),
      act("Copy", "copy:owner", () => copyText("owner", current), current ? {} : { disabled: true }),
      flashes.has("copy:owner") ? el("span", { class: "copied", text: "copied" }) : null,
      owner.buttons,
    ),
    el("span", { class: "tok fill", text: shown }),
    owner.line,
  );
  const agents = (tokens.agents ?? []).map((agent) =>
    el(
      "div",
      { class: "row tokrow" },
      el("span", { class: "name", text: agent.id }),
      el("span", { class: "tok fill", text: agent.masked }),
    ),
  );
  const agentRotate = confirmParts("rotate:agents", {
    arm: "Rotate",
    run: "Rotate now",
    line: "The old token stops working.",
    onRun: rotateAgents,
  });
  const agentRow = el(
    "div",
    { class: "row tokrow" },
    el("span", { class: "name", text: "All agents" }),
    el("span", { class: "acts" }, agentRotate.buttons),
    agentRotate.line,
  );
  return [
    group("Owner token", null, [ownerRow]),
    group("Agent tokens", null, [...agents, agentRow]),
    statusLine("access"),
  ];
}

// ---- Notifications --------------------------------------------------------------------------

function askPermission() {
  try {
    Promise.resolve(Notification.requestPermission()).finally(() => render());
  } catch {
    render();
  }
}

function notificationsBody() {
  const n = settings.notifications ?? {};
  const set = (key, value) => change("notifications", { ...settings.notifications, [key]: value });
  const supported = "Notification" in window;
  const permission = supported ? Notification.permission : "unsupported";
  let state = null;
  if (!supported) state = el("span", { class: "muted", text: "not in this browser" });
  else if (permission === "granted") state = el("span", { class: "muted", text: "allowed" });
  else if (permission === "denied") state = el("span", { class: "muted", text: "blocked in browser" });
  else state = act("Allow", "allow", () => askPermission());
  const hint = permission === "denied" ? el("p", { class: "fill note", text: "Change it in site settings." }) : null;
  return [
    group("Alerts", null, [
      toggleRow("Needs-you alerts", n.needs_you === true, (value) => set("needs_you", value), { path: "notifications.needs_you" }),
      toggleRow("Sound", n.sound === true, (value) => set("sound", value), { path: "notifications.sound" }),
      toggleRow(
        "Browser notifications",
        n.browser === true,
        (value) => {
          if (value && supported && Notification.permission === "default") askPermission();
          set("browser", value);
        },
        { path: "notifications.browser", before: state, after: hint },
      ),
    ]),
    statusLine("notifications"),
  ];
}

// ---- Data -----------------------------------------------------------------------------------

// The export choice and whether the server has the data routes. A 404 turns both off, so an older
// server shows the section without failing.
let exportWhat = "receipts";
let exportBusy = false;
let dataMissing = false;

const EXPORT_CHOICES = [
  { value: "receipts", label: "Receipts" },
  { value: "replay", label: "Replays" },
  { value: "all", label: "All" },
];

function saveBlob(blob, name) {
  const url = URL.createObjectURL(blob);
  const link = el("a", { href: url, download: name });
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

// Downloads the zip for the chosen export. Nothing is saved on a refusal.
async function exportZip() {
  if (exportBusy) return;
  exportBusy = true;
  notes.delete("data");
  render();
  const what = exportWhat;
  let res = { status: 0, body: null };
  try {
    res = await api.download(apiPath(target(), `/api/settings/data/export?what=${what}`));
  } catch {
    res = { status: 0, body: null };
  }
  exportBusy = false;
  if (res.status === 200 && res.blob) saveBlob(res.blob, `acan-${what}.zip`);
  if (res.status === 404) dataMissing = true;
  notes.set("data", exportNote(res.status, res.body));
  render();
}

// Clears the local receipts and replays on the server. The answer lists what went and what stayed.
async function clearData() {
  armed = null;
  clearTimeout(armTimer);
  let res = { status: 0, body: null };
  try {
    res = await api.request("POST", apiPath(target(), "/api/settings/data/clear"), { what: "local", confirm: true });
  } catch {
    res = { status: 0, body: null };
  }
  if (res.status === 404) dataMissing = true;
  notes.set("data", clearNote(res.status, res.body));
  render();
}

function dataBody() {
  const choices = seg(
    EXPORT_CHOICES.map((choice) => ({ value: choice.value, label: choice.label })),
    exportWhat,
    (value) => {
      exportWhat = value;
      render();
    },
    "what",
  );
  const exportButton = act(exportBusy ? "Exporting…" : "Export zip", "export", () => exportZip(), {
    disabled: exportBusy || dataMissing ? true : null,
  });
  const clear = confirmParts("clear", {
    arm: "Clear",
    run: "Clear now",
    line: "Cannot be undone.",
    onRun: clearData,
  });
  return [
    group("Export", "A zip of what the page has recorded.", [
      el("div", { class: "row wide" }, el("span", { class: "label", text: "Include" }), choices, exportButton),
    ]),
    group("Local data", null, [
      el(
        "div",
        { class: "row wide" },
        el("span", { class: "label", text: "Clear local data" }),
        el("span", { class: "acts" }, dataMissing ? null : clear.buttons),
        clear.line,
      ),
    ]),
    statusLine("data"),
  ];
}

// ---- About and update -----------------------------------------------------------------------

// The drawer asks GET /api/settings/update/check, and an owner's install posts to
// POST /api/settings/update. A member can check but not install.
async function checkUpdate() {
  updateBusy = true;
  render();
  let ok = false;
  try {
    const res = await api.request("GET", "/api/settings/update/check");
    ok = res.status === 200 && res.body !== null;
    if (ok) update = res.body;
  } catch {
    ok = false;
  }
  updateBusy = false;
  updateStatus = ok ? null : { state: "check-failed" };
  render();
}

// The page posts once and shows the answer: installing, downloaded, or the reason it was refused.
async function startUpdate() {
  updateStatus = { state: "starting" };
  render();
  let note;
  try {
    const res = await api.request("POST", "/api/settings/update");
    note = installNote(res.status, res.body);
  } catch {
    note = installNote(0, null);
  }
  updateStatus = note;
  render();
}

function updateNodes() {
  const plain = (text, cls = "") => el("p", { class: `ustatus ${cls}`.trim(), text });
  const owner = !ro();
  if (updateBusy) return [plain("checking…"), act("Check", "check", () => checkUpdate(), { disabled: true })];
  const state = updateStatus?.state ?? null;
  if (state === "starting") return [plain("Starting…")];
  if (state === "installing" || state === "downloaded") return [plain(updateStatus.text, "ink")];
  if (state === "failed") {
    const nodes = [el("p", { class: "ustatus bad" }, chip("failed", "not installed")), plain(updateStatus.text)];
    if (owner) nodes.push(act("Retry", "retry", () => startUpdate()));
    return nodes;
  }
  if (state === "check-failed") {
    return [
      el("p", { class: "ustatus bad" }, chip("failed", "check failed")),
      plain("Could not check for updates. Nothing changed."),
      act("Retry", "retry", () => checkUpdate()),
    ];
  }
  if (update) {
    const view = updateView(update);
    const nodes = [plain(view.text, view.state === "available" ? "ink" : "")];
    if (view.state === "available" && owner) {
      nodes.push(act("Update", "update", () => startUpdate(), { class: "primary" }));
    }
    nodes.push(act("Check", "check", () => checkUpdate()));
    return nodes;
  }
  return [plain("Not checked yet."), act("Check", "check", () => checkUpdate())];
}

function aboutBody() {
  const about = settings.about ?? {};
  const line = [about.version ? `v${about.version}` : "", about.commit ?? ""].filter(Boolean).join(" · ");
  const version = el(
    "div",
    { class: "row wide" },
    el("span", { class: "ver", text: line }),
    flashes.has("copy:about") ? el("span", { class: "copied", text: "copied" }) : null,
    act("Copy", "copy:about", () => copyText("about", line)),
  );
  return [
    el("p", { class: "note", text: "Channel: pre-release." }),
    group("Version", null, [version]),
    ro() ? null : group("Update", null, [el("div", { class: "ublock" }, updateNodes())]),
    statusLine("about"),
  ];
}
