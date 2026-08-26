import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

/** Mirrors the `AppInfo` struct returned by the `app_info` Tauri command. */
export interface AppInfo {
  name: string;
  version: string;
}

/** True when running inside the Tauri webview (vs a plain browser). */
export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/**
 * Invokes a Tauri command, or returns `fallback` when running in a plain
 * browser so the UI stays honest about what the host can provide.
 */
export function useTauriCommand<T>(command: string, fallback: T): T {
  const [value, setValue] = useState<T>(fallback);

  const load = useCallback(async () => {
    if (!isTauri()) {
      return;
    }
    try {
      setValue(await invoke<T>(command));
    } catch (error) {
      console.error(`command ${command} failed`, error);
    }
  }, [command]);

  useEffect(() => {
    void load();
  }, [load]);

  return value;
}
