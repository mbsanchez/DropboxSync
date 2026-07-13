import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { StartupRequirements } from "../types";

/**
 * Tracks whether Dropbox auth + the local sync folder are configured. Refreshes on
 * mount and on window focus (e.g. coming back from the OAuth browser tab). Exposes
 * the raw setters too: `useAuthFlow` needs to update this same state for immediate
 * UI feedback before its own network round-trip (via `refreshStartupRequirements`)
 * resolves.
 */
export function useStartupRequirements(pushLog: (line: string) => void) {
  const [startupLoading, setStartupLoading] = useState(true);
  const [authOk, setAuthOk] = useState(false);
  const [syncFolderOk, setSyncFolderOk] = useState(false);
  const [syncFolder, setSyncFolder] = useState("");

  const refreshStartupRequirements = useCallback(async () => {
    try {
      const requirements = await invoke<StartupRequirements>("get_startup_requirements");
      setAuthOk(requirements.authOk);
      setSyncFolderOk(requirements.syncFolderOk);
      if (requirements.syncFolder) {
        setSyncFolder(requirements.syncFolder);
      }
    } catch (error) {
      pushLog(`Startup check failed: ${String(error)}`);
      setAuthOk(false);
    } finally {
      setStartupLoading(false);
    }
  }, [pushLog]);

  useEffect(() => {
    void refreshStartupRequirements();
  }, [refreshStartupRequirements]);

  useEffect(() => {
    const onFocus = () => {
      void refreshStartupRequirements();
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refreshStartupRequirements]);

  return {
    startupLoading,
    authOk,
    syncFolderOk,
    syncFolder,
    setSyncFolder,
    setAuthOk,
    setSyncFolderOk,
    setStartupLoading,
    refreshStartupRequirements,
  };
}
