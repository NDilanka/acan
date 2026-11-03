// Node tests for the card word cap: a word is a run of non-space characters or a backtick code
// span, text past the cap is cut with an ellipsis, and firstWords keeps its old behaviour. Run with:
//   node --test crates/acan-render-web/tests/js/words.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import { CARD_WORDS, capWords, firstWords, wordSpans } from "../../www/words.js";

const words = (count) => Array.from({ length: count }, (_, i) => `w${i}`);

test("one_hundred_words_pass_untouched", () => {
  assert.equal(CARD_WORDS, 100);
  const text = words(100).join(" ");
  assert.equal(capWords(text), text);
});

test("one_hundred_and_one_words_are_cut_with_an_ellipsis", () => {
  const text = words(101).join(" ");
  const expected = `${text.split(" ").slice(0, 100).join(" ")}…`;
  assert.equal(capWords(text), expected);
  assert.equal(capWords("one  two   three", 2), "one  two…", "the kept part keeps its spacing");
});

test("code_spans_count_as_one_word", () => {
  assert.deepEqual(wordSpans("`a b c` d"), [{ start: 0, end: 7 }, { start: 8, end: 9 }]);
  const head = words(98).join(" ");
  // The span is word 99 and t1 is word 100, so the cut lands after t1 and the span stays whole.
  assert.equal(capWords(`${head} \`x y z\` t1 t2`), `${head} \`x y z\` t1…`);
});

test("first_words_keeps_its_old_behaviour", () => {
  const long = "  Count failed logins per client address or per account?  ";
  assert.equal(firstWords(long), "Count failed logins per client address…");
  assert.equal(firstWords("keep the old header?"), "keep the old header?", "six words or fewer: whole, no ellipsis");
  assert.equal(firstWords("one two three four five six"), "one two three four five six");
  assert.equal(firstWords(""), "");
  assert.equal(firstWords("   "), "");
  assert.equal(firstWords(undefined), "");
});
