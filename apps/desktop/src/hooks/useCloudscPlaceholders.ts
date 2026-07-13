import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { CloudscPlaceholderInfo } from "../types";

type UseCloudscPlaceholdersParams = {
  pushLog: (line: string) => void;
  authOk: boolean;
  syncFolderOk: boolean;
  syncFolder: string;
  /** Current `status.syncRunning`; used to refresh the list once a background sync finishes. */
  syncRunning: boolean;
};

/** Read-only `.cloudsc` placeholder listing: auto-indexes the remote root once per
 * session, then re-lists after each background sync run. */
export function useCloudscPlaceholders({
  pushLog,
  authOk,
  syncFolderOk,
  syncFolder,
  syncRunning,
}: UseCloudscPlaceholdersParams) {
  const [cloudscEntries, setCloudscEntries] = useState<CloudscPlaceholderInfo[]>([]);
  const [cloudscLoading, setCloudscLoading] = useState(false);
  const didAutoIndexRef = useRef(false);
  const prevSyncRunningRef = useRef(false);

  const refreshCloudsc = useCallback(
    async (limit = 200) => {
      setCloudscLoading(true);
      try {
        const entries = await invoke<CloudscPlaceholderInfo[]>("list_cloudsc_placeholders", {
          limit,
        });
        setCloudscEntries(entries);
      } catch (e) {
        pushLog(`Failed to list .cloudsc placeholders: ${String(e)}`);
      } finally {
        setCloudscLoading(false);
      }
    },
    [pushLog],
  );

  useEffect(() => {
    if (!authOk || !syncFolderOk || !syncFolder) return;
    // Auto-index root `.cloudsc` placeholders once per app session.
    if (!didAutoIndexRef.current) {
      didAutoIndexRef.current = true;
      (async () => {
        pushLog("Indexing remote root placeholders (.cloudsc)...");
        try {
          const res = await invoke<number>("index_remote_root_placeholders");
          pushLog(`Remote root placeholders indexed. New: ${res}`);
        } catch (e) {
          pushLog(`Cloudsc indexing failed: ${String(e)}`);
        } finally {
          await refreshCloudsc().catch(() => {});
        }
      })();
    } else {
      refreshCloudsc().catch(() => {});
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [syncFolder, authOk, syncFolderOk]);

  useEffect(() => {
    const prev = prevSyncRunningRef.current;
    if (prev && !syncRunning) {
      refreshCloudsc().catch(() => {});
    }
    prevSyncRunningRef.current = syncRunning;
  }, [syncRunning, refreshCloudsc]);

  return { cloudscEntries, cloudscLoading, refreshCloudsc };
}
