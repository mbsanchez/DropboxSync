import { useEffect, useRef, useState, type ReactElement } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useActivityLog } from "../hooks/useActivityLog";
import { useStartupRequirements } from "../hooks/useStartupRequirements";
import { useSyncDashboard } from "../hooks/useSyncDashboard";
import { useTransferProgress } from "../hooks/useTransferProgress";
import { usePlaceholderEvents } from "../hooks/usePlaceholderEvents";
import { formatBytes, formatSpeed } from "../format";
import type { ActiveTransfer, ActivityEntry, SyncConflict, SyncJob } from "../types";
import "./FlyoutApp.css";

type Section = "home" | "folders" | "activity";

const NAV_ITEMS: { id: Section; label: string; glyph: string }[] = [
  { id: "home", label: "Accueil", glyph: "⌂" },
  { id: "folders", label: "Dossiers", glyph: "☷" },
  { id: "activity", label: "Activité", glyph: "⚙" },
];

type EventKind = "upload" | "download" | "delete" | "conflict" | "auth" | "error" | "sync";

function queueActionLabel(jobType: string): string {
  switch (jobType) {
    case "upload":
      return "Upload (local -> Dropbox)";
    case "download":
      return "Download (Dropbox -> local)";
    case "delete":
      return "Supprimé sur Dropbox";
    case "local_delete":
      return "Supprimé (retiré du remote)";
    case "hydrate_cloudsc":
      return "Hydrate (.cloudsc)";
    default:
      return jobType;
  }
}

/** Maps a `SyncJob.jobType` to a badge kind for the recent-transfers icon. */
function jobEventKind(job: SyncJob): EventKind {
  if (job.status === "failed") return "error";
  switch (job.jobType) {
    case "upload":
      return "upload";
    case "download":
    case "hydrate_cloudsc":
      return "download";
    case "delete":
    case "local_delete":
      return "delete";
    default:
      return "sync";
  }
}

/** Best-effort classification of a free-text activity-log line into an event
 * kind, so the Activité feed can show a meaningful icon without any structured
 * event data from the backend. */
function classifyActivityMessage(message: string): EventKind {
  const text = message.toLowerCase();
  if (text.includes("conflict")) return "conflict";
  if (text.includes("uploading") || text.includes("upload")) return "upload";
  if (text.includes("download") || text.includes("hydrat")) return "download";
  if (text.includes("requeue") || text.includes("retry")) return "sync";
  if (text.includes("failed") || text.includes("error")) return "error";
  if (text.includes("dropbox") || text.includes("auth") || text.includes("connect")) return "auth";
  return "sync";
}

function basename(path?: string): string {
  if (!path) return "";
  const cleaned = path.replace(/[\\/]+$/, "");
  const parts = cleaned.split(/[\\/]/);
  return parts[parts.length - 1] || cleaned;
}

/** Renders a compact "il y a Xmin" style relative time from an epoch-ms
 * number, an ISO/RFC3339 string, or a Date. */
function formatRelativeTime(input: number | string | Date | undefined): string {
  if (input === undefined) return "";
  const ts = input instanceof Date ? input.getTime() : typeof input === "number" ? input : new Date(input).getTime();
  if (Number.isNaN(ts)) return "";

  const diffSec = Math.round((Date.now() - ts) / 1000);
  if (diffSec < 5) return "à l'instant";
  if (diffSec < 60) return `il y a ${diffSec}s`;

  const diffMin = Math.round(diffSec / 60);
  if (diffMin < 60) return `il y a ${diffMin} min`;

  const diffH = Math.round(diffMin / 60);
  if (diffH < 24) return `il y a ${diffH} h`;

  const diffDay = Math.round(diffH / 24);
  if (diffDay === 1) return "hier";
  if (diffDay === 2) return "avant-hier";
  if (diffDay < 7) return `il y a ${diffDay} j`;

  const diffWeek = Math.round(diffDay / 7);
  if (diffWeek < 5) return `il y a ${diffWeek} sem`;

  const diffMonth = Math.round(diffDay / 30);
  if (diffMonth < 12) return `il y a ${diffMonth} mois`;

  const diffYear = Math.round(diffDay / 365);
  return `il y a ${diffYear} an${diffYear > 1 ? "s" : ""}`;
}

/** Small inline (CSP-safe, no icon-library) SVG glyphs for the event badges. */
const EVENT_ICON_PATHS: Record<EventKind, ReactElement> = {
  upload: (
    <path d="M12 19V5M12 5l-6 6M12 5l6 6" strokeLinecap="round" strokeLinejoin="round" />
  ),
  download: (
    <path d="M12 5v14M12 19l-6-6M12 19l6-6" strokeLinecap="round" strokeLinejoin="round" />
  ),
  delete: (
    <path
      d="M5 7h14M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2m2 0-1 12a1 1 0 0 1-1 1H8a1 1 0 0 1-1-1L6 7h12Z"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  ),
  conflict: (
    <path
      d="M12 4 2.5 20h19L12 4Zm0 6v4m0 3h.01"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  ),
  auth: (
    <path
      d="M9.5 12.5a3.5 3.5 0 1 1 3.11-1.89M12.6 10.6 20 3M17 6l2.5 2.5M14.5 8.5 17 11"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  ),
  error: <path d="M6 6l12 12M18 6 6 18" strokeLinecap="round" strokeLinejoin="round" />,
  sync: (
    <path
      d="M4 12a8 8 0 0 1 13.66-5.66M20 12a8 8 0 0 1-13.66 5.66M4 4v5h5M20 20v-5h-5"
      strokeLinecap="round"
      strokeLinejoin="round"
    />
  ),
};

function EventIcon({ kind }: { kind: EventKind }) {
  return (
    <span className={`event-icon ${kind}`} aria-hidden="true">
      <svg viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" strokeWidth="2">
        {EVENT_ICON_PATHS[kind]}
      </svg>
    </span>
  );
}

function openSettings() {
  invoke("show_setup_window").catch(() => {});
}

type CurrentTransferCardProps = { transfer: ActiveTransfer };

/** Compact "in-flight" card shown above the recent-transfers list while a
 * transfer is streaming. Falls back to an indeterminate/striped bar when the
 * total size is unknown (`total === 0`, e.g. a fresh resumable-upload session). */
function CurrentTransferCard({ transfer }: CurrentTransferCardProps) {
  const { path, transferred, total, direction, speedBytesPerSec } = transfer;
  const knownTotal = total > 0;
  const pct = knownTotal ? Math.min(100, Math.max(0, (transferred / total) * 100)) : 0;

  return (
    <div className="current-transfer-card">
      <div className="current-transfer-head">
        <EventIcon kind={direction} />
        <div className="current-transfer-name" title={path}>
          {basename(path)}
        </div>
      </div>
      <div className={`current-transfer-bar${knownTotal ? "" : " indeterminate"}`}>
        <div className="current-transfer-bar-fill" style={knownTotal ? { width: `${pct}%` } : undefined} />
      </div>
      <div className="current-transfer-meta">
        <span>
          {formatBytes(transferred)}
          {knownTotal ? ` / ${formatBytes(total)}` : ""}
        </span>
        <span>{formatSpeed(speedBytesPerSec)}</span>
      </div>
    </div>
  );
}

type RecentRowProps = { job: SyncJob };

function RecentTransferRow({ job }: RecentRowProps) {
  const kind = jobEventKind(job);
  const name = basename(job.targetPath) || queueActionLabel(job.jobType);
  return (
    <li className="recent-row">
      <EventIcon kind={kind} />
      <div className="recent-row-body">
        <div className="row-main">{name}</div>
        <div className={`row-sub ${job.status === "failed" ? "error" : ""}`}>
          {job.status === "failed" ? job.lastError ?? "Échec" : formatRelativeTime(job.updatedAt)}
        </div>
      </div>
    </li>
  );
}

type ActivityRowProps = { entry: ActivityEntry };

function ActivityRow({ entry }: ActivityRowProps) {
  const kind = classifyActivityMessage(entry.message);
  // pushLog() prefixes each line with a locale time string ("14:32:05 - ...");
  // strip it here since the row already renders a relative time separately.
  const text = entry.message.replace(/^\d{1,2}:\d{2}:\d{2}(?:\s?[AP]M)?\s-\s/, "");
  return (
    <li className="activity-row">
      <EventIcon kind={kind} />
      <div className="activity-row-body">
        <div className="row-main">{text}</div>
        <div className="row-sub">{formatRelativeTime(entry.timestamp)}</div>
      </div>
    </li>
  );
}

type JobActivityRowProps = { job: SyncJob };

/** Renders a background sync job (upload/download/delete/local_delete/hydrate) as
 * an Activité row, so events driven by the scheduler (not pushLog) are visible too. */
function JobActivityRow({ job }: JobActivityRowProps) {
  const kind = jobEventKind(job);
  const name = basename(job.targetPath);
  const main = name ? `${queueActionLabel(job.jobType)} — ${name}` : queueActionLabel(job.jobType);
  return (
    <li className="activity-row">
      <EventIcon kind={kind} />
      <div className="activity-row-body">
        <div className="row-main">{main}</div>
        <div className={`row-sub ${job.status === "failed" ? "error" : ""}`}>
          {job.status === "failed" ? job.lastError ?? "Échec" : `${job.status} · ${formatRelativeTime(job.updatedAt)}`}
        </div>
      </div>
    </li>
  );
}

type ActivityFeedItem =
  | { kind: "job"; key: string; time: number; job: SyncJob }
  | { kind: "log"; key: string; time: number; entry: ActivityEntry };

/** Merges background sync `jobs` with the pushLog `activity` entries into a single
 * time-sorted (descending) feed for the Activité section, so events that only exist
 * as jobs (e.g. a scheduler-driven `local_delete`) show up alongside logged lines. */
function buildActivityFeed(jobs: SyncJob[], activity: ActivityEntry[]): ActivityFeedItem[] {
  const jobItems: ActivityFeedItem[] = jobs
    .filter((job) => job.updatedAt !== undefined)
    .map((job) => ({
      kind: "job",
      key: `job-${job.id}`,
      time: new Date(job.updatedAt as string).getTime(),
      job,
    }));

  const logItems: ActivityFeedItem[] = activity.map((entry) => ({
    kind: "log",
    key: `log-${entry.id}`,
    time: entry.timestamp,
    entry,
  }));

  return [...jobItems, ...logItems]
    .filter((item) => !Number.isNaN(item.time))
    .sort((a, b) => b.time - a.time)
    .slice(0, 40);
}

/** Compact Dropbox-style flyout: nav rail + status footer over three read-mostly
 * sections. No hydrate/download actions here — hydration happens by double-clicking
 * the `.cloudsc` placeholder in the OS file manager. */
function FlyoutApp() {
  const { activity, pushLog } = useActivityLog();
  const { startupLoading, authOk, syncFolderOk, syncFolder } = useStartupRequirements(pushLog);
  const { status, jobs, conflicts, retryFailedJobs, resolveConflict } = useSyncDashboard(pushLog);
  const { activeTransfer } = useTransferProgress();
  usePlaceholderEvents(pushLog);

  // DBSYNC-35: apply a conflict decision. "Utiliser distant" on a remote-deleted
  // conflict discards the local file for good, so it gets a double-confirm.
  const onResolveConflict = (
    conflict: SyncConflict,
    action: "keep_local" | "use_remote" | "keep_both",
  ) => {
    if (action === "use_remote" && conflict.remoteDeleted) {
      const ok = window.confirm(
        `Le distant a été supprimé. « Utiliser distant » supprimera définitivement le fichier local « ${conflict.localPath} ». Continuer ?`,
      );
      if (!ok) return;
    }
    void resolveConflict(conflict.id, action);
  };

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

  const openLogs = async () => {
    try {
      await invoke("open_logs");
    } catch (e) {
      pushLog(`Failed to open logs: ${String(e)}`);
    }
  };

  const openSyncFolder = async () => {
    try {
      await invoke("open_sync_folder");
    } catch (e) {
      pushLog(`Failed to open sync folder: ${String(e)}`);
    }
  };

  // DBSYNC-32: "Sync Now" — kick off an immediate sync tick (non-blocking; the
  // backend CAS-guards against overlapping runs and refreshes the tray state).
  const syncNow = async () => {
    try {
      const accepted = await invoke<{ accepted: boolean }>("trigger_sync_tick");
      pushLog(accepted?.accepted ? "Sync requested." : "Sync already running.");
    } catch (e) {
      pushLog(`Failed to trigger sync: ${String(e)}`);
    }
  };

  const handleNavClick = (item: Section) => {
    setSection(item);
    if (item === "folders") {
      void openSyncFolder();
    }
  };

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
  const activityFeed = buildActivityFeed(jobs, activity);

  // "X of Y files": Y = jobs still pending or completed in the current run
  // (queued/running/retry_wait + done), X = the completed (done) count. Only
  // shown while a sync is actually running/pending, so a completed run's
  // "done" jobs don't linger in the indicator once the run is over.
  const pendingJobCount = jobs.filter(
    (job) => job.status === "queued" || job.status === "running" || job.status === "retry_wait",
  ).length;
  const doneJobCount = jobs.filter((job) => job.status === "done").length;
  const runTotal = pendingJobCount + doneJobCount;
  const runProgressLabel =
    (status.syncRunning || pendingJobCount > 0) && runTotal > 0 ? `${doneJobCount} sur ${runTotal} fichiers` : null;

  return (
    <div className="flyout">
      <nav className="flyout-nav">
        <div className="flyout-nav-items">
          {NAV_ITEMS.map((item) => (
            <button
              key={item.id}
              type="button"
              className={`flyout-nav-item${section === item.id ? " active" : ""}`}
              onClick={() => handleNavClick(item.id)}
              title={item.label}
            >
              <span className="glyph">{item.glyph}</span>
              <span className="label">{item.label}</span>
            </button>
          ))}
        </div>
        <button
          type="button"
          className="flyout-settings"
          onClick={() => void syncNow()}
          disabled={status.syncRunning}
          title="Synchroniser maintenant"
        >
          <span className="glyph">{"🔄"}</span>
        </button>
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
            {activeTransfer && <CurrentTransferCard transfer={activeTransfer} />}
            {runProgressLabel && <div className="current-transfer-run-label">{runProgressLabel}</div>}

            <button type="button" className="flyout-btn flyout-logs-btn" onClick={() => void openLogs()}>
              Ouvrir les journaux
            </button>

            <h2>Activité récente</h2>
            {hasFailedJobs && (
              <button type="button" className="flyout-btn flyout-retry" onClick={() => void retryFailedJobs()}>
                Retry failed jobs
              </button>
            )}
            <ul className="flyout-list recent-list">
              {recentJobs.length === 0 && <li className="muted">Aucune activité récente.</li>}
              {recentJobs.map((job) => (
                <RecentTransferRow key={job.id} job={job} />
              ))}
            </ul>
          </section>
        )}

        {section === "folders" && (
          <section className="flyout-section">
            <h2>Dossier synchronisé</h2>
            <div className="folder-panel">
              <span className="path">{syncFolder || "Non configuré"}</span>
              <button type="button" className="flyout-btn" onClick={() => void openSyncFolder()}>
                Ouvrir dans l'explorateur
              </button>
            </div>
          </section>
        )}

        {section === "activity" && (
          <section className="flyout-section">
            <h2>Conflicts</h2>
            <ul className="flyout-list compact">
              {conflicts.length === 0 && <li className="muted">No conflicts.</li>}
              {conflicts.map((conflict) => (
                <li key={conflict.id} className="activity-row conflict-row">
                  <EventIcon kind="conflict" />
                  <div className="activity-row-body">
                    <div className="row-main">{conflict.localPath}</div>
                    <div className="row-sub error">{conflict.reason}</div>
                    <div className="conflict-actions">
                      <button
                        type="button"
                        className="conflict-btn"
                        onClick={() => onResolveConflict(conflict, "keep_local")}
                      >
                        Garder local
                      </button>
                      <button
                        type="button"
                        className={`conflict-btn${conflict.remoteDeleted ? " danger" : ""}`}
                        onClick={() => onResolveConflict(conflict, "use_remote")}
                      >
                        {conflict.remoteDeleted ? "Supprimer local" : "Utiliser distant"}
                      </button>
                      {!conflict.remoteDeleted && (
                        <button
                          type="button"
                          className="conflict-btn"
                          onClick={() => onResolveConflict(conflict, "keep_both")}
                        >
                          Garder les deux
                        </button>
                      )}
                    </div>
                  </div>
                </li>
              ))}
            </ul>

            <h2>Activity log</h2>
            <ul className="flyout-list activity-list">
              {activityFeed.length === 0 && <li className="muted">No entries yet.</li>}
              {activityFeed.map((item) =>
                item.kind === "job" ? (
                  <JobActivityRow key={item.key} job={item.job} />
                ) : (
                  <ActivityRow key={item.key} entry={item.entry} />
                ),
              )}
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
