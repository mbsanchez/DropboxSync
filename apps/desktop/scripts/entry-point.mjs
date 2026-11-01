/**
 * Shared "is this module the script being run" check (DBSYNC-97), used by `render-entitlements.mjs`,
 * `check-app-groups.mjs` and `run-tests.mjs`.
 */
import { realpathSync } from "node:fs";
import { fileURLToPath } from "node:url";

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
