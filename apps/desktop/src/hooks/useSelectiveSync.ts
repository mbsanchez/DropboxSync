import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { SelectiveSyncFilters } from "../types";

/** Include/exclude path-prefix filters for selective sync (settings-level concern). */
export function useSelectiveSync(pushLog: (line: string) => void) {
  const [includeCsv, setIncludeCsv] = useState("");
  const [excludeCsv, setExcludeCsv] = useState("");

  useEffect(() => {
    invoke<SelectiveSyncFilters>("get_selective_sync_filters")
      .then((filters) => {
        setIncludeCsv(filters.includeCsv ?? "");
        setExcludeCsv(filters.excludeCsv ?? "");
      })
      .catch((e) => pushLog(`Failed to load selective sync filters: ${String(e)}`));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const saveSelectiveSync = useCallback(async () => {
    try {
      // Tauri v2 expects camelCase keys for Rust snake_case args (DBSYNC-114).
      await invoke("set_selective_sync_filters", {
        includeCsv,
        excludeCsv,
      });
      pushLog("Selective sync filters saved.");
    } catch (e) {
      pushLog(`Failed to save selective sync filters: ${String(e)}`);
      throw e; // let useSaveAction show the failure instead of "Saved"
    }
  }, [includeCsv, excludeCsv, pushLog]);

  return { includeCsv, setIncludeCsv, excludeCsv, setExcludeCsv, saveSelectiveSync };
}
