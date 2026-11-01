/**
 * Renders a macOS entitlements template into a gitignored file (DBSYNC-97).
 *
 * The App Group id is `<TEAM_ID>.com.dropboxsyncdesktop`. The team id comes only from the
 * `APPLE_TEAM_ID` environment variable. A team id is not a secret (every signed binary carries it), but
 * by the maintainer's choice this public repository never holds a literal one, so forks build with
 * their own team (older commits do contain the maintainer's; history is not rewritten). A template
 * holds the `__APP_GROUP_ID__` token; this script substitutes it, or drops the whole
 * `application-groups` key when there is no team, so a build can never sign a bare or wrong group.
 *
 *   node render-entitlements.mjs <template> <output>
 *
 * stdout: the group id, or nothing when there is no team (callers capture it). stderr: warnings.
 */
import { existsSync, lstatSync, mkdirSync, readFileSync, realpathSync, unlinkSync, writeFileSync } from "node:fs";
import { basename, dirname, join, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

export const GROUP_TOKEN = "__APP_GROUP_ID__";
export const GROUP_SUFFIX = "com.dropboxsyncdesktop";

const GROUP_KEY_BLOCK =
  /[ \t]*<key>com\.apple\.security\.application-groups<\/key>\s*<array>\s*<string>__APP_GROUP_ID__<\/string>\s*<\/array>[ \t]*\n?/;

/** `<TEAM>.com.dropboxsyncdesktop`, or `null` when there is no team. Throws on a malformed team. */
export function appGroupId(team) {
  const t = (team ?? "").trim();
  if (t === "") return null;
  if (!/^[A-Z0-9]{10}$/.test(t)) {
    throw new Error(`APPLE_TEAM_ID is not a valid Apple team id (10 upper-case letters or digits)`);
  }
  return `${t}.${GROUP_SUFFIX}`;
}

/** Whether `identity` (`APPLE_SIGNING_IDENTITY`) names a real certificate: not unset, empty or `-` (ad-hoc). */
export function isRealSigningIdentity(identity) {
  const t = (identity ?? "").trim();
  return t !== "" && t !== "-";
}

/**
 * Whether this build is signed with a real identity and so may carry the App Group. The same rule
 * serves the host (`tauri-before-build.mjs`) and the appex (`build-appex.sh`) because both go through
 * this script, so the two can never disagree. `reason` is `"ok"`, `"no-team"` or `"ad-hoc"`.
 *
 * An ad-hoc signature cannot honour a team-prefixed group; if it carried the entitlement anyway,
 * `SecTaskCopyValueForEntitlement` would report the process as entitled and the host would touch the
 * container, which can raise the macOS "access data from other apps" prompt. Throws on a malformed team.
 */
export function decideAppGroup(env) {
  const groupId = appGroupId(env.APPLE_TEAM_ID);
  if (groupId === null) return { groupId: null, reason: "no-team" };
  if (!isRealSigningIdentity(env.APPLE_SIGNING_IDENTITY)) return { groupId: null, reason: "ad-hoc" };
  return { groupId, reason: "ok" };
}

/** Substitutes the group id, or removes the `application-groups` key when `groupId` is null. */
export function renderEntitlements(template, groupId) {
  const out = groupId
    ? template.replaceAll(GROUP_TOKEN, groupId)
    : template.replace(GROUP_KEY_BLOCK, "");
  if (out.includes(GROUP_TOKEN)) {
    throw new Error(`unreplaced ${GROUP_TOKEN} left in the rendered entitlements`);
  }
  return out;
}

const REPO_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");

/** The only two directories this script may write to; both are gitignored. */
const OUTPUT_DIRS = [
  join("apps", "desktop", "src-tauri", ".generated"),
  join("apps", "desktop", "native", "macos", "FinderSyncExtension", "build"),
];

/**
 * `path` with symlinks resolved as far as it exists: the deepest existing ancestor is `realpathSync`ed
 * and the not-yet-created remainder is appended. Lets a directory that does not exist yet be compared
 * with the real location of the whitelist.
 */
export function realpathLoose(path) {
  const abs = resolve(path);
  let existing = abs;
  const rest = [];
  while (!existsSync(existing) && !isDanglingLink(existing)) {
    const parent = dirname(existing);
    if (parent === existing) break;
    rest.unshift(basename(existing));
    existing = parent;
  }
  let real = existing;
  try {
    real = realpathSync(existing);
  } catch {
    // A dangling link cannot be resolved; keep it as is, it will not match the whitelist.
  }
  return join(real, ...rest);
}

function isDanglingLink(path) {
  try {
    return lstatSync(path).isSymbolicLink();
  } catch {
    return false;
  }
}

/**
 * The path a write to `path` would really land on: the directory is resolved through symlinks, the file
 * name is not. A symlink at the file name itself is handled by `main()` (unlinked, then created fresh), not
 * followed.
 */
export function resolveOutput(path) {
  const abs = resolve(path);
  return join(realpathLoose(dirname(abs)), basename(abs));
}

/**
 * Output must land in one of the two gitignored directories above, resolved against the repository
 * root, so a rendered team id can never be committed. A directory merely named `build` or
 * `.generated` somewhere else (a tracked `scripts/build/`, or a checkout under a parent called
 * `build`) does not qualify. Symlinks are resolved on both sides, so a symlinked directory inside an
 * output directory cannot point the write at a tracked file, and a symlinked checkout path still matches.
 */
export function isGeneratedPath(path, root = REPO_ROOT) {
  const target = resolveOutput(path);
  // Only the root is resolved: the output directories themselves must be real directories. If
  // `.generated` were a symlink to a tracked directory, resolving it on this side too would make the
  // target and the whitelist agree on the wrong place.
  const realRoot = realpathLoose(root);
  return OUTPUT_DIRS.some((dir) => target.startsWith(join(realRoot, dir) + sep));
}

/**
 * Whether the module `importMetaUrl` is the script being run. Compares real paths on both sides: through
 * a symlinked path (`/tmp` and `/var` are symlinks on macOS) a plain string comparison is false, `main()`
 * would silently not run and the process would exit 0 having done nothing.
 */
export function isEntryPoint(importMetaUrl, argv1 = process.argv[1]) {
  if (!argv1) return false;
  try {
    return realpathSync(argv1) === realpathSync(fileURLToPath(importMetaUrl));
  } catch {
    return false;
  }
}

function main() {
  const [template, output] = process.argv.slice(2);
  if (!template || !output) {
    console.error("usage: render-entitlements.mjs <template> <output>");
    process.exit(2);
  }
  const target = resolveOutput(output);
  if (!isGeneratedPath(target)) {
    console.error(`refusing to write ${output}: not inside the gitignored src-tauri/.generated/ or FinderSyncExtension/build/`);
    process.exit(2);
  }
  let decision;
  try {
    decision = decideAppGroup(process.env);
  } catch (e) {
    console.error(`ERROR: ${e.message}`);
    process.exit(1);
  }
  const { groupId, reason } = decision;
  if (reason === "no-team") {
    console.error("WARNING: APPLE_TEAM_ID is not set. Rendering entitlements WITHOUT an App Group.");
    console.error("         The app cannot open its data directory and will fail at startup; the Finder");
    console.error("         extension will show no badges. Export APPLE_TEAM_ID for a working build.");
  } else if (reason === "ad-hoc") {
    console.error("WARNING: APPLE_SIGNING_IDENTITY is unset, empty or '-' (ad-hoc signing). Rendering entitlements");
    console.error("         WITHOUT an App Group even though APPLE_TEAM_ID is set: an ad-hoc signature must not");
    console.error("         claim a team-prefixed group. The app will fail at startup and the Finder extension");
    console.error("         will show no badges. Export APPLE_SIGNING_IDENTITY (a Developer ID) for a working build.");
  }
  const rendered = renderEntitlements(readFileSync(template, "utf8"), groupId);
  mkdirSync(dirname(target), { recursive: true });
  // writeFileSync follows a symlink planted at the output path and would overwrite whatever it points
  // to (possibly a tracked file) with the rendered team id. Remove the name first, then create it with
  // O_EXCL ("wx"), which refuses to follow a link even if one is planted between the two calls.
  if (existsSync(target) || isDanglingLink(target)) unlinkSync(target);
  writeFileSync(target, rendered, { flag: "wx" });
  if (groupId) process.stdout.write(`${groupId}\n`);
}

if (isEntryPoint(import.meta.url)) main();
