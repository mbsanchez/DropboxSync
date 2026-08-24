import type { SyncHealth } from "@dropbox-sync/shared";

/**
 * Whether the user has switched the Finder Sync extension on (DBSYNC-86).
 *
 * Three states rather than a boolean on purpose: `notApplicable` means "not macOS, or we could
 * not tell", and must never be treated as `disabled` — a Windows user has no Finder extension
 * to configure and must not be warned about one.
 */
export type FinderExtensionState =
  | "enabled"
  | "disabled"
  /**
   * The app is running from a macOS App Translocation mount, so it cannot know the
   * extension's real state — it would be asking about a temporary copy of itself
   * (DBSYNC-88). Kept separate from "disabled", which would be a false alarm, and from
   * "notApplicable", which would say nothing while the user has a real, fixable problem.
   */
  | "translocated"
  | "notApplicable";

export type StartupRequirements = {
  authOk: boolean;
  syncFolderOk: boolean;
  syncFolder?: string;
  finderExtension: FinderExtensionState;
};

export type SyncStatus = {
  health: SyncHealth;
  queueDepth: number;
  trackedPath?: string;
  lastError?: string;
  lastScanAt?: string;
  processedJobs: number;
  conflictsDetected: number;
  syncRunning: boolean;
};

export type SyncJob = {
  id: number;
  jobType: string;
  targetPath?: string;
  status: string;
  attemptCount: number;
  nextRetryAt?: string;
  updatedAt?: string;
  lastError?: string;
};

export type SyncConflict = {
  id: number;
  localPath: string;
  remotePath: string;
  reason: string;
  /** Sibling copy preserving the local content, relative to the sync root.
   *  Absent for the remote-deleted scenario (only the local primary survives). */
  conflictedCopyPath?: string | null;
  /** True when the remote was deleted while local diverged — "Use Remote" then
   *  means discarding the local file (guarded by a confirm in the UI). */
  remoteDeleted: boolean;
  createdAt: string;
};

export type SyncDashboard = {
  status: SyncStatus;
  jobs: SyncJob[];
  conflicts: SyncConflict[];
  /** DBSYNC-64: true while the mass-deletion circuit breaker has paused sync
   *  (a blocked deletion batch is pending `confirm_pending_deletions`). */
  massDeletePaused: boolean;
};

export type RemoteEntry = {
  tag: "file" | "folder" | string;
  pathDisplay: string;
  size?: number;
  isSynced: boolean;
  isExcluded: boolean;
};

export type ListRemoteFolderResponse = {
  currentPath: string;
  entries: RemoteEntry[];
};

export type SelectiveSyncFilters = {
  includeCsv: string;
  excludeCsv: string;
};

/** User-defined local ignore glob patterns (device-local only, distinct from
 *  remote selective sync). See Rust `path_util::matches_ignore_globs`. */
export type IgnoreGlobs = {
  csv: string;
};

/** Mirror of Rust `models::TriggerActionResponse`, returned by
 *  `trigger_hydrate_cloudsc_placeholder`, `trigger_download_remote_file`, and
 *  `trigger_hydrate_remote_folder`. */
export type TriggerActionResponse = {
  accepted: boolean;
};

export type TriggerSyncReason = "started" | "already_running";

/** Mirror of Rust `models::TriggerSyncResponse`, returned by `trigger_sync_tick` only. */
export type TriggerSyncResponse = {
  accepted: boolean;
  reason: TriggerSyncReason;
};

export type CloudscPlaceholderInfo = {
  localPathDisplay: string; // path relative to sync folder, e.g. "Videos/a.mp4.cloudsc"
  tag: "file" | "folder" | string;
  remotePathDisplay: string;
};

export type ActivityEntry = {
  id: string;
  message: string;
  /** Epoch ms captured when the entry was logged, used for relative-time display. */
  timestamp: number;
};

export type TransferDirection = "upload" | "download";

/** Payload shared by the `upload-progress` / `download-progress` Tauri events. */
export type TransferProgressEvent = {
  path: string;
  transferred: number;
  total: number;
};

export type ActiveTransfer = {
  path: string;
  transferred: number;
  total: number;
  direction: TransferDirection;
  speedBytesPerSec: number;
};
