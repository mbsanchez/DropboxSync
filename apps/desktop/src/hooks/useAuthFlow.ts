import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";
import type { StartupRequirements } from "../types";

type UseAuthFlowParams = {
  pushLog: (line: string) => void;
  /** Current auth state from `useStartupRequirements`, so a refresh arriving via
   * window focus (not this hook's own listener/poll) also clears `awaitingCallback`. */
  authOk: boolean;
  setAuthOk: (v: boolean) => void;
  setSyncFolderOk: (v: boolean) => void;
  setSyncFolder: (v: string) => void;
  setStartupLoading: (v: boolean) => void;
};

/**
 * Dropbox OAuth start/cancel + completion detection (DBSYNC-22). Completion can
 * arrive via the primary Rust event OR the fallback poll, and both can fire for the
 * same login. `oauthCompletedRef` is a once-only guard: whichever path arrives first
 * runs the state update + log exactly once; the loser no-ops. Reset in `startOAuth`
 * so a fresh (re-)connect attempt can complete again. DO NOT alter this guard logic.
 */
export function useAuthFlow({
  pushLog,
  authOk,
  setAuthOk,
  setSyncFolderOk,
  setSyncFolder,
  setStartupLoading,
}: UseAuthFlowParams) {
  const [authUrl, setAuthUrl] = useState("");
  const [awaitingCallback, setAwaitingCallback] = useState(false);
  const [connectError, setConnectError] = useState<string | null>(null);

  // OAuth completion can arrive via the primary Rust event OR the fallback poll,
  // and both can fire for the same login. This once-only guard lets whichever
  // path arrives first run the state update + log exactly once; the loser no-ops.
  // Reset in startOAuth so a fresh (re-)connect attempt can complete again.
  const oauthCompletedRef = useRef(false);

  const startOAuth = async () => {
    setConnectError(null);
    // New login attempt: allow the completion handlers to fire once again.
    oauthCompletedRef.current = false;
    try {
      const payload = await invoke<{ authUrl: string; state: string }>("start_oauth_flow");
      setAuthUrl(payload.authUrl);
      setAwaitingCallback(true);
      await openUrl(payload.authUrl);
      pushLog("Opened Dropbox login. Waiting for callback on localhost.");
    } catch (error) {
      const msg = String(error);
      setConnectError(msg);
      pushLog(`Could not start OAuth: ${msg}`);
      setAwaitingCallback(false);
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

  // If auth becomes OK via some other refresh path (e.g. window focus), stop waiting.
  useEffect(() => {
    if (authOk) {
      setAwaitingCallback(false);
    }
  }, [authOk]);

  // Primary: Rust emits `dropbox-oauth-finished` after token exchange (works while WebView is throttled).
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    void listen<{ ok: boolean; message?: string }>("dropbox-oauth-finished", (event) => {
      if (!active) return;
      setAwaitingCallback(false);
      if (event.payload.ok) {
        if (oauthCompletedRef.current) return;
        oauthCompletedRef.current = true;
        setConnectError(null);
        setAuthOk(true);
        setStartupLoading(false);
        pushLog("Dropbox authentication completed and token session stored securely.");
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
        if (oauthCompletedRef.current) return;
        oauthCompletedRef.current = true;
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
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [awaitingCallback]);

  return { authUrl, awaitingCallback, connectError, startOAuth, cancelOAuth };
}
