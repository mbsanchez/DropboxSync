import assert from "node:assert/strict";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { spawnSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

import {
  TEAM_ID_PATTERN,
  appGroupId,
  decideAppGroup,
  isGeneratedPath,
  isRealSigningIdentity,
  renderEntitlements,
} from "./render-entitlements.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const script = join(here, "render-entitlements.mjs");
// A made-up team: tests must never carry a real one (the repository is public).
const TEAM = "ABCDE12345";

const TEMPLATE = `<dict>
	<key>com.apple.security.app-sandbox</key>
	<true/>
	<key>com.apple.security.application-groups</key>
	<array>
		<string>__APP_GROUP_ID__</string>
	</array>
</dict>
`;

test("the group id is the team prefix plus the fixed suffix", () => {
  assert.equal(appGroupId(TEAM), `${TEAM}.com.dropboxsyncdesktop`);
});

test("no team yields no group id, never a bare suffix", () => {
  for (const empty of [undefined, null, "", "   "]) {
    assert.equal(appGroupId(empty), null);
  }
});

test("a malformed team is refused rather than turned into a wrong group", () => {
  for (const bad of ["abc", "ABCDE1234", "ABCDE123456", "ABCDE1234!", "abcde12345"]) {
    assert.throws(() => appGroupId(bad), /not a valid Apple team id/);
  }
});

test("the token is replaced by the group id", () => {
  const out = renderEntitlements(TEMPLATE, `${TEAM}.com.dropboxsyncdesktop`);
  assert.match(out, new RegExp(`<string>${TEAM}\\.com\\.dropboxsyncdesktop</string>`));
  assert.doesNotMatch(out, /__APP_GROUP_ID__/);
});

test("with no group id the application-groups key is omitted and the rest survives", () => {
  const out = renderEntitlements(TEMPLATE, null);
  assert.doesNotMatch(out, /application-groups/);
  assert.doesNotMatch(out, /__APP_GROUP_ID__/);
  assert.doesNotMatch(out, /<array>/);
  assert.match(out, /com\.apple\.security\.app-sandbox/);
});

test("a template that still holds the token after rendering is an error", () => {
  assert.throws(
    () => renderEntitlements("<dict><string>__APP_GROUP_ID__</string></dict>", null),
    /unreplaced/,
  );
});

const ROOT = "/r";
const GEN = `${ROOT}/apps/desktop/src-tauri/.generated`;
const APPEX_BUILD = `${ROOT}/apps/desktop/native/macos/FinderSyncExtension/build`;
const FILE_PROVIDER_BUILD = `${ROOT}/apps/desktop/native/macos/FileProviderExtension/build`;

test("output is only allowed in the gitignored output directories", () => {
  assert.equal(isGeneratedPath(`${GEN}/host.entitlements`, ROOT), true);
  assert.equal(isGeneratedPath(`${APPEX_BUILD}/x.entitlements`, ROOT), true);
  assert.equal(isGeneratedPath(`${FILE_PROVIDER_BUILD}/x.entitlements`, ROOT), true);
  assert.equal(isGeneratedPath(`${ROOT}/apps/desktop/native/macos/FileProviderExtension/x.entitlements`, ROOT), false);
  assert.equal(isGeneratedPath(`${ROOT}/apps/desktop/native/macos/FileProviderExtension/build-evil/x`, ROOT), false);
  assert.equal(isGeneratedPath(`${ROOT}/apps/desktop/native/macos/FinderSyncExtension/x.entitlements`, ROOT), false);
  assert.equal(isGeneratedPath(`${ROOT}/apps/desktop/src-tauri/host.entitlements`, ROOT), false);
});

test("a directory merely named build or .generated elsewhere is refused", () => {
  // Tracked directories the old "any ancestor named build" rule accepted.
  assert.equal(isGeneratedPath(`${ROOT}/apps/desktop/scripts/build/foo`, ROOT), false);
  assert.equal(isGeneratedPath(`${ROOT}/apps/desktop/src-tauri/build/x`, ROOT), false);
  assert.equal(isGeneratedPath(`${ROOT}/apps/desktop/scripts/.generated/x`, ROOT), false);
  // A checkout living under a parent called `build`: the parent must not count.
  assert.equal(isGeneratedPath("/build/checkout/apps/desktop/scripts/x", "/build/checkout"), false);
  assert.equal(isGeneratedPath("/build/checkout/apps/desktop/src-tauri/.generated/x", "/build/checkout"), true);
});

test("a path that climbs out of an allowed directory is refused", () => {
  assert.equal(isGeneratedPath(`${GEN}/../host.entitlements`, ROOT), false);
  assert.equal(isGeneratedPath(`${GEN}-evil/x`, ROOT), false);
  assert.equal(isGeneratedPath(GEN, ROOT), false, "the directory itself is not a file target");
});

const IDENTITY = `Developer ID Application: Test Org (${TEAM})`;

test("only a named identity is a real signing identity", () => {
  for (const no of [undefined, null, "", "   ", "-", " - "]) {
    assert.equal(isRealSigningIdentity(no), false, JSON.stringify(no));
  }
  assert.equal(isRealSigningIdentity(IDENTITY), true);
});

test("the group is decided by team AND a real identity", () => {
  const id = `${TEAM}.com.dropboxsyncdesktop`;
  assert.deepEqual(decideAppGroup({ APPLE_TEAM_ID: TEAM, APPLE_SIGNING_IDENTITY: IDENTITY }), { groupId: id, reason: "ok" });
  for (const identity of [undefined, "", "-"]) {
    assert.deepEqual(
      decideAppGroup({ APPLE_TEAM_ID: TEAM, APPLE_SIGNING_IDENTITY: identity }),
      { groupId: null, reason: "ad-hoc" },
      `identity ${JSON.stringify(identity)}`,
    );
  }
  assert.deepEqual(decideAppGroup({ APPLE_SIGNING_IDENTITY: IDENTITY }), { groupId: null, reason: "no-team" });
  assert.throws(() => decideAppGroup({ APPLE_TEAM_ID: "bad", APPLE_SIGNING_IDENTITY: IDENTITY }), /not a valid/);
});

/**
 * Runs the real CLI from a throwaway copy of the repository layout, so the script resolves its
 * whitelist against the temp root and nothing is ever written into the working tree.
 *
 * `via` picks how the script and output are reached: `"plain"` uses the temp path as the OS hands it
 * out (on macOS `/var/...`, itself a symlink to `/private/var/...`), `"real"` realpaths it, and `"link"`
 * goes through an explicit extra symlink to the root. `setup(root)` may plant files before the run.
 */
function runCli(team, rel = "apps/desktop/src-tauri/.generated/out.entitlements", opts = {}) {
  const { identity = "identity" in opts ? undefined : IDENTITY, via = "plain", setup, flags = [] } = opts;
  const made = mkdtempSync(join(tmpdir(), "render-ent-"));
  const realRoot = realpathSync(made);
  const root = via === "real" ? realRoot : made;
  const scriptsDir = join(realRoot, "apps", "desktop", "scripts");
  mkdirSync(scriptsDir, { recursive: true });
  copyFileSync(script, join(scriptsDir, "render-entitlements.mjs"));
  copyFileSync(join(here, "entry-point.mjs"), join(scriptsDir, "entry-point.mjs"));
  writeFileSync(join(realRoot, "t.plist"), TEMPLATE);
  if (setup) setup(realRoot);
  let seen = root;
  if (via === "link") {
    seen = join(mkdtempSync(join(tmpdir(), "render-link-")), "link");
    symlinkSync(realRoot, seen);
  }
  const copy = join(seen, "apps", "desktop", "scripts", "render-entitlements.mjs");
  const target = join(seen, rel);
  const env = { ...process.env };
  delete env.APPLE_TEAM_ID;
  delete env.APPLE_SIGNING_IDENTITY;
  if (team !== undefined) env.APPLE_TEAM_ID = team;
  if (identity !== undefined) env.APPLE_SIGNING_IDENTITY = identity;
  const r = spawnSync("node", [copy, ...flags, join(seen, "t.plist"), target], { env, encoding: "utf8" });
  return { ...r, target, realRoot };
}

test("CLI with a team writes the group and prints the id", () => {
  const r = runCli(TEAM);
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.stdout.trim(), `${TEAM}.com.dropboxsyncdesktop`);
  assert.match(readFileSync(r.target, "utf8"), new RegExp(`${TEAM}\\.com\\.dropboxsyncdesktop`));
});

test("CLI writes the appex output directory too", () => {
  const r = runCli(TEAM, "apps/desktop/native/macos/FinderSyncExtension/build/x.entitlements");
  assert.equal(r.status, 0, r.stderr);
});

test("CLI writes the File Provider appex output directory too", () => {
  const r = runCli(TEAM, "apps/desktop/native/macos/FileProviderExtension/build/x.entitlements");
  assert.equal(r.status, 0, r.stderr);
});

test("CLI without a team writes no group, prints nothing and warns loudly", () => {
  const r = runCli(undefined);
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.stdout.trim(), "");
  assert.match(r.stderr, /WARNING/);
  assert.match(r.stderr, /APPLE_TEAM_ID/);
  assert.doesNotMatch(readFileSync(r.target, "utf8"), /application-groups/);
});

test("CLI refuses a tracked directory that is merely named build", () => {
  for (const rel of ["apps/desktop/scripts/build/out.entitlements", "apps/desktop/src-tauri/build/out.entitlements"]) {
    const r = runCli(TEAM, rel);
    assert.notEqual(r.status, 0, rel);
    assert.match(r.stderr, /gitignored/);
  }
});

test("CLI refuses to write outside a gitignored directory", () => {
  const r = runCli(TEAM, "apps/desktop/src-tauri/host.entitlements");
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /gitignored/);
});

test("CLI works whichever way the path is spelled: plain, realpath, explicit symlink", () => {
  for (const via of ["plain", "real", "link"]) {
    const r = runCli(TEAM, undefined, { via });
    assert.equal(r.status, 0, `${via}: ${r.stderr}`);
    assert.equal(r.stdout.trim(), `${TEAM}.com.dropboxsyncdesktop`, `${via} must really render, not silently skip`);
    assert.ok(existsSync(r.target), `${via} wrote the file`);
  }
});

test("CLI through a symlink still refuses an output outside the gitignored directories", () => {
  const r = runCli(TEAM, "apps/desktop/src-tauri/host.entitlements", { via: "link" });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /gitignored/);
});

test("CLI with a team but an ad-hoc identity renders no group and warns loudly", () => {
  for (const identity of [undefined, "", "-"]) {
    const r = runCli(TEAM, undefined, { identity });
    assert.equal(r.status, 0, r.stderr);
    assert.equal(r.stdout.trim(), "", `identity ${JSON.stringify(identity)}`);
    assert.match(r.stderr, /WARNING.*APPLE_SIGNING_IDENTITY/);
    assert.doesNotMatch(readFileSync(r.target, "utf8"), /application-groups/);
  }
});

test("CLI does not write through a symlink planted at the output path", () => {
  const victim = "apps/desktop/tracked.txt";
  const r = runCli(TEAM, undefined, {
    setup(root) {
      const gen = join(root, "apps", "desktop", "src-tauri", ".generated");
      mkdirSync(gen, { recursive: true });
      writeFileSync(join(root, victim), "TRACKED\n");
      symlinkSync(join(root, victim), join(gen, "out.entitlements"));
    },
  });
  assert.equal(readFileSync(join(r.realRoot, victim), "utf8"), "TRACKED\n", "the tracked file was overwritten");
  assert.equal(r.status, 0, r.stderr);
  assert.match(readFileSync(r.target, "utf8"), /application-groups/, "the output is a regular file with the content");
});

test("CLI refuses an output directory that is a symlink out of the whitelist", () => {
  const r = runCli(TEAM, undefined, {
    setup(root) {
      const src = join(root, "apps", "desktop", "src-tauri");
      mkdirSync(join(root, "apps", "desktop", "tracked-dir"), { recursive: true });
      mkdirSync(src, { recursive: true });
      symlinkSync(join(root, "apps", "desktop", "tracked-dir"), join(src, ".generated"));
    },
  });
  assert.notEqual(r.status, 0);
  assert.match(r.stderr, /gitignored/);
  assert.equal(existsSync(join(r.realRoot, "apps", "desktop", "tracked-dir", "out.entitlements")), false);
});

test("isGeneratedPath resolves a symlinked output directory", () => {
  const root = realpathSync(mkdtempSync(join(tmpdir(), "gen-root-")));
  mkdirSync(join(root, "apps", "desktop", "src-tauri"), { recursive: true });
  mkdirSync(join(root, "elsewhere"));
  symlinkSync(join(root, "elsewhere"), join(root, "apps", "desktop", "src-tauri", ".generated"));
  assert.equal(isGeneratedPath(join(root, "apps", "desktop", "src-tauri", ".generated", "x"), root), false);
});

test("isGeneratedPath matches when the repository root is spelled through a symlink", () => {
  const real = realpathSync(mkdtempSync(join(tmpdir(), "gen-real-")));
  mkdirSync(join(real, "apps", "desktop", "src-tauri", ".generated"), { recursive: true });
  const link = join(mkdtempSync(join(tmpdir(), "gen-link-")), "checkout");
  symlinkSync(real, link);
  const file = join(link, "apps", "desktop", "src-tauri", ".generated", "host.entitlements");
  assert.equal(isGeneratedPath(file, link), true);
  assert.equal(isGeneratedPath(file, real), true);
  assert.equal(isGeneratedPath(join(link, "apps", "desktop", "elsewhere", "x"), link), false);
});

test("TEAM_ID_PATTERN accepts exactly ten upper-case letters or digits", () => {
  assert.equal(TEAM_ID_PATTERN.test(TEAM), true);
  for (const bad of ["", "abcde12345", "ABCDE1234", "ABCDE123456", " ABCDE12345", "ABCDE12345\n", "ABCDE-2345"]) {
    assert.equal(TEAM_ID_PATTERN.test(bad), false, JSON.stringify(bad));
  }
});

test("CLI --require-group renders and prints the id when a group is possible", () => {
  const r = runCli(TEAM, undefined, { flags: ["--require-group"] });
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.stdout.trim(), `${TEAM}.com.dropboxsyncdesktop`);
  assert.ok(existsSync(r.target));
});

test("CLI --require-group fails, and writes nothing, when there is no team", () => {
  const r = runCli(undefined, undefined, { flags: ["--require-group"] });
  assert.equal(r.status, 1, r.stderr);
  assert.equal(r.stdout.trim(), "");
  assert.match(r.stderr, /--require-group.*APPLE_TEAM_ID/);
  assert.equal(existsSync(r.target), false);
});

test("CLI --require-group fails, and writes nothing, for an unset, empty or ad-hoc identity", () => {
  for (const identity of [undefined, "", "-"]) {
    const r = runCli(TEAM, undefined, { identity, flags: ["--require-group"] });
    assert.equal(r.status, 1, `identity ${JSON.stringify(identity)}: ${r.stderr}`);
    assert.match(r.stderr, /--require-group.*APPLE_SIGNING_IDENTITY/);
    assert.equal(existsSync(r.target), false);
  }
});

test("CLI --require-group still rejects a malformed team, and a stray extra argument", () => {
  assert.equal(runCli("bad", undefined, { flags: ["--require-group"] }).status, 1);
  const r = runCli(TEAM, undefined, { flags: ["--bogus"] });
  assert.equal(r.status, 2, "an unknown flag is read as the template and the extra argument is refused");
  assert.match(r.stderr, /usage/);
});
