// The usage panel's data, with no DOM, so Node can test it. The origin's usage endpoint counts
// every model call the server has made since it started, with the role, provider and model of
// each. The page reads it with the owner token. Calls the offline stand-in answered are recorded
// with the provider `offline`: they are shown as a stand-in and left out of the live total.
import { apiPath, target } from "./projects.js";

export const OFFLINE_PROVIDER = "offline";

// The name a role has on the panel. `vision` is the advisory vision witness, `director` shows as
// `tour`, and `scope` shows as `scope`.
const ROLE_LABELS = { vision: "vision witness", director: "tour", scope: "scope" };

export function roleLabel(role) {
  return ROLE_LABELS[role] ?? role;
}

// The foreman plate shows this many history rows. The view carries the same count.
export const HISTORY_ROWS = 3;

// A tour's detail names its receipt, then the director's verdict.
const TOUR_DETAIL = /^(tour for receipt [0-9a-fA-F]{8}):(.*)$/;

// A history row as the foreman plate shows it: the model, the step (the role's label), and the
// title, which is the row's full detail on hover, or null when the row has none. A tour whose
// detail matches TOUR_DETAIL adds its verdict to the step: `directed` when the director admitted
// it, `deterministic` otherwise.
export function foremanLine(row) {
  const model = row.model;
  const title = row.detail || null;
  const tour = TOUR_DETAIL.exec(row.detail ?? "");
  let step = roleLabel(row.role);
  if (row.role === "director" && tour !== null) {
    step += tour[2].trim() === "admitted" ? " · directed" : " · deterministic";
  }
  return { model, step, title };
}

// Splits the origin's rows into live provider calls and offline stand-in calls. The live total
// counts only the live rows.
export function splitUsage(rows) {
  const live = rows.filter((row) => row.provider !== OFFLINE_PROVIDER);
  const offline = rows.filter((row) => row.provider === OFFLINE_PROVIDER);
  const total = live.reduce(
    (sum, row) => ({
      calls: sum.calls + row.calls,
      input: sum.input + row.prompt_tokens,
      output: sum.output + row.completion_tokens,
    }),
    { calls: 0, input: 0, output: 0 },
  );
  return { live, offline, total };
}

// Asks the origin for its usage, one request at a time. An event that arrives while a request is
// out sets `usageDirty`, and the request is sent again once the first one answers, so the panel is
// never more than one request behind. `onChange` runs after every answer, failed or not.
export function requestUsage(state, fetchText, onChange) {
  state.usageDirty = true;
  if (state.usageInFlight || !state.token) return;
  state.usageDirty = false;
  state.usageInFlight = true;
  fetchText(apiPath(target(), "/api/usage"))
    .then((text) => {
      const rows = JSON.parse(text);
      if (!Array.isArray(rows)) throw new Error("the usage answer is not a list");
      state.serverUsage = { rows, error: null };
    })
    .catch((err) => {
      state.serverUsage = { rows: [], error: String(err?.message ?? err) };
    })
    .finally(() => {
      state.usageInFlight = false;
      onChange();
      if (state.usageDirty) requestUsage(state, fetchText, onChange);
    });
}
