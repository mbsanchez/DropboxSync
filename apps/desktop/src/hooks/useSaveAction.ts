import { useCallback, useEffect, useRef, useState } from "react";

export type SaveStatus = { kind: "ok" | "error"; text: string };

const OK_CLEAR_MS = 3000;

/** Busy flag + inline result for a Settings Save button. `fn` must throw on failure.
 *  "Saved" clears itself after ~3 s; an error stays until `clearStatus` or the next run. */
export function useSaveAction() {
  const [saving, setSaving] = useState(false);
  const [status, setStatus] = useState<SaveStatus | null>(null);
  // Read and set synchronously: state would still be stale on a fast double-click.
  const inFlightRef = useRef(false);
  const timerRef = useRef<number | undefined>(undefined);

  const clearTimer = useCallback(() => {
    window.clearTimeout(timerRef.current);
    timerRef.current = undefined;
  }, []);

  useEffect(() => clearTimer, [clearTimer]);

  const clearStatus = useCallback(() => {
    clearTimer();
    setStatus(null);
  }, [clearTimer]);

  const run = useCallback(
    async (fn: () => Promise<void>) => {
      if (inFlightRef.current) return;
      inFlightRef.current = true;
      clearTimer();
      setStatus(null);
      setSaving(true);
      try {
        await fn();
        setStatus({ kind: "ok", text: "Saved" });
        timerRef.current = window.setTimeout(() => setStatus(null), OK_CLEAR_MS);
      } catch (e) {
        setStatus({ kind: "error", text: String(e) });
      } finally {
        setSaving(false);
        inFlightRef.current = false;
      }
    },
    [clearTimer],
  );

  return { saving, status, run, clearStatus };
}
