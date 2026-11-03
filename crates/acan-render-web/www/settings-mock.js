// A stand-in for the settings API, for the fixture mode and the Node tests. It keeps its own copy
// of the settings and checks each write the way the server does. The copy is replaced only when a
// write passes, so a 422 changes nothing.
import { BUDGET_FIELDS, THEME_IDS, changedPaths, getAt, parseField } from "./settings-model.js";

const LABEL_DENSITIES = ["sparse", "normal", "dense"];
// How each section takes effect: live, client, restart, or stored (saved, no server path reads it yet).
const APPLY = {
  models: "live",
  harness: "stored",
  budgets: "stored",
  look: "client",
  notifications: "client",
  projects: "restart",
};
const EXPORTS = ["receipts", "replay", "all"];
const RECHECK = "Install it, then Recheck.";
const CODEX_LOGIN = "Run `codex login`, then Recheck.";
const makeAdapter = (id, name, installed, version, auth, hint = "") => ({
  id,
  name,
  installed,
  version,
  auth,
  hint,
});
const ADAPTERS = [
  makeAdapter("builtin", "Built-in · Meta Muse Spark Open", true, null, "logged_in"),
  makeAdapter("claude_code", "Claude Code", true, "1.0.42", "logged_in"),
  makeAdapter("codex", "Codex CLI", true, "0.31.0", "logged_out", CODEX_LOGIN),
  makeAdapter("cursor", "Cursor CLI", false, null, "not_installed", RECHECK),
  makeAdapter("grok", "Grok Build", true, "0.4.1", "unknown"),
  makeAdapter("opencode", "OpenCode", true, "0.6.3", "unknown"),
  makeAdapter("antigravity", "Antigravity", false, null, "not_installed", RECHECK),
  makeAdapter("fx", "fx", true, "2.1.0", "logged_in"),
  makeAdapter("custom", "Custom command", true, null, "unknown"),
];
const UPDATE = {
  latest: "0.1.1-pre.1",
  notes: "Pre-release notes for the fixture.",
  asset: "acan-0.1.1-pre.1-linux.tar.gz",
  sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
};

const clone = (value) => JSON.parse(JSON.stringify(value));
const isObject = (value) => value !== null && typeof value === "object" && !Array.isArray(value);
const mask = (prefix, token) => `${prefix}••••••${token.slice(-4)}`;
const forbidden = () => ({ status: 403, body: { error: "forbidden" } });

// Deep-merges `patch` into `target`. Arrays and other values are replaced.
function merge(target, patch) {
  for (const [key, value] of Object.entries(patch)) {
    if (isObject(value) && isObject(target[key])) merge(target[key], value);
    else target[key] = value;
  }
  return target;
}

function setKey(next, provider, set) {
  const value = { provider, set, source: set ? "saved" : null };
  const row = next.models.keys.find((key) => key.provider === provider);
  if (row) Object.assign(row, value);
  else next.models.keys.push(value);
}

// Applies the key and role changes to `next`. A role's provider must be on the keys list once the
// key changes are in. Returns the `keys` map for the answer.
function applyModels(next, models, fields) {
  const keys = {};
  for (const [provider, change] of Object.entries(models.keys ?? {})) {
    if (change?.clear === true) {
      setKey(next, provider, false);
    } else if (typeof change?.key === "string" && change.key.trim() === "") {
      fields.push({ path: `models.keys.${provider}`, why: "empty key" });
    } else if (typeof change?.key === "string") {
      setKey(next, provider, true);
      keys[provider] = "stored";
    }
  }
  for (const [role, provider] of Object.entries(models.roles ?? {})) {
    const row = next.models.roles.find((entry) => entry.role === role);
    const onList = provider === null || next.models.keys.some((key) => key.provider === provider);
    const path = `models.roles.${role}`;
    if (!row) fields.push({ path, why: "unknown role" });
    else if (!onList) fields.push({ path, why: "unknown provider" });
    else Object.assign(row, { provider, picked: provider, off_reason: provider ? null : "off" });
  }
  return keys;
}

// The field errors for the budgets, the look and the harness. Models are checked as they apply.
function invalidSettings(body) {
  const fields = [];
  for (const field of BUDGET_FIELDS) {
    const value = getAt(body.budgets, field.path);
    if (value === undefined) continue;
    const check = typeof value === "number"
      ? parseField(field, String(value / field.scale))
      : { ok: false, why: "must be a number" };
    if (!check.ok) fields.push({ path: `budgets.${field.path}`, why: check.why });
  }
  const { theme, label_density: density } = body.look ?? {};
  if (theme !== undefined && !THEME_IDS.includes(theme)) {
    fields.push({ path: "look.theme", why: "unknown theme" });
  }
  if (density !== undefined && !LABEL_DENSITIES.includes(density)) {
    fields.push({ path: "look.label_density", why: "unknown label density" });
  }
  for (const [role, id] of Object.entries(body.harness?.roles ?? {})) {
    if (!ADAPTERS.some((entry) => entry.id === id)) {
      fields.push({ path: `harness.roles.${role}`, why: "unknown harness" });
    }
  }
  const command = body.harness?.custom_command;
  const isList = Array.isArray(command) && command.every((arg) => typeof arg === "string");
  if (command !== undefined && !isList) {
    fields.push({ path: "harness.custom_command", why: "must be a list of strings" });
  }
  return fields;
}

// A project's statemap flag must be on or off, and its worker count a whole number from 0 to 8. Each
// must name a project on the list.
function invalidProjects(projects, body = {}) {
  const fields = [];
  const known = (id) => projects.some((project) => project.id === id);
  for (const [id, on] of Object.entries(body.statemap_llm ?? {})) {
    if (!known(id)) fields.push({ path: `projects.${id}`, why: "unknown project" });
    else if (typeof on !== "boolean") fields.push({ path: `projects.${id}.statemap_llm`, why: "must be on or off" });
  }
  for (const [id, n] of Object.entries(body.workers ?? {})) {
    if (!known(id)) fields.push({ path: `projects.${id}`, why: "unknown project" });
    else if (!Number.isInteger(n) || n < 0 || n > 8) fields.push({ path: `projects.${id}.workers`, why: "must be 0 to 8" });
  }
  return fields;
}

// The model conflicts, checked against the settings before the write, as the codes the server
// sends in `what`: clearing an environment key, or a role on a provider with no key (unless this
// write adds that key).
function modelConflicts(current, models) {
  const conflicts = [];
  for (const [provider, change] of Object.entries(models.keys ?? {})) {
    const row = current.models.keys.find((key) => key.provider === provider);
    if (change?.clear === true && row?.source === "environment") conflicts.push("this key comes from the environment");
  }
  for (const [, provider] of Object.entries(models.roles ?? {})) {
    if (provider === null) continue;
    const row = current.models.keys.find((key) => key.provider === provider);
    const addedHere = typeof models.keys?.[provider]?.key === "string";
    if (!addedHere && !row?.set) conflicts.push("no_key");
  }
  return conflicts;
}

// Projects are added or removed by id, and take effect on restart.
function projectsAfter(projects, { add = [], remove = [] }) {
  const kept = projects.filter((project) => !remove.includes(project.id));
  for (const path of add) {
    const id = path.split("/").filter(Boolean).pop() ?? path;
    if (!kept.some((project) => project.id === id)) kept.push({ id, name: id, workers: 1 });
  }
  return kept;
}

export function createMockApi(initial) {
  let state = clone(initial);
  let ownerToken = "ac_own_mock7d2e";
  let ownerRotations = 0;
  let agentRotations = 0;
  // Replays need a recorded run. The fixture has none yet, so they answer 409 until one exists.
  let hasRun = false;
  const member = state.you !== "owner";

  // A member sees no tokens, and every setting reads as read only.
  function view(settings) {
    const copy = clone(settings);
    if (member) {
      delete copy.tokens;
      copy.editable = false;
    }
    return copy;
  }

  function put(body) {
    if (member) return forbidden();
    const next = clone(state);
    const fields = [];
    const keys = applyModels(next, body.models ?? {}, fields);
    fields.push(...invalidSettings(body), ...invalidProjects(state.projects, body.projects));
    if (fields.length) return { status: 422, body: { error: "invalid", fields } };
    const conflicts = modelConflicts(state, body.models ?? {});
    if (conflicts.length) return { status: 409, body: { error: "conflict", what: conflicts[0] } };

    if (body.harness) merge(next.harness, body.harness);
    if (body.budgets) merge(next.budgets, body.budgets);
    if (body.look) merge(next.look, body.look);
    if (body.notifications) merge(next.notifications, body.notifications);
    if (body.projects) next.projects = projectsAfter(next.projects, body.projects);
    for (const [id, on] of Object.entries(body.projects?.statemap_llm ?? {})) {
      next.projects = next.projects.map((project) => (project.id === id ? { ...project, statemap_llm: on } : project));
    }
    for (const [id, n] of Object.entries(body.projects?.workers ?? {})) {
      next.projects = next.projects.map((project) => (project.id === id ? { ...project, workers: n } : project));
    }
    state = next;

    // One entry per changed field, keyed by its dotted path. A project's flag applies live; a worker
    // count, or a project added or removed, applies at restart.
    const applied = {};
    for (const path of changedPaths(body)) {
      const [section, field] = path.split(".");
      if (section === "projects") {
        const restarts = ["add", "remove", "workers"].includes(field);
        applied[`projects.${field}`] = restarts ? "restart" : "live";
      } else if (section in APPLY) applied[path] = APPLY[section];
    }
    return { status: 200, body: { settings: view(next), applied, keys } };
  }

  function testRole(role) {
    const row = state.models.roles.find((entry) => entry.role === role);
    if (!row) return { status: 404, body: { error: "unknown role" } };
    const { provider } = row;
    if (state.models.keys.some((key) => key.provider === provider && key.set)) {
      const result = { role, ok: true, provider, model: "mock-model", ms: 412, error: null };
      return { status: 200, body: result };
    }
    const reason = provider === null ? "role is off" : `no key for ${provider}`;
    const error = row.off_reason ?? reason;
    return { status: 200, body: { role, ok: false, provider, model: null, ms: 0, error } };
  }

  function rotateOwner() {
    ownerRotations += 1;
    ownerToken = `mock-owner-${ownerRotations}`;
    state.tokens.owner = mask("ac_own_", ownerToken);
    return { status: 200, body: { token: ownerToken } };
  }

  function rotateAgents() {
    agentRotations += 1;
    state.tokens.agents = state.tokens.agents.map((agent, i) => ({
      id: agent.id,
      masked: mask("ac_agt_", `mock-agent-${agentRotations}-${i}`),
    }));
    return { status: 200, body: { rotated: state.tokens.agents.length } };
  }

  // Clears the local receipts and replays. The server keeps the keys, settings and projects.
  function clearData(body) {
    if (body?.what !== "local" || body?.confirm !== true) return { status: 400, body: { error: "bad request" } };
    hasRun = false;
    return { status: 200, body: { cleared: ["receipts", "replays"], kept: ["keys", "settings", "projects"] } };
  }

  // The export is a zip, so it comes back as a blob, not JSON.
  function exportZip(path) {
    const match = /^\/api(?:\/p\/[^/]+)?\/settings\/data\/export\?what=([a-z]+)$/.exec(path);
    if (!match) return { status: 404, body: { error: "not found" } };
    if (member) return forbidden();
    const what = match[1];
    if (!EXPORTS.includes(what)) return { status: 400, body: { error: "bad what" } };
    if (what !== "receipts" && !hasRun) return { status: 409, body: { error: "conflict", what: "no_recorded_run" } };
    return { status: 200, blob: new Blob([`fixture ${what}`], { type: "application/zip" }) };
  }

  async function download(path) {
    return exportZip(path);
  }

  async function request(method, path, body = {}) {
    if (method === "GET" && path === "/api/settings") return { status: 200, body: view(state) };
    if (method === "POST" && /^\/api(?:\/p\/[^/]+)?\/settings\/data\/clear$/.test(path)) {
      return member ? forbidden() : clearData(body);
    }
    if (method === "PUT" && path === "/api/settings") return put(body);
    if (method === "GET" && path === "/api/settings/harness/adapters") {
      return { status: 200, body: { adapters: ADAPTERS } };
    }
    if (method === "GET" && path === "/api/update") {
      return { status: 200, body: { current: state.about.version, ...UPDATE } };
    }
    if (method === "POST" && path === "/api/update") {
      return member ? forbidden() : { status: 200, body: { state: "verifying" } };
    }
    const role = /^\/api\/settings\/models\/roles\/([^/]+)\/test$/.exec(path);
    if (method === "POST" && role) return member ? forbidden() : testRole(role[1]);
    if (path.startsWith("/api/settings/tokens/")) {
      if (member) return forbidden();
      if (method === "GET" && path === "/api/settings/tokens/owner") {
        return { status: 200, body: { token: ownerToken } };
      }
      if (method === "POST" && path === "/api/settings/tokens/owner/rotate") return rotateOwner();
      if (method === "POST" && path === "/api/settings/tokens/agents/rotate") return rotateAgents();
    }
    return { status: 404, body: { error: "not found" } };
  }

  return { request, download };
}
