import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export interface AppInfo {
  name: string;
  version: string;
}

export interface AvailableUpdate {
  version: string;
  notes: string | null;
  date: string | null;
}

export interface UpdateCheck {
  configured: boolean;
  current_version: string;
  available: AvailableUpdate | null;
}

export type AudioPolicy = "copy_all" | "exclude";

export interface CameraSummary {
  camera_id: string;
  display_name: string;
  host: string;
  port: number;
  path: string;
  audio_policy: AudioPolicy;
}

export interface CameraCommandInput extends CameraSummary {
  username: string;
  password: string;
}

export interface CameraMutation<T> {
  value: T;
  warning: "orphan_credential_cleanup_failed" | null;
}

export type RecordingState =
  | "stopped"
  | "starting"
  | "recovering"
  | "connecting"
  | "recording"
  | "backoff"
  | "stopping"
  | "failed";

export interface RecordingStatus {
  state: RecordingState;
  camera_id: string | null;
  failure_category: string | null;
  reconnect_attempt: number;
  finalized_segments: number;
}

export interface ProbeResult {
  reachable: boolean;
  video_stream_found: boolean;
  codec: string | null;
  width: number | null;
  height: number | null;
  audio_stream_count: number;
}

export interface ApplicationSettings {
  storage_root: string | null;
  segment_target_secs: number;
  max_age_days: number | null;
  max_storage_bytes: number | null;
  cleanup_target_bytes: number | null;
  launch_at_login: boolean;
}

export interface RecordingIntent {
  camera_id: string | null;
}

export interface DesktopError {
  code: string;
  message: string;
}

export type TimelineRecordingKind = "normal" | "recovered";

export interface RecordingDto {
  recording_id: string;
  camera_id: string;
  kind: TimelineRecordingKind;
  started_at: string;
  sequence: number;
  size_bytes: number;
  media_duration_ms: number | null;
  end_at: string | null;
}

export interface AdjacentRecordingsDto {
  previous: RecordingDto | null;
  next: RecordingDto | null;
}

export interface PlaybackInspectDto {
  duration_ms: number | null;
  video_codec: string;
  width: number | null;
  height: number | null;
  audio_available: boolean;
  container_compatibility: string;
  seekable: boolean;
}

export interface PlaybackOpenDto {
  session_id: string;
  url: string;
  recording: RecordingDto;
  inspect: PlaybackInspectDto;
  adjacent: AdjacentRecordingsDto;
}

export const STOPPED_STATUS: RecordingStatus = {
  state: "stopped",
  camera_id: null,
  failure_category: null,
  reconnect_attempt: 0,
  finalized_segments: 0,
};

export function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

export async function invokeDesktop<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  return invoke<T>(command, args);
}

export function desktopError(error: unknown): DesktopError {
  if (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    "message" in error &&
    typeof (error as DesktopError).code === "string" &&
    typeof (error as DesktopError).message === "string"
  ) {
    return error as DesktopError;
  }
  return { code: "internal", message: "Desktop operation failed." };
}

/** Loads static host data but stays usable in a plain-browser UI preview. */
export function useTauriCommand<T>(command: string, fallback: T): T {
  const [value, setValue] = useState<T>(fallback);

  const load = useCallback(async () => {
    if (!isTauri()) return;
    try {
      setValue(await invokeDesktop<T>(command));
    } catch (error) {
      console.error(`command ${command} failed`, error);
    }
  }, [command]);

  useEffect(() => {
    void load();
  }, [load]);

  return value;
}
