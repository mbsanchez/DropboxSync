/**
 * Tauri beforeBuildCommand: frontend build + Finder Sync appex (macOS only), plus the host app's
 * entitlements (DBSYNC-97). `bundle.macOS.entitlements` in tauri.conf.json points at the rendered file.
 */
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

function run(cmd, args) {
  const result = spawnSync(cmd, args, {
    stdio: "inherit",
    cwd: root,
    shell: process.platform === "win32",
  });
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

run("npm", ["run", "build"]);
if (process.platform === "darwin") {
  // Rendered here, not committed: the App Group id carries the team id (Team ID: see
  // FinderSyncExtension/README.md, App Group ADR).
  // The renderer renders the group only when APPLE_TEAM_ID is set AND APPLE_SIGNING_IDENTITY is a real
  // identity (not unset, empty or "-"), and warns loudly otherwise. `tauri.conf.json` keeps
  // `signingIdentity: "-"`, so without that rule an ad-hoc `dev:mac`/`bundle:dev` with APPLE_TEAM_ID
  // exported would sign the host ad-hoc WITH the group. build-appex.sh goes through the same script.
  run("node", [
    "scripts/render-entitlements.mjs",
    "src-tauri/entitlements.host.plist",
    "src-tauri/.generated/host.entitlements",
  ]);
  run("npm", ["run", "build:finder-sync"]);
}
