// Rules for the page's chrome (the shell, the key bar, the rail, the badges and the replay bar)
// that have no DOM, so Node can test them.

// The link words that mean all is well: the shell hides them at rest and shows them with the key
// bar. Anything else (sign in, failed, a held map) stays in view.
export function linkQuiet(text) {
  return /^(live\b|replay$|starting$)/.test(text ?? "");
}

// The Meta badge's words: the server's count of model calls since it started, or the log's own
// count where the server has none (a replay, a fixture). `serverCalls` is null with no server
// rows, `error` when the usage read failed.
export function metaBadgeText({ serverCalls = null, logCalls = 0, error = null } = {}) {
  if (error !== null) return "Meta · not read";
  const calls = serverCalls ?? logCalls;
  return `Meta · ${calls} ${calls === 1 ? "call" : "calls"}`;
}
