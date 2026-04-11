/**
 * Launches the release binary after `tauri build` (Windows only).
 * Cargo binary name matches [package].name in src-tauri/Cargo.toml.
 */
import { spawn } from "node:child_process";
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

const child = spawn(exePath, [], {
  detached: true,
  stdio: "ignore",
  windowsHide: false,
});
child.unref();
