import assert from "node:assert/strict";
import { copyFileSync, mkdirSync, mkdtempSync, symlinkSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import { checkAppGroups, readGroups } from "./check-app-groups.mjs";
import { GROUP_SUFFIX } from "./render-entitlements.mjs";

const script = join(dirname(fileURLToPath(import.meta.url)), "check-app-groups.mjs");
// Made-up teams: tests must never carry a real one (the repository is public).
const TEAM = "ABCDE12345";
const OTHER = "ZZZZZ99999";

const plist = (...ids) => `<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
	<key>com.apple.security.app-sandbox</key><true/>${
    ids.length
      ? `
	<key>com.apple.security.application-groups</key>
	<array>${ids.map((i) => `<string>${i}</string>`).join("")}</array>`
      : ""
  }
</dict></plist>`;

function run(team, host, ...appexes) {
  const dir = mkdtempSync(join(tmpdir(), "appgroups-"));
  const files = [host, ...appexes].map((xml, i) => {
    const f = join(dir, `${i}.plist`);
    writeFileSync(f, xml);
    return f;
  });
  return spawnSync("node", [script, team, ...files], { encoding: "utf8" });
}

const good = plist(`${TEAM}.com.dropboxsyncdesktop`);

test("a matching host and appex pass and say so", () => {
  const r = run(TEAM, good, good);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /^app-groups: OK$/m);
});

test("a failing check prints no OK token", () => {
  const r = run(TEAM, good, plist());
  assert.equal(r.status, 1);
  assert.doesNotMatch(r.stdout, /app-groups: OK/);
});

/**
 * Runs the CLI through a symlinked directory (`/tmp` and `/var` are symlinks on macOS). The script is
 * copied next to its sibling module so that the only thing that differs from a plain run is the path.
 */
function runViaSymlink(team, host, ...appexes) {
  const dir = mkdtempSync(join(tmpdir(), "appgroups-link-"));
  const real = join(dir, "real");
  mkdirSync(real);
  copyFileSync(script, join(real, "check-app-groups.mjs"));
  copyFileSync(join(dirname(script), "render-entitlements.mjs"), join(real, "render-entitlements.mjs"));
  copyFileSync(join(dirname(script), "entry-point.mjs"), join(real, "entry-point.mjs"));
  const link = join(dir, "link");
  symlinkSync(real, link);
  const files = [host, ...appexes].map((xml, i) => {
    const f = join(dir, `${i}.plist`);
    writeFileSync(f, xml);
    return f;
  });
  return spawnSync("node", [join(link, "check-app-groups.mjs"), team, ...files], { encoding: "utf8" });
}

test("through a symlinked path a good check still runs and prints OK", () => {
  const r = runViaSymlink(TEAM, good, good);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /^app-groups: OK$/m);
});

test("through a symlinked path a bad check still fails: the CLI does not silently skip", () => {
  const r = runViaSymlink(TEAM, good, plist());
  assert.equal(r.status, 1, "a skipped main() would exit 0");
  assert.match(r.stderr, /has no com\.apple\.security\.application-groups/);
});

test("several appexes all matching pass", () => {
  assert.equal(run(TEAM, good, good, good).status, 0);
});

test("a missing group on the appex fails", () => {
  const r = run(TEAM, good, plist());
  assert.equal(r.status, 1);
  assert.match(r.stderr, /has no com\.apple\.security\.application-groups/);
});

test("a missing group on the host fails", () => {
  assert.equal(run(TEAM, plist(), good).status, 1);
});

test("an empty array fails", () => {
  const empty = `<key>com.apple.security.application-groups</key><array></array>`;
  assert.equal(run(TEAM, empty, empty).status, 1);
});

test("an extra group on the appex fails", () => {
  const r = run(TEAM, good, plist(`${TEAM}.com.dropboxsyncdesktop`, `${TEAM}.extra`));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /is not "ABCDE12345\.com\.dropboxsyncdesktop"/);
});

test("a wrong team prefix fails even when both sides agree", () => {
  const wrong = plist(`${OTHER}.com.dropboxsyncdesktop`);
  const r = run(TEAM, wrong, wrong);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /is not/);
});

test("a prefix that merely begins with the team text fails", () => {
  const sneaky = plist(`${TEAM}X.com.dropboxsyncdesktop`);
  assert.equal(run(TEAM, sneaky, sneaky).status, 1);
});

test("a correctly prefixed id with any other suffix fails even when both sides agree", () => {
  for (const id of [`${TEAM}.anything`, `${TEAM}.com.dropboxsyncdesktop.extra`, `${TEAM}.`, `${TEAM}.com.otherapp`]) {
    const bad = plist(id);
    const r = run(TEAM, bad, bad);
    assert.equal(r.status, 1, id);
    assert.match(r.stderr, /is not "ABCDE12345\.com\.dropboxsyncdesktop"/);
  }
});

test("a padded id fails: ids are compared verbatim, never trimmed", () => {
  const padded = plist(` ${TEAM}.com.dropboxsyncdesktop `);
  assert.equal(run(TEAM, padded, padded).status, 1);
  assert.deepEqual(readGroups(padded), [` ${TEAM}.com.dropboxsyncdesktop `]);
});

test("a self-closing <string/> entry is an invalid entry and fails", () => {
  const blank = `<key>com.apple.security.application-groups</key><array><string/></array>`;
  assert.deepEqual(readGroups(blank), [""]);
  const r = run(TEAM, blank, blank);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /group "" is not/);
  const mixed = `<key>com.apple.security.application-groups</key><array><string>${TEAM}.com.dropboxsyncdesktop</string><string /></array>`;
  assert.equal(run(TEAM, mixed, mixed).status, 1);
});

test("an empty or malformed team fails rather than matching everything", () => {
  assert.equal(run("", good, good).status, 2);
  assert.equal(run("abc", good, good).status, 1);
});

test("checkAppGroups rejects an empty or non-team value with the specific message", () => {
  const bundles = [{ name: "host", xml: good }, { name: "appex", xml: good }];
  assert.deepEqual(checkAppGroups("", bundles), [`team "" is not a 10-character team id`]);
  assert.deepEqual(checkAppGroups("not set", bundles), [`team "not set" is not a 10-character team id`]);
  assert.deepEqual(checkAppGroups(undefined, bundles), [`team "" is not a 10-character team id`]);
  assert.deepEqual(checkAppGroups("abcde12345", bundles), [`team "abcde12345" is not a 10-character team id`]);
});

test("the expected suffix is the renderer's, not a second copy", () => {
  assert.deepEqual(checkAppGroups(TEAM, [{ name: "a", xml: good }, { name: "b", xml: good }]), []);
  assert.equal(GROUP_SUFFIX, "com.dropboxsyncdesktop");
});

test("readGroups returns every id in order", () => {
  assert.deepEqual(readGroups(plist("A.x", "B.y")), ["A.x", "B.y"]);
  assert.deepEqual(readGroups(plist()), []);
});

test("checkAppGroups needs a host and an appex", () => {
  assert.notEqual(checkAppGroups(TEAM, [{ name: "host", xml: good }]).length, 0);
});
