export type SyncHealth = "idle" | "syncing" | "error";

export interface SyncStatus {
  health: SyncHealth;
  queueDepth: number;
  lastError?: string;
  trackedPath?: string;
}
