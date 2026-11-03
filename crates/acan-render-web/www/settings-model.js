// The settings drawer's data, with no DOM, so Node can test it. The server's GET /api/settings
// answer (the shape of fixtures/settings.json) goes in, and the rows and texts the drawer draws
// come out. A key never passes through here: the page sends it once and does not keep it.
import { providerLabel } from "./setup.js";
import { THEMES } from "./themes.js";

export { providerLabel };

export const SECTIONS = [
  { id: "models", title: "Models" },
  { id: "harness", title: "Harness" },
  { id: "projects", title: "Workers and projects" },
  { id: "budgets", title: "Budgets and rules" },
  { id: "look", title: "Look and feel" },
  { id: "access", title: "Access" },
  { id: "notifications", title: "Notifications" },
  { id: "data", title: "Data" },
  { id: "about", title: "About and update" },
];

// Access holds the owner's tokens, so only the owner sees it.
export function sectionsFor(settings) {
  if (settings?.you === "owner") return SECTIONS;
  return SECTIONS.filter((section) => section.id !== "access");
}

export const THEME_IDS = THEMES.map((theme) => theme.id);

export function themeLabel(id) {
  return THEMES.find((theme) => theme.id === id)?.label ?? id;
}

function sourceText(source) {
  if (source === "environment") return "from environment";
  return source === "saved" ? "saved" : "";
}

export function keyRows(models) {
  return (models?.keys ?? []).map(({ provider, set, source }) => ({
    provider,
    label: providerLabel(provider),
    set: set === true,
    text: set === true ? "set" : "not set",
    source: sourceText(source),
  }));
}

// The text is the provider's label. An off role says so, and gives the server's reason if any.
export function roleRows(models) {
  return (models?.roles ?? []).map(({ role, provider, picked, off_reason, meta_only }) => {
    const offReason = off_reason ?? null;
    const name = provider ? providerLabel(provider) : null;
    let text = "off";
    if (name) text = name;
    else if (offReason) text = `off · ${offReason}`;
    return {
      role,
      provider: provider || null,
      picked: picked ?? null,
      offReason,
      metaOnly: meta_only === true,
      providerLabel: name,
      text,
    };
  });
}

function harnessStatus(installed, auth) {
  if (!installed || auth === "not_installed") return "not installed";
  if (auth === "logged_in") return "ready";
  if (auth === "logged_out") return "log in";
  return "installed";
}

// Red is for failures only, and v1 has no adapter failure, so every row is quiet.
export function harnessRows(adapters, harness) {
  const roles = harness?.roles ?? {};
  return (adapters ?? []).map((adapter) => {
    const { id, name, installed, auth, version, hint } = adapter;
    return {
      id,
      name,
      installed: installed === true,
      auth,
      version: version ?? null,
      hint: hint ?? "",
      status: harnessStatus(installed, auth),
      tone: "quiet",
      selectedRoles: Object.keys(roles).filter((role) => roles[role] === id),
    };
  });
}

const field = (path, label, unit, scale, min, max, step) => ({
  path,
  label,
  unit,
  scale,
  min,
  max,
  step,
});

// `scale` turns the stored value into the shown unit: 5000000 micros show as 5 dollars.
export const BUDGET_FIELDS = [
  field("repair_rounds", "Repair rounds", "", 1, 0, 10, 1),
  field("max_episodes", "Episode cap", "", 1, 1, 1000, 1),
  field("wall_time_ms", "Wall time", "min", 60000, 1, 1440, 1),
  field("model_cost_micros", "Cost cap", "$", 1000000, 0, 1000, 0.5),
  field("call_limits.foreman", "Foreman", "calls", 1, 0, 10000, 1),
  field("call_limits.worker", "Worker", "calls", 1, 0, 10000, 1),
  field("call_limits.fast", "Fast", "calls", 1, 0, 10000, 1),
  field("call_limits.vision", "Vision", "calls", 1, 0, 10000, 1),
  field("call_limits.director", "Director", "calls", 1, 0, 10000, 1),
  field("call_limits.wonder", "Wonder", "calls", 1, 0, 10000, 1),
  field("call_limits.statemap", "State map", "calls", 1, 0, 10000, 1),
  field("stuck_declare_ms", "Stuck after", "min", 60000, 1, 1440, 1),
  field("stuck_build_ms", "Stuck while building", "min", 60000, 1, 1440, 1),
  field("drift_threshold", "Drift threshold", "", 1, 0, 1, 0.05),
];

export function getAt(obj, path) {
  return path.split(".").reduce((node, key) => node?.[key], obj);
}

// A copy of `obj` with the dotted `path` set to `value`. `obj` itself is not changed.
export function setAt(obj, path, value) {
  const [head, ...rest] = path.split(".");
  const base = obj ?? {};
  const next = rest.length ? setAt(base[head], rest.join("."), value) : value;
  return { ...base, [head]: next };
}

// Reads what the user typed in the field's shown unit and returns the stored value.
export function parseField(field, text) {
  const typed = String(text ?? "").trim();
  const n = typed === "" ? NaN : Number(typed);
  if (!Number.isFinite(n)) return { ok: false, why: "must be a number" };
  if (Number.isInteger(field.step) && !Number.isInteger(n)) {
    return { ok: false, why: "must be a whole number" };
  }
  if (n < field.min || n > field.max) {
    return { ok: false, why: `must be ${field.min} to ${field.max}` };
  }
  // toPrecision drops float noise, such as 1.1 * 1000000 landing a hair off 1100000.
  return { ok: true, value: Number((n * field.scale).toPrecision(12)) };
}

const isObject = (value) => value !== null && typeof value === "object" && !Array.isArray(value);

// The section's object, whether the caller passes the section or the whole settings object.
function sectionOf(section, value) {
  return value?.[section] ?? value ?? {};
}

// The leaves of `after` that differ from `before`, nested the same way, or null when none differ.
// Arrays count as one leaf.
function changedLeaves(before, after) {
  const out = {};
  for (const key of Object.keys(after)) {
    const was = before[key];
    const now = after[key];
    if (isObject(was) && isObject(now)) {
      const inner = changedLeaves(was, now);
      if (inner) out[key] = inner;
    } else if (JSON.stringify(was) !== JSON.stringify(now)) {
      out[key] = now;
    }
  }
  return Object.keys(out).length > 0 ? out : null;
}

// The PUT body fragment for one section, holding only what changed, or null when nothing did.
export function patchFor(section, before, after) {
  const changed = changedLeaves(sectionOf(section, before), sectionOf(section, after));
  return changed ? { [section]: changed } : null;
}

// The 422 answer's fields as a map from path to reason.
export function fieldErrors(fields) {
  return new Map((Array.isArray(fields) ? fields : []).map(({ path, why }) => [path, why]));
}

export function tokenFromRotate(body) {
  const token = body?.token;
  return typeof token === "string" && token.trim() !== "" ? token : null;
}

// A Test answer as the row shows it: the latency when it worked, else the error or "failed".
export function testText(result) {
  if (result?.ok === true) return { ok: true, text: `ok · ${result.ms} ms` };
  return { ok: false, text: result?.error || "failed" };
}

function keyNote(state) {
  if (state === "stored") return "Key saved.";
  if (state === "rejected") return "Key rejected by the provider.";
  if (state === "unreachable") return "Provider unreachable; key kept.";
  return "";
}

// One sentence per provider in the PUT answer's `keys` map. An unknown state gives "".
export function keyNotes(keys) {
  return Object.values(keys ?? {}).map(keyNote);
}

// The dotted path of each field a PUT body changes, one per leaf: {budgets:{call_limits:{worker:5}}}
// gives "budgets.call_limits.worker". Only the paths are kept, never the values.
export function changedPaths(body, base = "") {
  const out = [];
  for (const [key, value] of Object.entries(body ?? {})) {
    const path = base ? `${base}.${key}` : key;
    if (value && typeof value === "object" && !Array.isArray(value)) out.push(...changedPaths(value, path));
    else out.push(path);
  }
  return out;
}

// How one changed field takes effect. The server keys `applied` by dotted path, so an exact match
// wins, then the longest prefix (projects.workers covers projects.workers.acan). The older answer
// keyed whole sections, which the same lookup reads as prefixes. Unknown gives undefined.
export function appliedFor(applied, path) {
  if (!applied || typeof applied !== "object") return undefined;
  if (typeof applied[path] === "string") return applied[path];
  for (let at = path.lastIndexOf("."); at > 0; at = path.lastIndexOf(".", at - 1)) {
    const prefix = path.slice(0, at);
    if (typeof applied[prefix] === "string") return applied[prefix];
  }
  return undefined;
}

// The line under a saved change. "restart" says so; "stored" is saved and no more; "live" and
// "client" need no line, since the change shows at once. Without paths, every key in `applied` counts.
export function appliedNote(applied, paths = Object.keys(applied ?? {})) {
  const values = paths.map((path) => appliedFor(applied, path));
  if (values.includes("restart")) return "Saved; takes effect after restart.";
  if (values.includes("stored")) return "Saved.";
  return "";
}

// The line under the statemap toggle: the most a scan can spend, from the statemap call limit. No
// limit means no figure, so no line.
export function statemapHint(budgets) {
  const calls = getAt(budgets, "call_limits.statemap");
  if (!Number.isFinite(calls)) return "";
  return `Uses up to ${calls} calls × 3k tokens per scan`;
}

// The server's 409 names its conflict in `what`. Each code gets a short sentence; an unknown code
// gets the plain fallback.
const CONFLICT_TEXT = {
  routing_file: "A routing file sets this role. Change it there. Nothing changed.",
  no_key: "That provider has no key yet. Add one first. Nothing changed.",
  unavailable: "That provider is unavailable right now. Nothing changed.",
  "this key comes from the environment": "This key comes from the environment. Unset it there.",
};

export function conflictNote(body) {
  return CONFLICT_TEXT[body?.what] ?? "Not saved: a model setting conflicts. Nothing changed.";
}

// A worker count runs from 0 to 8 per project, and the stepper moves it by one.
export const WORKERS = { min: 0, max: 8 };

export function stepWorkers(current, delta) {
  const n = Number.isInteger(current) ? current : WORKERS.min;
  return Math.min(WORKERS.max, Math.max(WORKERS.min, n + delta));
}

// The tokens come from GET /api/settings once the token routes land. Until then `tokens` is {}.
export function hasTokens(tokens) {
  return typeof tokens?.owner === "string" && tokens.owner !== "";
}

export function updateView(update) {
  const { current, latest } = update ?? {};
  if (latest === current) return { state: "current", text: `Up to date · ${current}` };
  return { state: "newer", text: `${latest} available` };
}

const REFUSED = {
  0: "Could not reach acan. Nothing changed.",
  403: "Only the owner can do this. Nothing changed.",
};

// A data export's answer. A 409 with no recorded run is not a failure: receipts still export.
export function exportNote(status, body) {
  if (status === 200) return { bad: false, text: "Export downloaded." };
  if (status === 409 && body?.what === "no_recorded_run") {
    return { bad: false, text: "No recorded run yet. Receipts still export." };
  }
  if (status === 404) return { bad: false, text: "Not on this server yet." };
  return { bad: true, text: REFUSED[status] ?? "Not exported. Nothing changed." };
}

// A clear's answer: what the server cleared and what it kept.
export function clearNote(status, body) {
  if (status === 200) {
    const names = (value) => (Array.isArray(value) ? value.map(String).join(", ") : "");
    return { bad: false, text: `Cleared: ${names(body?.cleared) || "nothing"}. Kept: ${names(body?.kept) || "nothing"}.` };
  }
  if (status === 404) return { bad: false, text: "Not on this server yet." };
  return { bad: true, text: REFUSED[status] ?? "Not cleared. Nothing changed." };
}
