import assert from "node:assert/strict";
import { mkdtempSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { test } from "node:test";

import { isEntryPoint } from "./entry-point.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const script = join(here, "entry-point.mjs");

test("isEntryPoint is true for the module itself, also through a symlink, and false for another script", () => {
  const dir = mkdtempSync(join(tmpdir(), "entry-"));
  const link = join(dir, "link");
  symlinkSync(here, link);
  const url = pathToFileURL(script).href;
  assert.equal(isEntryPoint(url, script), true);
  assert.equal(isEntryPoint(url, join(link, "entry-point.mjs")), true);
  assert.equal(isEntryPoint(url, join(here, "check-app-groups.mjs")), false);
  assert.equal(isEntryPoint(url, undefined), false);
  assert.equal(isEntryPoint(url, join(dir, "missing.mjs")), false);
});
