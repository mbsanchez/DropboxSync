import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import "./App.css";

type StartupRequirements = {
  authOk: boolean;
  syncFolderOk: boolean;
  syncFolder?: string;
};

type SyncStatus = {
  health: "idle" | "syncing" | "error";
  queueDepth: number;
  trackedPath?: string;
  lastError?: string;
  lastScanAt?: string;
  processedJobs: number;
  conflictsDetected: number;
  syncRunning: boolean;
};

type SyncJob = {
  id: number;
  jobType: string;
  targetPath?: string;
  status: string;
  attemptCount: number;
  nextRetryAt?: string;
};

type SyncConflict = {
  id: number;
  localPath: string;
  reason: string;
  createdAt: string;
};

type SyncDashboard = {
  status: SyncStatus;
  jobs: SyncJob[];
  conflicts: SyncConflict[];
};

type RemoteEntry = {
  tag: "file" | "folder" | string;
  pathDisplay: string;
  size?: number;
  isSynced: boolean;
  isExcluded: boolean;
};

type ListRemoteFolderResponse = {
  currentPath: string;
  entries: RemoteEntry[];
};

type SelectiveSyncFilters = {
  includeCsv: string;
  excludeCsv: string;
};

type TriggerActionResponse = {
  accepted: boolean;
};

type CloudscPlaceholderInfo = {
  localPathDisplay: string; // path relative to sync folder, e.g. "Videos/a.mp4.cloudsc"
  tag: "file" | "folder" | string;
  remotePathDisplay: string;
};

type ActivityEntry = {
  id: string;
  message: string;
};

function App() {
  const [authUrl, setAuthUrl] = useState("");
  const [syncFolder, setSyncFolder] = useState("");
  const [activity, setActivity] = useState<ActivityEntry[]>([]);
  const [status, setStatus] = useState<SyncStatus>({
    health: "idle",
    queueDepth: 0,
    processedJobs: 0,
    conflictsDetected: 0,
    syncRunning: false,
  });
  const [jobs, setJobs] = useState<SyncJob[]>([]);
  const [conflicts, setConflicts] = useState<SyncConflict[]>([]);
  const [awaitingCallback, setAwaitingCallback] = useState(false);
  const [autoSyncEnabled, setAutoSyncEnabled] = useState(false);
  const tickInFlightRef = useRef(false);
  const prevSyncRunningRef = useRef(false);

  const [remoteCurrentPath, setRemoteCurrentPath] = useState("");
  const [remoteEntries, setRemoteEntries] = useState<RemoteEntry[]>([]);
  const [remoteLoading, setRemoteLoading] = useState(false);
  const [includeCsv, setIncludeCsv] = useState("");
  const [excludeCsv, setExcludeCsv] = useState("");

  const [cloudscEntries, setCloudscEntries] = useState<CloudscPlaceholderInfo[]>([]);
  const [cloudscLoading, setCloudscLoading] = useState(false);
  const [startupLoading, setStartupLoading] = useState(true);
  const [authOk, setAuthOk] = useState(false);
  const [syncFolderOk, setSyncFolderOk] = useState(false);
  const [showFolderSetup, setShowFolderSetup] = useState(false);
  /** Shown on the Connect card (activity log is hidden until the dashboard is reachable). */
  const [connectError, setConnectError] = useState<string | null>(null);

  const didAutoIndexCloudscRef = useRef(false);
  const schedulerStartedRef = useRef(false);
  const loggedTrayHideRef = useRef(false);

  const pushLog = (line: string) =>
    setActivity((prev) => [
      {
        id: `${Date.now()}-${Math.random().toString(16).slice(2, 8)}`,
        message: `${new Date().toLocaleTimeString()} - ${line}`,
      },
      ...prev,
    ].slice(0, 40));

  const refreshDashboard = async () => {
    const dashboard = await invoke<SyncDashboard>("get_sync_dashboard");
    setStatus(dashboard.status);
    setJobs(dashboard.jobs);
    setConflicts(dashboard.conflicts);
    if (dashboard.status.trackedPath) {
      setSyncFolder(dashboard.status.trackedPath);
    }
  };

  const refreshStartupRequirements = async () => {
    try {
      const requirements = await invoke<StartupRequirements>("get_startup_requirements");
      setAuthOk(requirements.authOk);
      setSyncFolderOk(requirements.syncFolderOk);
      if (requirements.syncFolder) {
        setSyncFolder(requirements.syncFolder);
      }
      if (requirements.authOk) {
        setAwaitingCallback(false);
      }
    } catch (error) {
      pushLog(`Startup check failed: ${String(error)}`);
      setAuthOk(false);
    } finally {
      setStartupLoading(false);
    }
  };

  const refreshStartupRequirementsRef = useRef(refreshStartupRequirements);
  refreshStartupRequirementsRef.current = refreshStartupRequirements;

  const refreshCloudsc = async (limit = 200) => {
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
  };

  const cancelOAuth = async () => {
    try {
      await invoke("cancel_oauth_flow");
    } catch (error) {
      pushLog(`Cancel OAuth failed: ${String(error)}`);
    }
    setAwaitingCallback(false);
    setConnectError(null);
    pushLog("Dropbox login cancelled. You can start again when ready.");
  };

  const startOAuth = async () => {
    setConnectError(null);
    try {
      const payload = await invoke<{ authUrl: string; state: string }>("start_oauth_flow");
      setAuthUrl(payload.authUrl);
      setAwaitingCallback(true);
      setShowFolderSetup(false);
      await openUrl(payload.authUrl);
      pushLog("Opened Dropbox login. Waiting for callback on localhost.");
    } catch (error) {
      const msg = String(error);
      setConnectError(msg);
      pushLog(`Could not start OAuth: ${msg}`);
      setAwaitingCallback(false);
    }
  };

  const saveFolder = async () => {
    await invoke("set_sync_folder", { folder: syncFolder });
    pushLog(`Sync folder configured: ${syncFolder}`);
    await refreshDashboard();
    await refreshStartupRequirements();
  };

  const runTick = async () => {
    if (!status.trackedPath || status.syncRunning || tickInFlightRef.current) {
      return;
    }
    tickInFlightRef.current = true;
    try {
      const result = await invoke<{ accepted: boolean }>("trigger_sync_tick");
      if (result.accepted) {
        pushLog("Sync tick started in background.");
      } else {
        pushLog("Sync already running; skipping duplicate request.");
      }
      window.setTimeout(() => {
        refreshDashboard().catch((error) => pushLog(`Dashboard refresh failed: ${String(error)}`));
      }, 350);
    } finally {
      tickInFlightRef.current = false;
    }
  };

  // Primary: Rust emits `dropbox-oauth-finished` after token exchange (works while WebView is throttled).
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    void listen<{ ok: boolean; message?: string }>("dropbox-oauth-finished", (event) => {
      if (!active) return;
      setAwaitingCallback(false);
      if (event.payload.ok) {
        setConnectError(null);
        setAuthOk(true);
        setStartupLoading(false);
        pushLog("Dropbox authentication completed and token session stored securely.");
        void refreshStartupRequirementsRef.current();
      } else {
        const msg = event.payload.message ?? "unknown error";
        setConnectError(msg);
        pushLog(`Dropbox login failed: ${msg}`);
      }
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
  }, []);

  // Fallback while waiting: slow poll (WebView often throttles `setInterval` when the browser has focus).
  useEffect(() => {
    if (!awaitingCallback) return;

    let disposed = false;

    const tick = async () => {
      if (disposed) return;
      try {
        const requirements = await invoke<StartupRequirements>("get_startup_requirements");
        if (disposed || !requirements.authOk) return;
        setAwaitingCallback(false);
        setAuthOk(true);
        setSyncFolderOk(requirements.syncFolderOk);
        if (requirements.syncFolder) {
          setSyncFolder(requirements.syncFolder);
        }
        setStartupLoading(false);
        pushLog("Dropbox authentication completed and token session stored securely.");
      } catch (e) {
        pushLog(`OAuth status check failed: ${String(e)}`);
      }
    };

    const interval = window.setInterval(() => {
      void tick();
    }, 1500);
    void tick();

    return () => {
      disposed = true;
      window.clearInterval(interval);
    };
  }, [awaitingCallback]);

  useEffect(() => {
    pushLog("UI ready.");
    refreshDashboard().catch((error) => pushLog(`Dashboard load failed: ${String(error)}`));
    void refreshStartupRequirements();
  }, []);

  useEffect(() => {
    const onFocus = () => {
      void refreshStartupRequirements();
      void refreshDashboard();
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, []);

  useEffect(() => {
    if (!authOk || !syncFolderOk || schedulerStartedRef.current) return;
    invoke<boolean>("start_background_scheduler")
      .then((started) => {
        schedulerStartedRef.current = true;
        if (started) {
          pushLog("Background sync scheduler started (every 60s).");
        }
      })
      .catch((e) => pushLog(`Failed to start scheduler: ${String(e)}`));
  }, [authOk, syncFolderOk]);

  useEffect(() => {
    if (startupLoading) return;
    const fullyReady = authOk && syncFolderOk;
    if (fullyReady) {
      invoke("hide_main_window").catch(() => {});
      if (!loggedTrayHideRef.current) {
        loggedTrayHideRef.current = true;
        pushLog("App moved to tray. Use tray menu to Exit.");
      }
    } else {
      loggedTrayHideRef.current = false;
      invoke("show_main_window").catch(() => {});
    }
  }, [startupLoading, authOk, syncFolderOk]);

  useEffect(() => {
    if (!authOk) {
      setShowFolderSetup(false);
    }
  }, [authOk]);

  useEffect(() => {
    if (!authOk || !syncFolderOk) return;
    if (!syncFolder) return;
    // Auto-index root `.cloudsc` placeholders once per app session.
    if (!didAutoIndexCloudscRef.current) {
      didAutoIndexCloudscRef.current = true;
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
  }, [syncFolder, authOk, syncFolderOk]);

  useEffect(() => {
    invoke<SelectiveSyncFilters>("get_selective_sync_filters")
      .then((filters) => {
        setIncludeCsv(filters.includeCsv ?? "");
        setExcludeCsv(filters.excludeCsv ?? "");
      })
      .catch((e) => pushLog(`Failed to load selective sync filters: ${String(e)}`));
  }, []);

  const loadRemoteFolder = async (path: string) => {
    setRemoteLoading(true);
    try {
      const resp = await invoke<ListRemoteFolderResponse>("list_remote_folder", { path });
      setRemoteCurrentPath(resp.currentPath);
      setRemoteEntries(resp.entries);
      pushLog(`Remote folder loaded: ${resp.currentPath || "/"} (${resp.entries.length} entries)`);
    } finally {
      setRemoteLoading(false);
    }
  };

  const parentDropboxPath = (p: string) => {
    const cleaned = p?.trim() ?? "";
    if (!cleaned || cleaned === "/") {
      return "";
    }
    const noTrailing = cleaned.endsWith("/") ? cleaned.slice(0, -1) : cleaned;
    const parts = noTrailing.split("/").filter(Boolean);
    parts.pop();
    return parts.length ? `/${parts.join("/")}` : "";
  };

  const queueActionLabel = (jobType: string) => {
    switch (jobType) {
      case "upload":
        return "Upload (local -> Dropbox)";
      case "download":
        return "Download (Dropbox -> local)";
      case "delete":
        return "Delete (Dropbox)";
      case "hydrate_cloudsc":
        return "Hydrate (.cloudsc)";
      default:
        return jobType;
    }
  };

  // While sync runs in the background, refresh the dashboard without blocking the UI thread.
  useEffect(() => {
    if (!status.syncRunning) return;
    const id = window.setInterval(() => {
      refreshDashboard().catch(() => {});
    }, 400);
    return () => window.clearInterval(id);
  }, [status.syncRunning]);

  useEffect(() => {
    if (!autoSyncEnabled || !status.trackedPath) return;

    const interval = window.setInterval(() => {
      runTick().catch((error) => {
        tickInFlightRef.current = false;
        pushLog(`Tick failed: ${String(error)}`);
      });
    }, 10000);

    return () => window.clearInterval(interval);
  }, [autoSyncEnabled, status.trackedPath]);

  useEffect(() => {
    const prev = prevSyncRunningRef.current;
    if (prev && !status.syncRunning) {
      refreshDashboard().catch(() => {});
      refreshCloudsc().catch(() => {});
      if (remoteCurrentPath !== undefined) {
        // Reload the remote tree to reflect hydrations/downloads done in the background.
        loadRemoteFolder(remoteCurrentPath).catch(() => {});
      }
      if (status.lastError) {
        pushLog(`Sync finished with error: ${status.lastError}`);
      } else {
        pushLog(`Sync finished successfully. Last scan: ${status.lastScanAt ?? "n/a"}`);
      }
    }
    prevSyncRunningRef.current = status.syncRunning;
  }, [status.syncRunning, status.lastError, status.lastScanAt]);

  return (
    <main className="container">
      <section className="card hero">
        <h1 className="title">Dropbox Sync Desktop</h1>
        <p className="subtitle">Smart placeholders, selective hydration, and background sync.</p>
      </section>

      {startupLoading && (
        <section className="card onboarding">
          <h2>Starting...</h2>
          <p>Checking Dropbox connection and local sync settings.</p>
        </section>
      )}

      {!startupLoading && !authOk && (
        <section className="card onboarding">
          <h2>Connect Dropbox</h2>
          <p>Sign in with Dropbox in your browser. Use Retry if the browser tab closed or something went wrong.</p>
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
            <button type="button" disabled={authOk} onClick={startOAuth}>
              {awaitingCallback ? "Retry login" : "Start Dropbox login"}
            </button>
            {awaitingCallback && (
              <button type="button" onClick={cancelOAuth}>
                Cancel
              </button>
            )}
          </div>
          {awaitingCallback && <p>Waiting for Dropbox confirmation...</p>}
          {connectError && (
            <p role="alert" style={{ color: "#b00020", marginTop: 12, whiteSpace: "pre-wrap" }}>
              {connectError}
            </p>
          )}
        </section>
      )}

      {!startupLoading && authOk && !syncFolderOk && !showFolderSetup && (
        <section className="card onboarding">
          <h2>Dropbox connected</h2>
          <p>Your token was registered successfully. Continue to choose the local sync folder.</p>
          <button onClick={() => setShowFolderSetup(true)}>Next</button>
        </section>
      )}

      {!startupLoading && authOk && !syncFolderOk && showFolderSetup && (
        <section className="card onboarding">
          <h2>Choose Sync Folder</h2>
          <p>Select the local folder where placeholders and hydrated files will be stored.</p>
          <div style={{ display: "flex", gap: 8 }}>
            <input
              value={syncFolder}
              onChange={(e) => setSyncFolder(e.currentTarget.value)}
              placeholder="/Users/me/DropboxSync"
            />
            <button
              onClick={async () => {
                const selected = await invoke<string | null>("pick_sync_folder_dialog");
                if (selected) {
                  setSyncFolder(selected);
                }
              }}
            >
              ...
            </button>
            <button onClick={saveFolder}>Save</button>
          </div>
        </section>
      )}

      {!startupLoading && authOk && syncFolderOk && (
        <>

      <section className="card dashboard">
        <h2>Dashboard</h2>
        <p className="subtitle" style={{ marginTop: 0 }}>
          Global sync status, queue, and recent activity.
        </p>
        <div className="dashboard-grid">
          <div>
            <h3>Status</h3>
            <p>Health: {status.health}</p>
            <p>Queue: {status.queueDepth} pending jobs</p>
            <p>Folder: {status.trackedPath || "—"}</p>
            <p>Last scan: {status.lastScanAt || "never"}</p>
            <p>Processed: {status.processedJobs}</p>
            <p>Conflicts detected: {status.conflictsDetected}</p>
            <p>Syncing: {status.syncRunning ? "yes" : "no"}</p>
            <p>Last error: {status.lastError || "none"}</p>
          </div>
          <div>
            <h3>Queue (recent)</h3>
            <ul>
              {jobs.length === 0 && <li>Queue empty</li>}
              {jobs.slice(0, 8).map((job) => (
                <li key={job.id}>
                  #{job.id} {queueActionLabel(job.jobType)} {job.targetPath || "-"} | {job.status} | attempt{" "}
                  {job.attemptCount}
                </li>
              ))}
            </ul>
          </div>
          <div>
            <h3>Conflicts</h3>
            <ul>
              {conflicts.length === 0 && <li>No conflicts</li>}
              {conflicts.map((conflict) => (
                <li key={conflict.id}>
                  {conflict.localPath} — {conflict.reason}
                </li>
              ))}
            </ul>
          </div>
        </div>
        <h3>Activity / logs</h3>
        <ul className="activity-list">
          {activity.length === 0 && <li>No entries yet.</li>}
          {activity.map((entry) => (
            <li key={entry.id}>{entry.message}</li>
          ))}
        </ul>
      </section>

      <section className="card">
        <h2>Connection and local folder</h2>
        <p className="subtitle" style={{ marginTop: 0 }}>
          Reconnect Dropbox, change the folder, or run a manual sync tick.
        </p>
        <h3>Dropbox OAuth</h3>
        <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
          <button type="button" onClick={startOAuth}>
            Reconnect Dropbox
          </button>
          {awaitingCallback && (
            <button type="button" onClick={cancelOAuth}>
              Cancel OAuth
            </button>
          )}
        </div>
        {authUrl && (
          <p>
            Authorization URL: <a href={authUrl}>{authUrl}</a>
          </p>
        )}
        <h3>Sync folder</h3>
        <input
          value={syncFolder}
          onChange={(e) => setSyncFolder(e.currentTarget.value)}
          placeholder="/Users/me/DropboxSync"
        />
        <button onClick={saveFolder}>Save Folder</button>
        <button onClick={runTick}>Run Sync Tick</button>
        <button onClick={() => setAutoSyncEnabled((v) => !v)}>
          {autoSyncEnabled ? "Disable Auto Sync" : "Enable Auto Sync"}
        </button>
      </section>

      <section className="card">
        <h2>Remote Browser (On-demand)</h2>
        <p>Lists the remote tree without downloading. Use “Sync” to hydrate files or folders.</p>

        <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
          <button disabled={remoteLoading} onClick={() => loadRemoteFolder("")}>
            {remoteLoading ? "Loading..." : "Load remote root"}
          </button>
          <button
            disabled={remoteLoading || !remoteCurrentPath}
            onClick={() => loadRemoteFolder(parentDropboxPath(remoteCurrentPath))}
          >
            Up
          </button>
        </div>

        <p>Remote path: {remoteCurrentPath || "/"}</p>

        <div style={{ display: "grid", gap: 8 }}>
          <input
            value={includeCsv}
            onChange={(e) => setIncludeCsv(e.currentTarget.value)}
            placeholder="Include prefixes (CSV), e.g. Fotos,Videos/2024"
          />
          <input
            value={excludeCsv}
            onChange={(e) => setExcludeCsv(e.currentTarget.value)}
            placeholder="Exclude prefixes (CSV), e.g. Videos/Secret"
          />
          <button
            onClick={async () => {
              await invoke("set_selective_sync_filters", {
                include_csv: includeCsv,
                exclude_csv: excludeCsv,
              });
              pushLog("Selective sync filters saved.");
              if (!remoteLoading) {
                await loadRemoteFolder(remoteCurrentPath || "");
              }
            }}
          >
            Save selective sync
          </button>
        </div>

        <ul>
          {remoteEntries.length === 0 && <li>No entries. Load remote root to start.</li>}
          {remoteEntries.map((entry) => {
            const statusLabel = entry.isExcluded
              ? "Excluded"
              : entry.isSynced
                ? "Synced"
                : "Not downloaded";
            return (
              <li key={entry.pathDisplay}>
                {entry.tag === "folder" ? "[DIR]" : "[FILE]"} {entry.pathDisplay} | {statusLabel} |{" "}
                {entry.tag === "folder" ? (
                  <>
                    <button disabled={remoteLoading} onClick={() => loadRemoteFolder(entry.pathDisplay)}>
                      Open
                    </button>
                    <button
                      disabled={remoteLoading || entry.isExcluded || status.syncRunning}
                      onClick={async () => {
                        const res = await invoke<TriggerActionResponse>("trigger_hydrate_remote_folder", {
                          folder_path_display: entry.pathDisplay,
                        });
                        if (res.accepted) {
                          pushLog("Hydrating remote folder in background...");
                        }
                      }}
                    >
                      Sync folder
                    </button>
                  </>
                ) : (
                  <button
                    disabled={remoteLoading || entry.isExcluded || status.syncRunning}
                    onClick={async () => {
                      const res = await invoke<TriggerActionResponse>("trigger_download_remote_file", {
                        path_display: entry.pathDisplay,
                      });
                      if (res.accepted) {
                        pushLog("Syncing remote file in background...");
                      }
                    }}
                  >
                    Sync file
                  </button>
                )}
              </li>
            );
          })}
        </ul>
      </section>

      <section className="card">
        <h2>`.cloudsc` placeholders (on-demand download)</h2>
        <p>
          The <code>Sync folder</code> may contain <code>*.cloudsc</code> metadata placeholders.
          Hydrating a placeholder downloads only that file or folder (and its immediate children for folders).
        </p>

        <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
          <button
            disabled={cloudscLoading || status.syncRunning}
            onClick={async () => {
              setCloudscLoading(true);
              try {
                const created = await invoke<number>("index_remote_root_placeholders");
                pushLog(`Indexed remote root placeholders. New: ${created}`);
                await refreshCloudsc().catch(() => {});
              } finally {
                setCloudscLoading(false);
              }
            }}
          >
            {cloudscLoading ? "Indexing..." : "Index remote root"}
          </button>
          <button disabled={cloudscLoading || status.syncRunning} onClick={() => refreshCloudsc()}>
            {cloudscLoading ? "Refreshing..." : "Refresh placeholders"}
          </button>
        </div>

        <ul>
          {cloudscEntries.length === 0 && (
            <li>No placeholders. Click “Index remote root” or wait for the initial index.</li>
          )}
          {cloudscEntries.map((entry) => {
            const label = entry.tag === "folder" ? "[DIR]" : "[FILE]";
            return (
              <li key={entry.localPathDisplay}>
                {label} {entry.localPathDisplay} | {entry.remotePathDisplay}{" "}
                <button
                  disabled={cloudscLoading || status.syncRunning}
                  onClick={async () => {
                    try {
                      const res = await invoke<TriggerActionResponse>(
                        "trigger_hydrate_cloudsc_placeholder",
                        { placeholderLocalRelPath: entry.localPathDisplay }
                      );
                      if (res.accepted) {
                        pushLog(`Hydrating placeholder: ${entry.localPathDisplay}`);
                        await refreshDashboard().catch(() => {});
                      } else {
                        pushLog("Sync already running; hydration skipped.");
                      }
                    } catch (e) {
                      pushLog(`Hydration request failed: ${String(e)}`);
                    }
                  }}
                >
                  Hydrate
                </button>
              </li>
            );
          })}
        </ul>
      </section>

        </>
      )}
    </main>
  );
}

export default App;
