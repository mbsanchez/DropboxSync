import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const src = join(dirname(fileURLToPath(import.meta.url)), "..", "src");

function sources(dir) {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sources(path);
    return /\.(ts|tsx)$/.test(name) ? [path] : [];
  });
}

// The macOS webview (wry 0.54) implements no JavaScript panels: `confirm` answers false without showing
// anything, and "Confirm & resume" was declined in silence. Ask through `confirmAction` instead.
test("the UI never asks through the webview's own dialogs", () => {
  const panel = /\b(?:window\.)?(?:confirm|alert|prompt)\s*\(/;
  const offenders = sources(src)
    .filter((path) => !path.endsWith("confirm.ts"))
    .filter((path) =>
      readFileSync(path, "utf8")
        .split("\n")
        .some((line) => !line.trim().startsWith("*") && !line.trim().startsWith("//") && panel.test(line)),
    )
    .map((path) => relative(src, path));
  assert.deepEqual(offenders, []);
});
