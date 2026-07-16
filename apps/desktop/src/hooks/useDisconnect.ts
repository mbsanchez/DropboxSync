import { useCallback, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/**
 * Disconnects Dropbox (DBSYNC-36 S3): the backend revokes the token and clears the
 * local session + sync-folder config (see `commands::disconnect_dropbox`). Once it
 * resolves, `get_startup_requirements` reports authOk=false / syncFolderOk=false, so
 * refreshing startup requirements is what actually routes `SetupApp` back to
 * onboarding — the caller must pass its `refreshStartupRequirements` in.
 */
export function useDisconnect(
  pushLog: (line: string) => void,
  refreshStartupRequirements: () => Promise<void>,
) {
  const [disconnecting, setDisconnecting] = useState(false);

  const disconnect = useCallback(async () => {
    setDisconnecting(true);
    try {
      await invoke("disconnect_dropbox");
      pushLog("Disconnected from Dropbox.");
    } catch (e) {
      pushLog(`Failed to disconnect: ${String(e)}`);
    } finally {
      // Refresh even on error so the UI reflects whatever the backend state ended
      // up in (partial failures still clear enough state to fail authOk).
      await refreshStartupRequirements();
      setDisconnecting(false);
    }
  }, [pushLog, refreshStartupRequirements]);

  return { disconnecting, disconnect };
}
