// Every named import between the page's modules names a real export, so a merge that renames or
// drops an export fails here rather than as a blank page. The wasm package is built, not
// checked in, so imports from pkg/ are skipped. Run with:
//   node --test crates/acan-render-web/tests/js/imports.test.mjs
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";

const WWW = new URL("../../www/", import.meta.url);

function exportsOf(source) {
  const names = new Set();
  for (const m of source.matchAll(/^export\s+(?:async\s+)?(?:function\*?|const|let|var|class)\s+([A-Za-z_$][\w$]*)/gm)) {
    names.add(m[1]);
  }
  for (const m of source.matchAll(/^export\s*\{([^}]*)\}/gm)) {
    for (const part of m[1].split(",")) {
      const name = part.trim().split(/\s+as\s+/).pop();
      if (name) names.add(name);
    }
  }
  return names;
}

function importsOf(source) {
  const found = [];
  for (const m of source.matchAll(/^import\s+(?:[\w$]+\s*,\s*)?\{([^}]*)\}\s*from\s*"(\.\/[^"]+)"/gm)) {
    const names = m[1].split(",").map((part) => part.trim().split(/\s+as\s+/)[0]).filter(Boolean);
    found.push({ from: m[2], names });
  }
  return found;
}

test("each named import between www modules resolves to an export of its module", () => {
  const files = readdirSync(WWW).filter((name) => name.endsWith(".js"));
  const exported = new Map(files.map((name) => [name, exportsOf(readFileSync(new URL(name, WWW), "utf8"))]));
  const missing = [];
  let checked = 0;
  for (const file of files) {
    for (const { from, names } of importsOf(readFileSync(new URL(file, WWW), "utf8"))) {
      if (from.startsWith("./pkg/")) continue;
      const target = from.slice(2);
      const have = exported.get(target);
      if (have === undefined) {
        missing.push(`${file}: no module ${from}`);
        continue;
      }
      for (const name of names) {
        checked += 1;
        if (!have.has(name)) missing.push(`${file}: ${from} has no export ${name}`);
      }
    }
  }
  assert.deepEqual(missing, []);
  assert.ok(checked > 100, `only ${checked} imports were found, so the parser missed some`);
});
