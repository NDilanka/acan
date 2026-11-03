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

// One short line for a card line the map's model writes, such as a settlement option or a
// declaration: the first clause, cut at a word near `max` characters. A leading id tag
// ("t-0 · ") is kept, since the separator there is part of the name. `title` is the full text
// for the hover, or null when the line is already short.
const CLAUSE = /\s+(?:—|–|--?)\s+|[:;,]\s+|\s+(?:because|since|so that)\s+/;

export function shortLine(text, max = 40) {
  const full = String(text ?? "");
  const tag = full.match(/^\S+ · /)?.[0] ?? "";
  const rest = full.slice(tag.length);
  let line = rest.split(CLAUSE)[0].trim();
  if (line === "") line = rest;
  if (line.length > max) {
    const cut = line.slice(0, max);
    const space = cut.lastIndexOf(" ");
    line = `${(space > max / 2 ? cut.slice(0, space) : cut).trimEnd()}…`;
  }
  const short = tag + line;
  return short === full ? { line: full, title: null } : { line: short, title: full };
}
