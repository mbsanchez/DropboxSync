import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useActivityLog } from "../hooks/useActivityLog";
import { useStartupRequirements } from "../hooks/useStartupRequirements";
import { useSyncDashboard } from "../hooks/useSyncDashboard";
import { useCloudscPlaceholders } from "../hooks/useCloudscPlaceholders";
import { useRemoteBrowser, parentDropboxPath } from "../hooks/useRemoteBrowser";
import type { SyncJob } from "../types";
import "./FlyoutApp.css";

type Section = "home" | "folders" | "activity";

const NAV_ITEMS: { id: Section; label: string; glyph: string }[] = [
  { id: "home", label: "Accueil", glyph: "⌂" },
  { id: "folders", label: "Dossiers", glyph: "☷" },
  { id: "activity", label: "Activité", glyph: "⚙" },
];

function queueActionLabel(jobType: string): string {
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
}

function openSettings() {
  invoke("show_setup_window").catch(() => {});
}

/** Compact Dropbox-style flyout: nav rail + status footer over three read-mostly
 * sections. No hydrate/download actions here — hydration happens by double-clicking
 * the `.cloudsc` placeholder in the OS file manager. */
function FlyoutApp() {
  const { activity, pushLog } = useActivityLog();
  const { startupLoading, authOk, syncFolderOk, syncFolder } = useStartupRequirements(pushLog);
  const { status, jobs, conflicts, retryFailedJobs } = useSyncDashboard(pushLog);
  const { cloudscEntries } = useCloudscPlaceholders({
    pushLog,
    authOk,
    syncFolderOk,
    syncFolder,
    syncRunning: status.syncRunning,
  });
  const { remoteCurrentPath, remoteEntries, remoteLoading, loadRemoteFolder } = useRemoteBrowser({
    pushLog,
    ready: authOk && syncFolderOk,
    syncRunning: status.syncRunning,
  });

  const [section, setSection] = useState<Section>("home");
  const schedulerStartedRef = useRef(false);
  const loggedReadyRef = useRef(false);

  useEffect(() => {
    if (loggedReadyRef.current) return;
    loggedReadyRef.current = true;
    pushLog("UI ready.");
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [authOk, syncFolderOk]);

  const footerLabel = startupLoading
    ? "Starting..."
    : !authOk || !syncFolderOk
      ? "Setup required"
      : status.lastError
        ? "Error"
        : status.syncRunning
          ? "Syncing..."
          : "Vos fichiers sont à jour";

  const footerClass = !authOk || !syncFolderOk || status.lastError ? "warn" : status.syncRunning ? "busy" : "ok";

  const recentJobs: SyncJob[] = jobs.slice(0, 8);
  const hasFailedJobs = jobs.some((job) => job.status === "failed");

  return (
    <div className="flyout">
      <nav className="flyout-nav">
        <div className="flyout-nav-items">
          {NAV_ITEMS.map((item) => (
            <button
              key={item.id}
              type="button"
              className={`flyout-nav-item${section === item.id ? " active" : ""}`}
              onClick={() => setSection(item.id)}
              title={item.label}
            >
              <span className="glyph">{item.glyph}</span>
              <span className="label">{item.label}</span>
            </button>
          ))}
        </div>
        <button type="button" className="flyout-settings" onClick={openSettings} title="Settings / Reconnect">
          <span className="glyph">{"⚙️"}</span>
        </button>
      </nav>

      <div className="flyout-body">
        {!authOk && (
          <div className="flyout-banner">
            <p>Dropbox is not connected.</p>
            <button type="button" className="flyout-btn" onClick={openSettings}>
              Connect Dropbox
            </button>
          </div>
        )}

        {section === "home" && (
          <section className="flyout-section">
            <h2>Recent transfers</h2>
            {hasFailedJobs && (
              <button type="button" className="flyout-btn flyout-retry" onClick={() => void retryFailedJobs()}>
                Retry failed jobs
              </button>
            )}
            <ul className="flyout-list">
              {recentJobs.length === 0 && <li className="muted">No recent activity.</li>}
              {recentJobs.map((job) => (
                <li key={job.id}>
                  <div className="row-main">
                    {queueActionLabel(job.jobType)} {job.targetPath ? `— ${job.targetPath}` : ""}
                  </div>
                  <div className={`row-sub ${job.status === "failed" ? "error" : ""}`}>
                    {job.status}
                    {job.lastError ? `: ${job.lastError}` : ""}
                  </div>
                </li>
              ))}
            </ul>

            <h2>Activity</h2>
            <ul className="flyout-list compact">
              {activity.length === 0 && <li className="muted">No entries yet.</li>}
              {activity.slice(0, 10).map((entry) => (
                <li key={entry.id}>{entry.message}</li>
              ))}
            </ul>
          </section>
        )}

        {section === "folders" && (
          <section className="flyout-section">
            <h2>Remote folder</h2>
            <div className="flyout-path-bar">
              <button
                type="button"
                className="flyout-btn"
                disabled={remoteLoading || !remoteCurrentPath}
                onClick={() => loadRemoteFolder(parentDropboxPath(remoteCurrentPath))}
              >
                Up
              </button>
              <span className="path">{remoteCurrentPath || "/"}</span>
            </div>
            <ul className="flyout-list">
              {remoteEntries.length === 0 && (
                <li className="muted">{remoteLoading ? "Loading..." : "No entries."}</li>
              )}
              {remoteEntries.map((entry) => {
                const statusLabel = entry.isExcluded ? "Excluded" : entry.isSynced ? "Synced" : "Not downloaded";
                const isFolder = entry.tag === "folder";
                return (
                  <li key={entry.pathDisplay}>
                    {isFolder ? (
                      <button
                        type="button"
                        className="row-link"
                        disabled={remoteLoading}
                        onClick={() => loadRemoteFolder(entry.pathDisplay)}
                      >
                        <span className="row-main">[DIR] {entry.pathDisplay}</span>
                      </button>
                    ) : (
                      <div className="row-main">[FILE] {entry.pathDisplay}</div>
                    )}
                    <div className="row-sub">{statusLabel}</div>
                  </li>
                );
              })}
            </ul>

            <h2>.cloudsc placeholders</h2>
            <ul className="flyout-list compact">
              {cloudscEntries.length === 0 && <li className="muted">None indexed yet.</li>}
              {cloudscEntries.map((entry) => (
                <li key={entry.localPathDisplay}>
                  <div className="row-main">
                    {entry.tag === "folder" ? "[DIR]" : "[FILE]"} {entry.localPathDisplay}
                  </div>
                  <div className="row-sub">{entry.remotePathDisplay}</div>
                </li>
              ))}
            </ul>
          </section>
        )}

        {section === "activity" && (
          <section className="flyout-section">
            <h2>Conflicts</h2>
            <ul className="flyout-list compact">
              {conflicts.length === 0 && <li className="muted">No conflicts.</li>}
              {conflicts.map((conflict) => (
                <li key={conflict.id}>
                  <div className="row-main">{conflict.localPath}</div>
                  <div className="row-sub error">{conflict.reason}</div>
                </li>
              ))}
            </ul>

            <h2>Activity log</h2>
            <ul className="flyout-list">
              {activity.length === 0 && <li className="muted">No entries yet.</li>}
              {activity.map((entry) => (
                <li key={entry.id}>{entry.message}</li>
              ))}
            </ul>
          </section>
        )}
      </div>

      <footer className={`flyout-footer ${footerClass}`}>
        <span className="dot" />
        {footerLabel}
      </footer>
    </div>
  );
}

export default FlyoutApp;
