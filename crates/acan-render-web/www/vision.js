// The vision witness as the map shows it, with no DOM, so Node can test it. The server answers
// GET /api/tasks/{task}/vision with the record of the task's latest gate run (`vision_witness/1`);
// this file says what each record means. Every verdict is advisory: the gate and the land never read
// it, so no verdict here is a pass or a fail, and `not_checked` is never shown as `consistent`.

export const ADVISORY = "Advisory only: a model compared these pages, and the gate never reads its verdict.";

// The record a server with no vision witness serves for every gated task. It names no check, so
// the map shows no pin for it.
const NO_WITNESS = "this server runs no vision witness";

// Whether the record says this server runs no vision witness at all. The server has no field for
// it, so its failed_reason wording is the only sign; this is the one place that reads it.
export function witnessOff(record) {
  return record?.status === "failed" && record.failed_reason === NO_WITNESS && checksOf(record).length === 0;
}

// What each verdict reads as, and its tone. `doubt` asks for a look; `none` makes no claim.
const VERDICTS = {
  consistent: { word: "consistent", tone: "consistent", line: "The model saw every must-keep item kept." },
  inconsistent: { word: "inconsistent", tone: "doubt", line: "The model saw a must-keep item change." },
  unclear: { word: "unclear", tone: "doubt", line: "The model could not tell from these pages." },
  not_checked: { word: "not checked", tone: "none", line: "No model judged this page." },
  pending: { word: "pending", tone: "none", line: "The vision check is still running." },
  failed: { word: "failed", tone: "none", line: "The vision check could not run." },
};

// The words and tone of one verdict. A verdict this page does not know makes no claim.
export function verdictInfo(verdict) {
  const known = VERDICTS[verdict];
  if (known) return { verdict, ...known };
  return { verdict: "unknown", word: "unknown", tone: "none", line: "This page does not know that verdict." };
}

function checksOf(record) {
  return Array.isArray(record?.checks) ? record.checks : [];
}

// Whether the map shows a pin for this record: a record with checks, a pending one, or one that
// failed for a reason other than the server having no vision witness.
export function hasPin(record) {
  if (!record || typeof record !== "object") return false;
  if (record.status === "pending") return true;
  if (record.status === "failed") return !witnessOff(record);
  return record.status === "done";
}

// How many checks reached each verdict.
export function verdictCounts(record) {
  const counts = { consistent: 0, inconsistent: 0, unclear: 0, not_checked: 0 };
  for (const check of checksOf(record)) {
    if (check.verdict in counts) counts[check.verdict] += 1;
    else counts.not_checked += 1;
  }
  return counts;
}

function pages(count) {
  return `${count} ${count === 1 ? "page" : "pages"}`;
}

// The verdict a pin shows for a whole record. A doubt wins over a clean page, and any page that no
// model judged keeps the record from reading as consistent.
export function summarize(record) {
  if (record?.status === "pending") return { ...verdictInfo("pending"), counts: "" };
  if (record?.status === "failed") {
    const reason = typeof record.failed_reason === "string" && record.failed_reason !== "" ? record.failed_reason : "no reason was given";
    return { ...verdictInfo("failed"), line: `The vision check could not run: ${reason}.`, counts: "" };
  }
  const total = checksOf(record).length;
  const counts = verdictCounts(record);
  let verdict;
  if (total === 0) verdict = "not_checked";
  else if (counts.inconsistent > 0) verdict = "inconsistent";
  else if (counts.unclear > 0) verdict = "unclear";
  else if (counts.not_checked > 0) verdict = "not_checked";
  else verdict = "consistent";
  return { ...verdictInfo(verdict), counts: countsLine(counts, total) };
}

// One line of counts, each against the total, with no count of zero.
export function countsLine(counts, total) {
  if (total === 0) return "no pages were checked";
  const parts = [];
  for (const key of ["consistent", "inconsistent", "unclear", "not_checked"]) {
    if (counts[key] > 0) parts.push(`${counts[key]} ${verdictInfo(key).word}`);
  }
  return `${pages(total)}: ${parts.join(" · ")}`;
}

// What one check says in the viewer: its verdict, and the model's reason or why no model judged it.
export function checkView(check) {
  const info = verdictInfo(check?.verdict);
  const reason = check?.verdict === "not_checked"
    ? (check?.not_checked_reason || "no reason was given")
    : (check?.reason || "");
  return {
    ...info,
    route: typeof check?.route === "string" ? check.route : "",
    reason,
    compared: check?.comparison === "candidate_only" ? "after only" : "before and after",
    mustKeep: Array.isArray(check?.must_keep) ? check.must_keep.map((item) => item.text) : [],
  };
}

// Muse Wonder's second look at a doubtful check, or null when the check has none. It sits beside
// the first verdict and never replaces it.
export function secondLook(check) {
  const second = check?.second_look;
  if (!second || typeof second !== "object") return null;
  const info = verdictInfo(second.verdict);
  let line;
  if (second.verdict === "not_checked") line = `Could not take a second look: ${second.not_checked_reason || "no reason was given"}.`;
  else if (second.agrees === true) line = `Agrees with the first look: ${info.word}.`;
  else if (second.agrees === false) line = `Disagrees with the first look: it sees ${info.word}.`;
  else line = `Sees ${info.word}.`;
  return { ...info, line, reason: second.reason || "", model: typeof second.model === "string" ? second.model : "" };
}

// The routes of a check's two screenshots, or null for a side the check has no image of.
export function imagePaths(task, check) {
  const base = `/api/tasks/${encodeURIComponent(task)}/vision/${check.index}`;
  return {
    before: check.baseline_image_sha256 ? `${base}/baseline.png` : null,
    after: check.candidate_image_sha256 ? `${base}.png` : null,
  };
}

// An id as the routes spell it. The view carries task and order ids as numbers, and the receipt
// counts carry orders as `o-N`.
export function spelled(prefix, id) {
  const text = String(id);
  return text.startsWith(`${prefix}-`) ? text : `${prefix}-${text}`;
}

// The tasks whose vision record the page asks for: each task on a machine whose order has a
// recorded receipt, since the witness runs once a receipt is recorded. `key` moves when the order's
// receipt count does, so the page asks again then.
export function visionTasks(view) {
  if (!view) return [];
  const recorded = new Map((view.receipts ?? []).map(({ order, recorded: count }) => [spelled("o", order), count]));
  return (view.tasks ?? [])
    .filter((task) => typeof task.machine === "string" && recorded.has(spelled("o", task.order)))
    .map((task) => {
      const order = spelled("o", task.order);
      return { task: spelled("t", task.id), machine: task.machine, order, key: recorded.get(order) };
    });
}

// The pin's accessible name: the task, the machine, the verdict and that it is advisory.
export function pinLabel(task, machine, summary) {
  return `Vision check of ${task} on ${machine}: ${summary.word}, advisory. Open the before and after pages.`;
}
