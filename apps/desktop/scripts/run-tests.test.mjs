import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { listTests, runTests } from "./run-tests.mjs";

const here = dirname(fileURLToPath(import.meta.url));

function dirWith(files) {
  const d = mkdtempSync(join(tmpdir(), "run-tests-"));
  for (const [name, body] of Object.entries(files)) writeFileSync(join(d, name), body);
  return d;
}

test("only *.test.mjs files are listed, as absolute sorted paths", () => {
  const d = dirWith({ "b.test.mjs": "", "a.test.mjs": "", "helper.mjs": "", "c.test.js": "", "notes.md": "" });
  assert.deepEqual(listTests(d).map((p) => basename(p)), ["a.test.mjs", "b.test.mjs"]);
  assert.ok(listTests(d).every((p) => p.startsWith(d)));
});

test("this directory's own tests are found, including this one", () => {
  const names = listTests(here).map((p) => basename(p));
  assert.ok(names.includes("run-tests.test.mjs"));
  assert.ok(names.includes("render-entitlements.test.mjs"));
});

test("a passing file gives exit 0 and a failing one gives a non-zero exit", () => {
  const ok = dirWith({ "ok.test.mjs": `import {test} from "node:test"; test("ok", () => {});` });
  const bad = dirWith({ "bad.test.mjs": `import {test} from "node:test"; test("bad", () => { throw new Error("x"); });` });
  const run = (d) =>
    spawnSync(
      process.execPath,
      ["--input-type=module", "-e", `import {runTests, listTests} from ${JSON.stringify(join(here, "run-tests.mjs"))}; process.exit(runTests(listTests(${JSON.stringify(d)})));`],
      { encoding: "utf8" },
    );
  assert.equal(run(ok).status, 0);
  assert.notEqual(run(bad).status, 0, "a failing test file must fail the run");
});

test("no test files at all is a failure, not a silent pass", () => {
  const r = spawnSync(
    process.execPath,
    ["--input-type=module", "-e", `import {runTests} from ${JSON.stringify(join(here, "run-tests.mjs"))}; process.exit(runTests([]));`],
    // An empty directory as cwd: if the guard were gone, `node --test` with no paths would discover
    // tests from the cwd, and from the repository that would recurse into this very file.
    { encoding: "utf8", cwd: mkdtempSync(join(tmpdir(), "run-tests-empty-")) },
  );
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no \*\.test\.mjs/);
});
