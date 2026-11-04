// Node tests for what a rewound map will not send. Run with:
//   node --test crates/acan-render-web/tests/js/guard.test.mjs
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { HISTORY_REASONS, historyBlock, setMappingBlock } from "../../www/guard.js";

const COMMANDS = ["send", "answer", "settle", "adopt", "seal", "strike", "cancel"];

test("a live map blocks nothing", () => {
  for (const command of COMMANDS) assert.equal(historyBlock(command, false), null);
});

test("a rewound map blocks each command with one sentence that says to go live", () => {
  assert.deepEqual(Object.keys(HISTORY_REASONS).sort(), [...COMMANDS].sort());
  for (const command of COMMANDS) {
    const reason = historyBlock(command, true);
    assert.match(reason, /^Go live to \w+\.$/, command);
    assert.equal(reason.split(". ").length, 1, `${command} gives one sentence`);
  }
  assert.equal(historyBlock("send", true), "Go live to send.");
});

test("a command with no rule is a programming error, not a silent pass", () => {
  assert.throws(() => historyBlock("launch", true), /no history rule/);
});

test("a map still mapping blocks every command, whatever the scrubber shows", () => {
  setMappingBlock("Mapping the codebase first.");
  try {
    for (const command of COMMANDS) {
      assert.equal(historyBlock(command, false), "Mapping the codebase first.", command);
      assert.equal(historyBlock(command, true), "Mapping the codebase first.", command);
    }
  } finally {
    setMappingBlock(null);
  }
  assert.equal(historyBlock("send", false), null, "writes open once the map has mapped");
});

// Each control that changes the origin's state asks the guard both where it paints (the control is
// off and says why) and where it sends (a stale click or key sends nothing).
test("each state-changing control asks the guard where it paints and where it sends", () => {
  const uses = { "compose.js": ["send"], "cards.js": ["settle", "answer"], "slip.js": ["adopt", "seal", "strike"], "history.js": ["cancel"] };
  for (const [file, commands] of Object.entries(uses)) {
    const source = readFileSync(new URL(`../../www/${file}`, import.meta.url), "utf8");
    for (const command of commands) {
      const count = source.split(`historyBlock("${command}"`).length - 1;
      assert.ok(count >= 2, `${file} asks about ${command} ${count} time(s), at paint and at send`);
    }
  }
});
