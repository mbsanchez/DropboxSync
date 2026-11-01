/**
 * Runs every `scripts/*.test.mjs` with `node --test` (DBSYNC-97).
 *
 * The files are listed here and passed as explicit paths because a glob in the npm script is not portable:
 * `cmd.exe` does not expand it, and Node only expands globs itself from version 21 while CI pins Node 20.
 * The exit code of `node --test` is propagated; no test files at all is a failure, never a silent pass.
 */
import { spawnSync } from "node:child_process";
import { readdirSync, realpathSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const dir = dirname(fileURLToPath(import.meta.url));

/** Absolute paths of the test files in `scriptsDir`, sorted so the order is stable. */
export function listTests(scriptsDir = dir) {
  return readdirSync(scriptsDir)
    .filter((name) => name.endsWith(".test.mjs"))
    .sort()
    .map((name) => join(scriptsDir, name));
}

export function runTests(files = listTests(), nodeArgs = []) {
  if (files.length === 0) {
    console.error("run-tests: no *.test.mjs files found");
    return 1;
  }
  const r = spawnSync(process.execPath, ["--test", ...nodeArgs, ...files], { stdio: "inherit" });
  if (r.error) {
    console.error(`run-tests: ${r.error.message}`);
    return 1;
  }
  return r.status ?? 1;
}

// Real paths on both sides: through a symlinked path a plain comparison is false and nothing would run.
if (process.argv[1] && realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url))) {
  process.exit(runTests());
}
