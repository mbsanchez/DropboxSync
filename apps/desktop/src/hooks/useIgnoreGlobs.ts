import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { IgnoreGlobs } from "../types";

/** User-defined local ignore glob patterns (device-local concern, distinct from
 *  remote selective sync). Persisting also activates the patterns immediately. */
export function useIgnoreGlobs(pushLog: (line: string) => void) {
  const [ignoreGlobsCsv, setIgnoreGlobsCsv] = useState("");

  useEffect(() => {
    invoke<IgnoreGlobs>("get_ignore_globs")
      .then((globs) => {
        setIgnoreGlobsCsv(globs.csv ?? "");
      })
      .catch((e) => pushLog(`Failed to load ignore globs: ${String(e)}`));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const saveIgnoreGlobs = useCallback(async () => {
    try {
      await invoke("set_ignore_globs", { csv: ignoreGlobsCsv });
      pushLog("Ignored files patterns saved.");
    } catch (e) {
      pushLog(`Failed to save ignore globs: ${String(e)}`);
    }
  }, [ignoreGlobsCsv, pushLog]);

  return { ignoreGlobsCsv, setIgnoreGlobsCsv, saveIgnoreGlobs };
}
