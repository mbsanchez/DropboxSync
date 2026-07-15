import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type { SyncConflict, SyncDashboard, SyncJob, SyncStatus } from "../types";

const idleStatus: SyncStatus = {
  health: "idle",
  queueDepth: 0,
  processedJobs: 0,
  conflictsDetected: 0,
  syncRunning: false,
};

/** Sync status/queue/conflicts, background-refreshed while a sync is running, plus
 * the Rust-emitted progress/conflict events. Refreshes on mount and on window focus. */
export function useSyncDashboard(pushLog: (line: string) => void) {
  const [status, setStatus] = useState<SyncStatus>(idleStatus);
  const [jobs, setJobs] = useState<SyncJob[]>([]);
  const [conflicts, setConflicts] = useState<SyncConflict[]>([]);
  const prevSyncRunningRef = useRef(false);

  const refreshDashboard = useCallback(async () => {
    try {
      const dashboard = await invoke<SyncDashboard>("get_sync_dashboard");
      setStatus(dashboard.status);
      setJobs(dashboard.jobs);
      setConflicts(dashboard.conflicts);
    } catch (e) {
      pushLog(`Dashboard refresh failed: ${String(e)}`);
    }
  }, [pushLog]);

  const retryFailedJobs = useCallback(async () => {
    try {
      const requeued = await invoke<number>("retry_failed_jobs");
      pushLog(`Requeued ${requeued} failed job(s).`);
      await refreshDashboard();
    } catch (error) {
      pushLog(`Retry failed jobs error: ${String(error)}`);
    }
  }, [pushLog, refreshDashboard]);

  // DBSYNC-35: apply the user's conflict decision (keep_local / use_remote / keep_both).
  // Optimistically drop the row so it disappears immediately, then reconcile from the DB.
  const resolveConflict = useCallback(
    async (id: number, action: "keep_local" | "use_remote" | "keep_both") => {
      setConflicts((prev) => prev.filter((c) => c.id !== id));
      try {
        await invoke("resolve_conflict", { id, action });
        pushLog(`Conflict ${id} resolved (${action}).`);
      } catch (error) {
        pushLog(`Resolve conflict error: ${String(error)}`);
      }
      await refreshDashboard();
    },
    [pushLog, refreshDashboard],
  );

  useEffect(() => {
    void refreshDashboard();
  }, [refreshDashboard]);

  useEffect(() => {
    const onFocus = () => {
      void refreshDashboard();
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refreshDashboard]);

  // Rust emits `upload-progress` while streaming a large file through the chunked
  // upload-session API, so the log shows liveness for uploads that take a while.
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    void listen<{ path: string; transferred: number; total: number }>("upload-progress", (event) => {
      if (!active) return;
      pushLog(`Uploading ${event.payload.path}: ${event.payload.transferred}/${event.payload.total} bytes`);
    }).then((fn) => {
      if (!active) {
        fn();
      } else {
        unlisten = fn;
      }
    });

    return () => {
      active = false;
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Rust emits `sync-conflict` when a remote download would overwrite unsynced local
  // changes, so the log and dashboard reflect the preserved conflicted copy immediately.
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    void listen<{ path: string; conflictPath: string; reason: string }>("sync-conflict", (event) => {
      if (!active) return;
      pushLog(`Conflict on ${event.payload.path}: local changes preserved as ${event.payload.conflictPath} (${event.payload.reason})`);
      void refreshDashboard();
    }).then((fn) => {
      if (!active) {
        fn();
      } else {
        unlisten = fn;
      }
    });

    return () => {
      active = false;
      unlisten?.();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // While sync runs in the background, refresh the dashboard without blocking the UI thread.
  useEffect(() => {
    if (!status.syncRunning) return;
    const id = window.setInterval(() => {
      refreshDashboard().catch(() => {});
    }, 400);
    return () => window.clearInterval(id);
  }, [status.syncRunning, refreshDashboard]);

  useEffect(() => {
    const prev = prevSyncRunningRef.current;
    if (prev && !status.syncRunning) {
      refreshDashboard().catch(() => {});
      if (status.lastError) {
        pushLog(`Sync finished with error: ${status.lastError}`);
      } else {
        pushLog(`Sync finished successfully. Last scan: ${status.lastScanAt ?? "n/a"}`);
      }
    }
    prevSyncRunningRef.current = status.syncRunning;
  }, [status.syncRunning, status.lastError, status.lastScanAt, refreshDashboard, pushLog]);

  return { status, jobs, conflicts, refreshDashboard, retryFailedJobs, resolveConflict };
}
