import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useActivityLog } from "../hooks/useActivityLog";
import { useStartupRequirements } from "../hooks/useStartupRequirements";
import { useAuthFlow } from "../hooks/useAuthFlow";
import { useSelectiveSync } from "../hooks/useSelectiveSync";
import { useIgnoreGlobs } from "../hooks/useIgnoreGlobs";
import { useDisconnect } from "../hooks/useDisconnect";
import "../App.css";

/**
 * Decorated onboarding/settings window. Connect Dropbox -> choose sync folder ->
 * (once both are OK) auto-closes itself so the app drops back to the tray flyout.
 * If opened later via the flyout's settings affordance while already fully
 * configured, it stays open as a small settings panel (reconnect / change folder /
 * selective sync) instead of instantly closing.
 */
function SetupApp() {
  const { pushLog } = useActivityLog();
  const {
    startupLoading,
    authOk,
    syncFolderOk,
    syncFolder,
    setSyncFolder,
    setAuthOk,
    setSyncFolderOk,
    setStartupLoading,
    refreshStartupRequirements,
  } = useStartupRequirements(pushLog);
  const { authUrl, awaitingCallback, connectError, startOAuth, cancelOAuth } = useAuthFlow({
    pushLog,
    authOk,
    setAuthOk,
    setSyncFolderOk,
    setSyncFolder,
    setStartupLoading,
  });
  const { includeCsv, setIncludeCsv, excludeCsv, setExcludeCsv, saveSelectiveSync } =
    useSelectiveSync(pushLog);
  const { ignoreGlobsCsv, setIgnoreGlobsCsv, saveIgnoreGlobs } = useIgnoreGlobs(pushLog);
  const { disconnecting, disconnect } = useDisconnect(pushLog, refreshStartupRequirements);

  const [showFolderSetup, setShowFolderSetup] = useState(false);

  useEffect(() => {
    if (!authOk) {
      setShowFolderSetup(false);
    }
  }, [authOk]);

  // Only auto-close if this window was opened while onboarding/re-auth was still
  // incomplete. If it was opened (e.g. via the flyout's settings gear) while already
  // fully configured, keep it open as a settings panel instead of closing instantly.
  const wasIncompleteRef = useRef(false);
  useEffect(() => {
    if (startupLoading) return;
    if (!authOk || !syncFolderOk) {
      wasIncompleteRef.current = true;
      return;
    }
    if (!wasIncompleteRef.current) return;
    const timer = window.setTimeout(() => {
      void getCurrentWindow().close();
    }, 900);
    return () => window.clearTimeout(timer);
  }, [startupLoading, authOk, syncFolderOk]);

  const handleStartOAuth = () => {
    setShowFolderSetup(false);
    void startOAuth();
  };

  const saveFolder = async () => {
    try {
      await invoke("set_sync_folder", { folder: syncFolder });
      pushLog(`Sync folder configured: ${syncFolder}`);
      await refreshStartupRequirements();
    } catch (e) {
      pushLog(`Failed to save sync folder: ${String(e)}`);
    }
  };

  const handleDisconnect = () => {
    const ok = window.confirm(
      "Disconnect Dropbox? This signs you out and clears local sync state. Your files on Dropbox are not affected.",
    );
    if (!ok) return;
    void disconnect();
  };

  const pickFolder = async () => {
    try {
      const selected = await invoke<string | null>("pick_sync_folder_dialog");
      if (selected) {
        setSyncFolder(selected);
      }
    } catch (e) {
      pushLog(`Folder picker failed: ${String(e)}`);
    }
  };

  return (
    <main className="container">
      <section className="card hero">
        <h1 className="title">Dropbox Sync Desktop</h1>
        <p className="subtitle">Smart placeholders, selective hydration, and background sync.</p>
      </section>

      {startupLoading && (
        <section className="card onboarding">
          <h2>Starting...</h2>
          <p>Checking Dropbox connection and local sync settings.</p>
        </section>
      )}

      {!startupLoading && !authOk && (
        <section className="card onboarding">
          <h2>Connect Dropbox</h2>
          <p>Sign in with Dropbox in your browser. Use Retry if the browser tab closed or something went wrong.</p>
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
            <button type="button" disabled={authOk} onClick={handleStartOAuth}>
              {awaitingCallback ? "Retry login" : "Start Dropbox login"}
            </button>
            {awaitingCallback && (
              <button type="button" onClick={cancelOAuth}>
                Cancel
              </button>
            )}
          </div>
          {awaitingCallback && <p>Waiting for Dropbox confirmation...</p>}
          {connectError && (
            <p role="alert" style={{ color: "#b00020", marginTop: 12, whiteSpace: "pre-wrap" }}>
              {connectError}
            </p>
          )}
        </section>
      )}

      {!startupLoading && authOk && !syncFolderOk && !showFolderSetup && (
        <section className="card onboarding">
          <h2>Dropbox connected</h2>
          <p>Your token was registered successfully. Continue to choose the local sync folder.</p>
          <button onClick={() => setShowFolderSetup(true)}>Next</button>
        </section>
      )}

      {!startupLoading && authOk && !syncFolderOk && showFolderSetup && (
        <section className="card onboarding">
          <h2>Choose Sync Folder</h2>
          <p>Select the local folder where placeholders and hydrated files will be stored.</p>
          <div style={{ display: "flex", gap: 8 }}>
            <input
              value={syncFolder}
              onChange={(e) => setSyncFolder(e.currentTarget.value)}
              placeholder="/Users/me/DropboxSync"
            />
            <button onClick={pickFolder}>...</button>
            <button onClick={saveFolder}>Save</button>
          </div>
        </section>
      )}

      {!startupLoading && authOk && syncFolderOk && (
        <section className="card onboarding">
          <h2>You're all set</h2>
          <p>Dropbox is connected and the sync folder is configured.</p>

          <h3>Dropbox account</h3>
          <div style={{ display: "flex", gap: 8, flexWrap: "wrap", alignItems: "center" }}>
            <button type="button" onClick={handleStartOAuth}>
              Reconnect Dropbox
            </button>
            {awaitingCallback && (
              <button type="button" onClick={cancelOAuth}>
                Cancel OAuth
              </button>
            )}
            <button type="button" disabled={disconnecting} onClick={handleDisconnect}>
              {disconnecting ? "Disconnecting..." : "Disconnect"}
            </button>
          </div>
          {authUrl && (
            <p>
              Authorization URL: <a href={authUrl}>{authUrl}</a>
            </p>
          )}
          {connectError && (
            <p role="alert" style={{ color: "#b00020", marginTop: 12, whiteSpace: "pre-wrap" }}>
              {connectError}
            </p>
          )}

          <h3>Sync folder</h3>
          <div style={{ display: "flex", gap: 8 }}>
            <input
              value={syncFolder}
              onChange={(e) => setSyncFolder(e.currentTarget.value)}
              placeholder="/Users/me/DropboxSync"
            />
            <button onClick={pickFolder}>...</button>
            <button onClick={saveFolder}>Save</button>
          </div>

          <h3>Selective sync</h3>
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
            <button onClick={() => void saveSelectiveSync()}>Save selective sync</button>
          </div>

          <h3>Ignored files</h3>
          <p style={{ fontSize: "0.85rem", color: "#64748b", marginTop: -4, marginBottom: 8 }}>
            Local-only filters — matching files are skipped on this device only (Dropbox and your
            other devices are unaffected). <code>Thumbs.db</code> and <code>.DS_Store</code> are
            already ignored by default.
          </p>
          <div
            style={{
              display: "grid",
              gap: 8,
              padding: 10,
              background: "#f8fafc",
              border: "1px dashed #c5d1e3",
              borderRadius: 10,
            }}
          >
            <input
              value={ignoreGlobsCsv}
              onChange={(e) => setIgnoreGlobsCsv(e.currentTarget.value)}
              placeholder="Ignore patterns (CSV), e.g. *.log,Notes/scratch.txt"
            />
            <button onClick={() => void saveIgnoreGlobs()}>Save ignored files</button>
          </div>
        </section>
      )}
    </main>
  );
}

export default SetupApp;
