// The open questions an order asks, and the answers the question card sends: the command, the
// line an answer leaves, and the sentence a refusal reads as. This module imports only words.js
// and touches no DOM, so Node can test it.
import { capWords } from "./words.js";

// The question with this id among the view's open ones, or undefined.
export function openQuestion(view, id) {
  return view.questions.find((question) => question.id === id);
}

// The open question a question flag names. Flags carry the question's words, in view order.
export function questionOfFlag(view, flag) {
  return view.questions.find((question) => question.at !== null && question.text === flag.text);
}

// The ids of open questions the page has not shown before, in view order. `seen` is the set of
// ids already shown; it is not changed.
export function freshQuestions(view, seen) {
  return view.questions.filter((question) => !seen.has(question.id)).map((question) => question.id);
}

export function questionTitle(question) {
  return `q-${question.id} · o-${question.order} asks`;
}

// The question card's text: the question's words, cut to CARD_WORDS words.
export function questionCardText(question) {
  return capWords(question.text);
}

// What an answer does, in one line, before it is sent.
export function answerEffect(question) {
  return `Your answer becomes a ruling on o-${question.order}, and its agents start again from the new revision.`;
}

// The AnswerQuestion body for key 1 or 2, or null for any other key. Key 1 is option 0.
export function answerCommand(question, key) {
  if (key !== 1 && key !== 2) return null;
  return { cmd: "answer_question", question: question.id, option: key - 1 };
}

export function answeredLine(question, option) {
  return `answered · ${question.options[option]}`;
}

// The plain sentence a refused answer reads as. A 409 carries the origin's Rejection. An answer
// to a question already answered is a WrongState. Each line says nothing changed, which holds
// because a refused command appends nothing.
export function answerRefusal(status, text) {
  const body = parseBody(text);
  if (status === 401) return "The owner token was not accepted, so nothing changed. Sign in again.";
  if (status === 403) return "Only a member can answer a question, and this token is not one. Nothing changed.";
  if (status === 400 || status === 415 || status === 422) {
    return "The server could not read this answer, so nothing changed.";
  }
  if (status >= 500) return "The server failed to take the answer. Try again.";
  switch (body?.reason) {
    case "wrong_state":
      if (body.what === "question" && body.found === "answered") {
        return "This question was already answered, so nothing changed.";
      }
      return `The ${body.what ?? "question"} is ${body.found ?? "in another state"}, so nothing changed.`;
    case "unknown":
      return `The server does not know this ${body.what ?? "question"}, so nothing changed.`;
    case "forbidden":
      return "You are not a member of this world, so nothing changed.";
    case "invalid":
      return `The server did not take it: ${sentence(body.why)} Nothing changed.`;
    default:
      return "The server refused the answer. Nothing changed.";
  }
}

function parseBody(text) {
  try {
    const body = JSON.parse(text);
    return body !== null && typeof body === "object" ? body : null;
  } catch {
    return null;
  }
}

function sentence(text) {
  const plain = typeof text === "string" && text.length > 0 ? text : "the answer is not valid";
  return /[.!?]$/.test(plain) ? plain : `${plain}.`;
}
