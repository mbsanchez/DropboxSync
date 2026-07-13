import { useCallback, useState } from "react";
import type { ActivityEntry } from "../types";

/** Rolling in-window activity/event log (most recent first, capped at 40 entries). */
export function useActivityLog() {
  const [activity, setActivity] = useState<ActivityEntry[]>([]);

  const pushLog = useCallback((line: string) => {
    const now = Date.now();
    setActivity((prev) =>
      [
        {
          id: `${now}-${Math.random().toString(16).slice(2, 8)}`,
          message: `${new Date(now).toLocaleTimeString()} - ${line}`,
          timestamp: now,
        },
        ...prev,
      ].slice(0, 40),
    );
  }, []);

  return { activity, pushLog };
}
