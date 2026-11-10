import { invoke } from "@tauri-apps/api/core";

/**
 * Asks the user to confirm an action in a native dialog. Use it instead of `window.confirm`, which the
 * macOS webview answers with `false` without showing anything.
 */
export async function confirmAction(title: string, message: string): Promise<boolean> {
  try {
    return await invoke<boolean>("confirm_dialog", { title, message });
  } catch {
    return false;
  }
}
