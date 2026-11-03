// Rules for the page's chrome (the shell, the key bar, the rail, the badges and the replay bar)
// that have no DOM, so Node can test them.

// The link words that mean all is well: the shell hides them at rest and shows them with the key
// bar. Anything else (sign in, failed, a held map) stays in view.
export function linkQuiet(text) {
  return /^(live\b|replay$|starting$)/.test(text ?? "");
}
