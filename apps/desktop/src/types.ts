import type { SyncHealth } from "@dropbox-sync/shared";

export type StartupRequirements = {
  authOk: boolean;
  syncFolderOk: boolean;
  syncFolder?: string;
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
  reason: string;
  createdAt: string;
};

export type SyncDashboard = {
  status: SyncStatus;
  jobs: SyncJob[];
  conflicts: SyncConflict[];
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

export type TriggerActionResponse = {
  accepted: boolean;
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
