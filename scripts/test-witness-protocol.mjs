import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";

for (const [runtime, file] of [["node", "driver.mjs"], ["bun", "driver.ts"]]) {
  const source = fileURLToPath(new URL(`../crates/acan-witness/src/harness/${file}`, import.meta.url));
  const child = spawn(runtime, [source], { stdio: ["pipe", "pipe", "inherit"] });
  const lines = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
  try {
    for (const nonce of [randomUUID(), randomUUID(), undefined]) {
      child.stdin.write(`${JSON.stringify({ op: "unknown", nonce })}\n`);
      const line = await Promise.race([
        lines.next(),
        new Promise((_, reject) => {
          const timer = setTimeout(() => reject(new Error("driver reply timed out")), 5000);
          timer.unref();
        }),
      ]);
      assert.equal(line.done, false);
      const reply = JSON.parse(line.value);
      assert.equal(reply.ok, false);
      assert.equal(reply.nonce, nonce ?? null);
      assert.equal(reply.detail, nonce ? "unknown operation" : "missing or invalid operation nonce");
    }
    console.log(`${runtime}: nonce echo and missing-nonce rejection passed`);
  } finally {
    child.kill();
  }
}
