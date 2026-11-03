// The scope draft: the answer of GET /api/orders/{order}/scope-draft, and the ReviseScope that
// adopts it. A draft carries no authority until a human adopts it here and then seals it on the
// slip. This module has no DOM imports, so Node can test it.

export const DRAFT_VERSION = "acan-scope-draft/1";
export const SCOPE_VERSION = "acan-scope/1";

// How long a missing draft waits before the slip asks again. The foreman drafts in the background,
// so a 404 usually means "not yet".
export const DRAFT_RETRY_MS = 3000;

// Whether the slip asks for a draft of `order`: the order is live and has no scope lane yet.
export function wantsDraft(order, lanes) {
  if (order === undefined || order === null) return false;
  if (order.phase === "landed" || order.phase === "cancelled") return false;
  return !lanes.some((lane) => lane.order === order.id);
}

// The ReviseScope that adopts a drafted answer for `order`, the view's numeric order id. The scope
// is the draft's proposal at the draft's revision and base commit, as the server's own adoption
// does. It throws when the answer is not a draft of that order, so nothing malformed is sent.
export function adoptCommand(order, answer) {
  if (answer?.state !== "drafted") throw new Error("the answer holds no draft");
  const draft = answer.draft;
  if (draft?.version !== DRAFT_VERSION) throw new Error(`the draft is not ${DRAFT_VERSION}`);
  if (draft.order !== `o-${order}`) throw new Error(`the draft is for ${draft.order}, not o-${order}`);
  const body = draft.draft?.proposal;
  if (!body || !Array.isArray(body.may) || !Array.isArray(body.must_keep) || !Array.isArray(body.open)) {
    throw new Error("the draft has no scope body");
  }
  if (!Number.isInteger(draft.rev) || typeof draft.base_commit !== "string") {
    throw new Error("the draft names no revision or base commit");
  }
  return {
    cmd: "revise_scope",
    order,
    scope: { version: SCOPE_VERSION, order: draft.order, rev: draft.rev, base_commit: draft.base_commit, body },
  };
}

// What the slip shows of an answer: the counts and the model of a draft, or why it failed.
export function draftSummary(answer) {
  if (answer?.state === "failed") return { kind: "failed", reason: String(answer.reason ?? "") };
  const body = answer?.draft?.draft?.proposal ?? { may: [], must_keep: [], open: [] };
  return {
    kind: "drafted",
    may: body.may.length,
    keep: body.must_keep.length,
    open: body.open.length,
    model: answer?.draft?.draft?.model ?? "",
    items: body.may.map((item) => item.id),
  };
}
