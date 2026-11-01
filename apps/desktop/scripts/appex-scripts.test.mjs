/**
 * Keeps the two appex build scripts aligned on the flags that decide whether Apple accepts the result
 * (DBSYNC-95). Tauri does not re-sign an appex copied through `bundle.macOS.files`, so each script's
 * signature is final and any drift reaches notarization unchanged. `FileProviderExtension/build-appex.sh`
 * was copied from `FinderSyncExtension/build-appex.sh`; this test fails if one is edited without the other.
 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { test } from "node:test";

const macos = join(dirname(fileURLToPath(import.meta.url)), "..", "native", "macos");
const SCRIPTS = {
  "Finder Sync": join(macos, "FinderSyncExtension", "build-appex.sh"),
  "File Provider": join(macos, "FileProviderExtension", "build-appex.sh"),
};

/** Each entry is a line the script must contain, as a regular expression (comments do not count). */
const REQUIRED = {
  "no injected get-task-allow": /^SIGN_ARGS=\(CODE_SIGN_INJECT_BASE_ENTITLEMENTS=NO\)$/m,
  "secure timestamp": /^\s*OTHER_CODE_SIGN_FLAGS="--timestamp"$/m,
  "manual signing": /^\s*CODE_SIGN_STYLE=Manual$/m,
  "named identity": /^\s*CODE_SIGN_IDENTITY="\$APPEX_IDENTITY"$/m,
  "group required for a real build": /RENDER_ARGS\+=\(--require-group\)/,
  "rendered entitlements are signed": /SIGN_ARGS\+=\(CODE_SIGN_ENTITLEMENTS="\$ENTITLEMENTS" DBSYNC_APP_GROUP_ID="\$APP_GROUP_ID"\)/,
  "strict shell": /^set -euo pipefail$/m,
  "principal class guard": /does not match the ObjC class the binary exports/,
};

const read = (name) => readFileSync(SCRIPTS[name], "utf8");

for (const name of Object.keys(SCRIPTS)) {
  for (const [what, pattern] of Object.entries(REQUIRED)) {
    test(`${name} build script keeps: ${what}`, () => {
      assert.match(read(name), pattern);
    });
  }
}

test("the two scripts call the same renderer with the same arguments shape", () => {
  const call = /scripts\/render-entitlements\.mjs" \\\n\s*"\$\{RENDER_ARGS\[@\]\+"\$\{RENDER_ARGS\[@\]\}"\}" "\$\{ROOT\}\/\w+\.entitlements" "\$RENDERED"\)"/;
  for (const name of Object.keys(SCRIPTS)) assert.match(read(name), call, name);
});

test("each script signs only its own entitlements template", () => {
  assert.match(read("Finder Sync"), /DropboxSyncFinderSync\.entitlements/);
  assert.doesNotMatch(read("Finder Sync"), /DropboxSyncFileProvider/);
  assert.match(read("File Provider"), /DropboxSyncFileProvider\.entitlements/);
  assert.doesNotMatch(read("File Provider"), /DropboxSyncFinderSync/);
});

test("the File Provider appex has no network entitlement and the group token", () => {
  const ent = readFileSync(join(macos, "FileProviderExtension", "DropboxSyncFileProvider.entitlements"), "utf8");
  assert.doesNotMatch(ent, /com\.apple\.security\.network/);
  assert.match(ent, /<string>__APP_GROUP_ID__<\/string>/);
  assert.match(ent, /com\.apple\.security\.app-sandbox/);
});

test("the File Provider Info.plist uses the File Provider extension point and omits DocumentGroup", () => {
  const plist = readFileSync(join(macos, "FileProviderExtension", "Info.plist"), "utf8");
  assert.match(plist, /<string>com\.apple\.fileprovider-nonui<\/string>/);
  assert.match(plist, /NSExtensionFileProviderSupportsEnumeration<\/key>\s*<true\/>/);
  assert.match(plist, /<key>DBSyncAppGroupID<\/key>/);
  assert.doesNotMatch(plist, /<key>NSExtensionFileProviderDocumentGroup<\/key>/);
  assert.doesNotMatch(plist, /<key>NSPrincipalClass<\/key>/);
});

test("the File Provider principal-class guard reads defined symbols only", () => {
  // `nm -a` also lists undefined imports, so an imported class such as NSObject would satisfy the guard.
  // `-g` is deliberately absent: the app's own ObjC classes are local symbols, so `nm -gU` would reject the
  // real principal class (checked against the built appex).
  const script = read("File Provider");
  assert.match(script, /EMITTED=\$\(nm -U "\$OUT\/Contents\/MacOS\/\$\{SCHEME\}"/);
  assert.doesNotMatch(script, /EMITTED=\$\(nm -\w*a/);
});

test("the File Provider target builds with the hardened runtime in every configuration", () => {
  const pbx = readFileSync(
    join(macos, "FileProviderExtension", "DropboxSyncFileProvider.xcodeproj", "project.pbxproj"),
    "utf8",
  );
  const configs = pbx.match(/ENABLE_HARDENED_RUNTIME = \w+;/g) ?? [];
  assert.ok(configs.length >= 2, "Debug and Release both set it");
  for (const c of configs) assert.equal(c, "ENABLE_HARDENED_RUNTIME = YES;");
});

test("the File Provider extension refuses containers it does not serve", () => {
  const swift = readFileSync(join(macos, "FileProviderExtension", "FileProviderExtension.swift"), "utf8");
  assert.match(swift, /case \.trashContainer:\s*(\/\/[^\n]*\s*)*throw CocoaError\(\.featureUnsupported\)/);
  assert.match(swift, /default:\s*throw NSFileProviderError\(\.noSuchItem\)/);
  assert.doesNotMatch(swift, /fetchContents[\s\S]*?completionHandler\(nil, nil, NSFileProviderError\(\.cannotSynchronize\)\)/);
});
