const BYTE_UNITS = ["B", "KB", "MB", "GB", "TB"];

/** Binary (1024-based) byte formatting, 1 decimal place ("1.2 MB", "640 KB"). */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), BYTE_UNITS.length - 1);
  const value = bytes / 1024 ** exponent;
  return `${exponent === 0 ? Math.round(value) : value.toFixed(1)} ${BYTE_UNITS[exponent]}`;
}

/** Throughput formatting built on top of `formatBytes` ("1.2 MB/s", "640 KB/s"). */
export function formatSpeed(bytesPerSec: number): string {
  if (!Number.isFinite(bytesPerSec) || bytesPerSec <= 0) return "0 B/s";
  return `${formatBytes(bytesPerSec)}/s`;
}
