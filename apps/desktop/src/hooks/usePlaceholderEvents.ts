import { useEffect } from "react";
import { listen } from "@tauri-apps/api/event";

type PlaceholderChanged = { created?: string[]; removed?: string[] };

/**
 * Listens for the Rust `placeholder-changed` event (emitted once per index
 * sweep, DBSYNC-45) and logs `.cloudsc` placeholder create/prune activity into
 * the flyout Activité feed via `pushLog`. Summarises when many changed at once
 * (a large initial index) so the log isn't flooded.
 */
export function usePlaceholderEvents(pushLog: (line: string) => void): void {
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    void listen<PlaceholderChanged>("placeholder-changed", (event) => {
      if (!active) return;
      const created = event.payload?.created ?? [];
      const removed = event.payload?.removed ?? [];
      const total = created.length + removed.length;
      if (total === 0) return;

      if (total <= 6) {
        created.forEach((name) => pushLog(`Placeholder ajouté : ${name}`));
        removed.forEach((name) => pushLog(`Placeholder retiré : ${name}`));
      } else {
        const parts: string[] = [];
        if (created.length) parts.push(`${created.length} ajouté(s)`);
        if (removed.length) parts.push(`${removed.length} retiré(s)`);
        pushLog(`Placeholders : ${parts.join(", ")}`);
      }
    }).then((fn) => {
      if (!active) fn();
      else unlisten = fn;
    });

    return () => {
      active = false;
      unlisten?.();
    };
  }, [pushLog]);
}
