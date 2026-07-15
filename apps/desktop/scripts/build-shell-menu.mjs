/**
 * Builds the Windows Explorer context-menu COM DLL
 * (native/windows/shell-menu) and copies it next to the release exe so the app
 * auto-registers it on startup (see src-tauri/src/windows_shell_menu.rs).
 *
 * Usage: npm run build:shell-menu   (Windows only)
 *
 * NOTE: if Explorer has already loaded a previous copy, the DLL is file-locked
 * and the copy will fail. Close Explorer windows / restart Explorer first:
 *   taskkill /f /im explorer.exe & start explorer
 */
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

if (process.platform !== "win32") {
  console.error("build:shell-menu is only supported on Windows.");
  process.exit(1);
}

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const crateDir = path.join(__dirname, "..", "native", "windows", "shell-menu");
const dllName = "dropbox_sync_shell_menu.dll";

console.log("Building shell-menu COM DLL (release)...");
execFileSync("cargo", ["build", "--release"], { cwd: crateDir, stdio: "inherit" });

const builtDll = path.join(crateDir, "target", "release", dllName);
if (!fs.existsSync(builtDll)) {
  console.error(`Build produced no DLL at ${builtDll}`);
  process.exit(1);
}

const destDir = path.join(__dirname, "..", "src-tauri", "target", "release");
fs.mkdirSync(destDir, { recursive: true });
const destDll = path.join(destDir, dllName);

try {
  fs.copyFileSync(builtDll, destDll);
} catch (err) {
  console.error(`Failed to copy DLL to ${destDll}: ${err.message}`);
  console.error(
    "If it is EBUSY/locked, Explorer has it loaded — restart Explorer and retry:",
  );
  console.error("  taskkill /f /im explorer.exe & start explorer");
  process.exit(1);
}

console.log(`Copied ${dllName} -> ${destDll}`);

// DBSYNC-62: also stage the out-of-proc CloudFilesContextMenus COM ExeServer, which
// the sparse-package manifest declares (com:ExeServer Executable). It must sit at the
// external location alongside the app exe so packaged COM activation can find it.
const cmdExe = "dropbox_sync_cloudmenu.exe";
const builtExe = path.join(crateDir, "target", "release", cmdExe);
if (fs.existsSync(builtExe)) {
  try {
    fs.copyFileSync(builtExe, path.join(destDir, cmdExe));
    console.log(`Copied ${cmdExe} -> ${path.join(destDir, cmdExe)}`);
  } catch (err) {
    console.error(`Failed to copy ${cmdExe}: ${err.message}`);
    process.exit(1);
  }
} else {
  console.error(`Build produced no ExeServer at ${builtExe}`);
  process.exit(1);
}
console.log(
  "Start the app (npm run dev:win) to register it, then restart Explorer to load the menu.",
);
