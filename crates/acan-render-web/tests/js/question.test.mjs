// Node tests for the question card's rules: which question a flag names, which questions are new,
// the answer command for keys 1 and 2, and the sentences a refusal reads as. Run with:
//   node --test crates/acan-render-web/tests/js/question.test.mjs
import assert from "node:assert/strict";
import { test } from "node:test";

import {
  answerCommand,
  answerEffect,
  answerRefusal,
  answeredLine,
  freshQuestions,
  openQuestion,
  questionOfFlag,
  questionTitle,
} from "../../www/question.js";

const Q1 = { id: 1, order: 3, text: "Keep the old cookie name?", options: ["keep it", "rename it"], at: [4, 5] };
const Q2 = { id: 2, order: 4, text: "Drop the v1 route?", options: ["drop it", "keep it"], at: null };
const VIEW = { questions: [Q1, Q2] };

test("key 1 answers option 0 and key 2 answers option 1, by the question's own id", () => {
  assert.deepEqual(answerCommand(Q1, 1), { cmd: "answer_question", question: 1, option: 0 });
  assert.deepEqual(answerCommand(Q1, 2), { cmd: "answer_question", question: 1, option: 1 });
});

test("any other key sends nothing: a question has two answers", () => {
  for (const key of [0, 3, 9, "1"]) assert.equal(answerCommand(Q1, key), null);
});

test("the card names the question, the order that asks it, and what an answer does", () => {
  assert.equal(questionTitle(Q1), "q-1 · o-3 asks");
  assert.match(answerEffect(Q1), /ruling on o-3/);
  assert.equal(answeredLine(Q1, 1), "answered · rename it");
});

test("a flag names the open question with its words and a pin", () => {
  assert.equal(questionOfFlag(VIEW, { kind: "question", text: "Keep the old cookie name?" }), Q1);
  assert.equal(questionOfFlag(VIEW, { kind: "question", text: "Drop the v1 route?" }), undefined, "no pin, no flag");
  assert.equal(openQuestion(VIEW, 2), Q2);
  assert.equal(openQuestion(VIEW, 9), undefined);
});

test("only questions the page has not shown are fresh, and the seen set is not changed", () => {
  const seen = new Set([1]);
  assert.deepEqual(freshQuestions(VIEW, seen), [2]);
  assert.deepEqual([...seen], [1]);
  assert.deepEqual(freshQuestions({ questions: [] }, seen), []);
});

test("an answer to an answered question is a WrongState, said in one plain sentence", () => {
  const line = answerRefusal(409, '{"reason":"wrong_state","what":"question","found":"answered"}');
  assert.equal(line, "This question was already answered, so nothing changed.");
});

test("every other refusal is one plain sentence, never the JSON", () => {
  const cases = [
    [409, '{"reason":"wrong_state","what":"order","found":"cancelled"}', "The order is cancelled, so nothing changed."],
    [409, '{"reason":"unknown","what":"question","id":"q-9"}', "The server does not know this question, so nothing changed."],
    [409, '{"reason":"invalid","why":"a question has options 0 and 1"}', "The server did not take it: a question has options 0 and 1. Nothing changed."],
    [409, '{"reason":"forbidden","why":"not a member of this world"}', "You are not a member of this world, so nothing changed."],
    [403, '{"error":"forbidden"}', "Only a member can answer a question, and this token is not one. Nothing changed."],
    [400, "bad json", "The server could not read this answer, so nothing changed."],
    [503, "", "The server failed to take the answer. Try again."],
    [409, "", "The server refused the answer. Nothing changed."],
  ];
  for (const [status, body, expected] of cases) assert.equal(answerRefusal(status, body), expected);
});
