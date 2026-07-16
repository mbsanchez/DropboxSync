import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/**
 * Start-at-login toggle (DBSYNC-36 S4), backed by the Windows StartupTask API
 * (`commands::get_startup_at_login` / `set_startup_at_login`). The backend rejects
 * on non-Windows platforms and on unpackaged dev builds that lack package identity —
 * that's an expected condition, not an error, so we quietly flip `supported` to
 * false (only a debug pushLog line) instead of surfacing it to the user. Callers
 * should hide the toggle entirely when `supported` is false.
 */
export function useStartupAtLogin(pushLog: (line: string) => void) {
  const [supported, setSupported] = useState(false);
  const [enabled, setEnabled] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    invoke<boolean>("get_startup_at_login")
      .then((result) => {
        setSupported(true);
        setEnabled(result);
      })
      .catch((e) => {
        setSupported(false);
        pushLog(`Start at login unavailable: ${String(e)}`);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const toggle = useCallback(async () => {
    setBusy(true);
    try {
      const result = await invoke<boolean>("set_startup_at_login", { enabled: !enabled });
      setEnabled(result);
    } catch (e) {
      pushLog(`Failed to change start at login: ${String(e)}`);
    } finally {
      setBusy(false);
    }
  }, [enabled, pushLog]);

  return { supported, enabled, busy, toggle };
}
