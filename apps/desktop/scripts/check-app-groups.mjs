/**
 * Release gate for the App Group (DBSYNC-97): the host app and every nested appex must carry the
 * same `com.apple.security.application-groups`, and every id must be exactly `<team>.com.dropboxsyncdesktop`.
 *
 *   node check-app-groups.mjs <team> <host.plist> <appex.plist>...
 *
 * Each plist is the output of `codesign -d --entitlements - --xml <bundle>` (raw XML). Exit 0 when the
 * check passes, after printing `app-groups: OK` on stdout (a caller must require that token, not only the
 * exit code: a run that never reached the check also exits 0); exit 1 with one `::error::` line per
 * problem; exit 2 on bad usage.
 */
import { readFileSync } from "node:fs";

import { GROUP_SUFFIX, isEntryPoint } from "./render-entitlements.mjs";

const KEY = "com.apple.security.application-groups";

/**
 * The `application-groups` strings of an XML entitlements plist; `[]` when the key or array is absent.
 * Ids are returned verbatim: a padded id must fail the exact comparison, not be trimmed into passing.
 * A self-closing `<string/>` is an empty entry (`""`), which no valid group equals.
 */
export function readGroups(xml) {
  const m = xml.match(/<key>com\.apple\.security\.application-groups<\/key>\s*<array>([\s\S]*?)<\/array>/);
  if (!m) return [];
  return [...m[1].matchAll(/<string>([^<]*)<\/string>|<string\s*\/>/g)].map((s) => s[1] ?? "");
}

/** Problems found across `{ name, xml }` bundles (host first). Empty means the check passed. */
export function checkAppGroups(team, bundles) {
  const problems = [];
  if (!/^[A-Z0-9]{10}$/.test(team ?? "")) {
    return [`team "${team ?? ""}" is not a 10-character team id`];
  }
  const expected = `${team}.${GROUP_SUFFIX}`;
  if (bundles.length < 2) problems.push("expected the host and at least one appex");
  for (const { name, xml } of bundles) {
    const ids = readGroups(xml);
    if (ids.length === 0) problems.push(`${name} has no ${KEY}`);
    for (const id of ids) {
      if (id !== expected) problems.push(`${name} group "${id}" is not "${expected}"`);
    }
  }
  return problems;
}

if (isEntryPoint(import.meta.url)) {
  const [team, ...files] = process.argv.slice(2);
  if (!team || files.length < 2) {
    console.error("usage: check-app-groups.mjs <team> <host.plist> <appex.plist>...");
    process.exit(2);
  }
  const problems = checkAppGroups(team, files.map((f) => ({ name: f, xml: readFileSync(f, "utf8") })));
  for (const p of problems) console.error(`::error::${p}`);
  if (problems.length === 0) console.log("app-groups: OK");
  process.exit(problems.length ? 1 : 0);
}
