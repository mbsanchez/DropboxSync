import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { ListRemoteFolderResponse, RemoteEntry } from "../types";

export function parentDropboxPath(p: string): string {
  const cleaned = p?.trim() ?? "";
  if (!cleaned || cleaned === "/") {
    return "";
  }
  const noTrailing = cleaned.endsWith("/") ? cleaned.slice(0, -1) : cleaned;
  const parts = noTrailing.split("/").filter(Boolean);
  parts.pop();
  return parts.length ? `/${parts.join("/")}` : "";
}

type UseRemoteBrowserParams = {
  pushLog: (line: string) => void;
  ready: boolean;
  /** Current `status.syncRunning`; used to reload the current folder once a background sync finishes. */
  syncRunning: boolean;
};

/** Read-only remote (Dropbox) tree browser: lists a folder's entries without
 * downloading anything. Auto-loads the root once `ready`, and reloads the current
 * folder after a background sync finishes so synced/excluded state stays current. */
export function useRemoteBrowser({ pushLog, ready, syncRunning }: UseRemoteBrowserParams) {
  const [remoteCurrentPath, setRemoteCurrentPath] = useState("");
  const [remoteEntries, setRemoteEntries] = useState<RemoteEntry[]>([]);
  const [remoteLoading, setRemoteLoading] = useState(false);
  const didAutoLoadRef = useRef(false);
  const prevSyncRunningRef = useRef(false);

  const loadRemoteFolder = useCallback(
    async (path: string) => {
      setRemoteLoading(true);
      try {
        const resp = await invoke<ListRemoteFolderResponse>("list_remote_folder", { path });
        setRemoteCurrentPath(resp.currentPath);
        setRemoteEntries(resp.entries);
        pushLog(`Remote folder loaded: ${resp.currentPath || "/"} (${resp.entries.length} entries)`);
      } catch (e) {
        pushLog(`Failed to load remote folder: ${String(e)}`);
      } finally {
        setRemoteLoading(false);
      }
    },
    [pushLog],
  );

  useEffect(() => {
    if (!ready || didAutoLoadRef.current) return;
    didAutoLoadRef.current = true;
    loadRemoteFolder("").catch(() => {});
  }, [ready, loadRemoteFolder]);

  useEffect(() => {
    const prev = prevSyncRunningRef.current;
    if (prev && !syncRunning && didAutoLoadRef.current) {
      // Reload the remote tree to reflect hydrations/downloads done in the background.
      loadRemoteFolder(remoteCurrentPath).catch(() => {});
    }
    prevSyncRunningRef.current = syncRunning;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [syncRunning]);

  return {
    remoteCurrentPath,
    remoteEntries,
    remoteLoading,
    loadRemoteFolder,
  };
}
