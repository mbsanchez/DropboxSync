import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import "./App.css";

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

type OauthCallbackPayload = {
  code: string;
  state: string;
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

function App() {
  const [authUrl, setAuthUrl] = useState("");
  const [oauthCode, setOauthCode] = useState("");
  const [state, setState] = useState("");
  const [syncFolder, setSyncFolder] = useState("");
  const [activity, setActivity] = useState<string[]>([]);
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

  const didAutoIndexCloudscRef = useRef(false);

  const canComplete = useMemo(() => oauthCode.length > 4 && state.length > 4, [oauthCode, state]);

  const pushLog = (line: string) => setActivity((prev) => [line, ...prev].slice(0, 20));

  const refreshDashboard = async () => {
    const dashboard = await invoke<SyncDashboard>("get_sync_dashboard");
    setStatus(dashboard.status);
    setJobs(dashboard.jobs);
    setConflicts(dashboard.conflicts);
    if (dashboard.status.trackedPath) {
      setSyncFolder(dashboard.status.trackedPath);
    }
  };

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

  const startOAuth = async () => {
    try {
      const payload = await invoke<{ authUrl: string; state: string }>("start_oauth_flow");
      setAuthUrl(payload.authUrl);
      setState(payload.state);
      setAwaitingCallback(true);
      await openUrl(payload.authUrl);
      pushLog("Opened Dropbox login. Waiting for callback on localhost.");
    } catch (error) {
      pushLog(`Could not open browser automatically: ${String(error)}`);
    }
  };

  const completeOAuth = async (codeArg?: string, stateArg?: string) => {
    pushLog("Completing Dropbox login...");
    try {
      await invoke("complete_oauth_flow", { code: codeArg ?? oauthCode, state: stateArg ?? state });
      setAwaitingCallback(false);
      pushLog("Dropbox authentication completed and token session stored securely.");
    } catch (error) {
      pushLog(`Complete login failed: ${String(error)}`);
      throw error;
    }
  };

  const saveFolder = async () => {
    await invoke("set_sync_folder", { folder: syncFolder });
    pushLog(`Sync folder configured: ${syncFolder}`);
    await refreshDashboard();
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

  useEffect(() => {
    if (!awaitingCallback) return;

    const interval = window.setInterval(async () => {
      try {
        const payload = await invoke<OauthCallbackPayload | null>("poll_oauth_callback");
        if (payload?.code && payload?.state) {
          setOauthCode(payload.code);
          setState(payload.state);
          pushLog("OAuth callback received from browser.");
          await completeOAuth(payload.code, payload.state);
        }
      } catch (error) {
        pushLog(`OAuth callback polling error: ${String(error)}`);
      }
    }, 1000);

    return () => window.clearInterval(interval);
  }, [awaitingCallback]);

  useEffect(() => {
    refreshDashboard().catch((error) => pushLog(`Dashboard load failed: ${String(error)}`));
  }, []);

  useEffect(() => {
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
  }, [syncFolder]);

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

  // Mientras el sync corre en segundo plano, refrescar el dashboard sin bloquear el hilo de UI.
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
        // Recargar el árbol remoto para reflejar hidratas/descargas hechas en background.
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
      <h1>Dropbox Sync Desktop (Week 2 MVP)</h1>

      <section className="card">
        <h2>1) Dropbox OAuth</h2>
        <button onClick={startOAuth}>Start Dropbox Login</button>
        {authUrl && (
          <p>
            Authorization URL: <a href={authUrl}>{authUrl}</a>
          </p>
        )}
        <input value={state} onChange={(e) => setState(e.currentTarget.value)} placeholder="state" />
        <input
          value={oauthCode}
          onChange={(e) => setOauthCode(e.currentTarget.value)}
          placeholder="code from redirect"
        />
        <button
          disabled={!canComplete}
          onClick={() => {
            completeOAuth().catch(() => {});
          }}
        >
          Complete Login
        </button>
      </section>

      <section className="card">
        <h2>2) Sync folder</h2>
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
        <h2>3) Remote Browser (On-demand)</h2>
        <p>Se lista lo remoto sin descargar. Click en “Sync” hidrata archivos/carpetas.</p>

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
        <h2>4) `.cloudsc` Placeholders (no descargar hasta click)</h2>
        <p>
          En el <code>Sync folder</code> aparecen archivos <code>*.cloudsc</code> con metadata. Si
          hidratas un placeholder, se descarga solo ese archivo o carpeta (y sus hijos inmediatos).
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
          {cloudscEntries.length === 0 && <li>No placeholders. Click “Index remote root” o esperá el index inicial.</li>}
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

      <section className="card">
        <h2>3) Status</h2>
        <p>Health: {status.health}</p>
        <p>Queue depth: {status.queueDepth}</p>
        <p>Tracked path: {status.trackedPath || "not configured"}</p>
        <p>Last scan: {status.lastScanAt || "never"}</p>
        <p>Processed jobs: {status.processedJobs}</p>
        <p>Conflicts detected: {status.conflictsDetected}</p>
        <p>Sync running: {status.syncRunning ? "yes" : "no"}</p>
        <p>Last error: {status.lastError || "none"}</p>
      </section>

      <section className="card">
        <h2>4) Queue and errors</h2>
        <ul>
          {jobs.slice(0, 8).map((job) => (
            <li key={job.id}>
              #{job.id} {job.jobType} {job.targetPath || "-"} | {job.status} | attempt {job.attemptCount}
            </li>
          ))}
        </ul>
      </section>

      <section className="card">
        <h2>5) Conflicts</h2>
        <ul>
          {conflicts.length === 0 && <li>No conflicts</li>}
          {conflicts.map((conflict) => (
            <li key={conflict.id}>
              {conflict.localPath} - {conflict.reason}
            </li>
          ))}
        </ul>
      </section>

      <section className="card">
        <h2>Activity</h2>
        <ul>
          {activity.map((line) => (
            <li key={line}>{line}</li>
          ))}
        </ul>
      </section>
    </main>
  );
}

export default App;
