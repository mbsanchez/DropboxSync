import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import type { ActiveTransfer, TransferDirection, TransferProgressEvent } from "../types";

/** Clear the active transfer if no further progress event arrives within this
 * window, so a stalled/finished transfer doesn't linger in the UI. */
const IDLE_CLEAR_MS = 1500;

type SpeedSample = { path: string; prevTransferred: number; prevTime: number };

/** Live sync progress derived from the Rust-emitted `upload-progress` /
 * `download-progress` events. Tracks a single active transfer (v1: no
 * multi-row concurrent progress) and its instantaneous speed, clearing once
 * the transfer completes or goes idle. */
export function useTransferProgress() {
  const [activeTransfer, setActiveTransfer] = useState<ActiveTransfer | null>(null);
  const speedSampleRef = useRef<SpeedSample | null>(null);
  const idleTimerRef = useRef<number | undefined>(undefined);

  useEffect(() => {
    let active = true;
    const unlistenFns: Array<() => void> = [];

    const clearIdleTimer = () => {
      if (idleTimerRef.current !== undefined) {
        window.clearTimeout(idleTimerRef.current);
        idleTimerRef.current = undefined;
      }
    };

    const clearActive = () => {
      clearIdleTimer();
      speedSampleRef.current = null;
      setActiveTransfer(null);
    };

    const scheduleIdleClear = () => {
      clearIdleTimer();
      idleTimerRef.current = window.setTimeout(clearActive, IDLE_CLEAR_MS);
    };

    const handleProgress = (direction: TransferDirection) => (event: { payload: TransferProgressEvent }) => {
      if (!active) return;
      const { path, transferred, total } = event.payload;

      const completed = total > 0 && transferred >= total;
      if (completed) {
        clearActive();
        return;
      }

      const now = Date.now();
      const prev = speedSampleRef.current;
      let speedBytesPerSec = 0;
      if (prev && prev.path === path) {
        const deltaSec = (now - prev.prevTime) / 1000;
        const deltaBytes = transferred - prev.prevTransferred;
        speedBytesPerSec = deltaSec > 0 ? Math.max(0, deltaBytes / deltaSec) : 0;
      }
      speedSampleRef.current = { path, prevTransferred: transferred, prevTime: now };

      setActiveTransfer({ path, transferred, total, direction, speedBytesPerSec });
      scheduleIdleClear();
    };

    void listen<TransferProgressEvent>("upload-progress", handleProgress("upload")).then((fn) => {
      if (!active) {
        fn();
      } else {
        unlistenFns.push(fn);
      }
    });

    void listen<TransferProgressEvent>("download-progress", handleProgress("download")).then((fn) => {
      if (!active) {
        fn();
      } else {
        unlistenFns.push(fn);
      }
    });

    return () => {
      active = false;
      clearIdleTimer();
      unlistenFns.forEach((fn) => fn());
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return { activeTransfer };
}
