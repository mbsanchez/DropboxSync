/**
 * Launches the release binary after `tauri build` (Windows only).
 *
 * Prefers launching through the sparse-package **app model** (via the package
 * AUMID) so the process gets **package identity** — required for the Cloud Files
 * API status column (DBSYNC-57/58). A direct exe launch does NOT grant identity.
 * Falls back to a direct exe launch (identity-less; CfAPI features disable
 * themselves) when the sparse package isn't installed.
 */
import { spawn, execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

if (process.platform !== "win32") {
  console.error("dev:win is only supported on Windows.");
  process.exit(1);
}

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const exePath = path.join(
  __dirname,
  "..",
  "src-tauri",
  "target",
  "release",
  "dropbox_sync_desktop.exe",
);

if (!fs.existsSync(exePath)) {
  console.error(`Executable not found: ${exePath}`);
  console.error("Run `npm run bundle:win` successfully first.");
  process.exit(1);
}

/** Package family name of the installed sparse package, or null if not installed. */
function packageFamilyName() {
  try {
    const out = execFileSync(
      "powershell",
      [
        "-NoProfile",
        "-Command",
        "(Get-AppxPackage -Name DropboxSyncDesktop).PackageFamilyName",
      ],
      { encoding: "utf8" },
    ).trim();
    return out || null;
  } catch {
    return null;
  }
}

const family = packageFamilyName();
if (family) {
  // Launch through the shell app model → the process runs with package identity.
  const aumid = `${family}!DropboxSyncDesktop`;
  console.log(`Launching with package identity: ${aumid}`);
  const child = spawn("explorer.exe", [`shell:AppsFolder\\${aumid}`], {
    detached: true,
    stdio: "ignore",
  });
  child.unref();
} else {
  console.log(
    "Sparse package not installed — launching exe directly (no package identity; CfAPI features disabled).\n" +
      "Install it for the status column: native/windows/sparse-package/build-and-install.ps1 (elevated).",
  );
  const child = spawn(exePath, [], {
    detached: true,
    stdio: "ignore",
    windowsHide: false,
  });
  child.unref();
}
