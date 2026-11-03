// Words in the text the cards show. A word is a run of non-space characters, or a backtick code
// span, which may hold spaces and counts as one word. This module has no DOM, so Node can test it.

// The most words a person-facing card shows before its text is cut.
export const CARD_WORDS = 100;

// Where each word of a text starts and ends, in order.
export function wordSpans(text) {
  return [...String(text ?? "").matchAll(/`[^`]*`|\S+/g)].map((match) => ({
    start: match.index,
    end: match.index + match[0].length,
  }));
}

// The text, cut after its `cap`th word with "…" after the cut. Text of `cap` words or fewer is
// returned unchanged.
export function capWords(text, cap = CARD_WORDS) {
  const words = wordSpans(text);
  if (words.length <= cap) return text;
  return `${String(text).slice(0, words[cap - 1].end)}…`;
}

// The first `count` words of a text, with "…" when it has more. Whitespace is trimmed.
export function firstWords(text, count = 6) {
  const words = String(text ?? "").trim().split(/\s+/).filter((word) => word !== "");
  return words.length <= count ? words.join(" ") : `${words.slice(0, count).join(" ")}…`;
}
