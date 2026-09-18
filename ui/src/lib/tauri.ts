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
  sub_host?: string | null;
  sub_port?: number | null;
  sub_path?: string | null;
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

export type LiveState =
  | "starting"
  | "connecting"
  | "live"
  | "backoff"
  | "stopping"
  | "failed";

export type LiveFailureCategory =
  | "source_open_failed"
  | "unsupported_codec"
  | "worker_unavailable"
  | "media_failed"
  | "media_read_failed"
  | "media_fragment_create_failed"
  | "media_fragment_write_failed"
  | "media_packet_too_large"
  | "media_fragment_limit_exceeded"
  | "media_mux_write_failed"
  | "media_fragment_finalize_failed"
  | "media_fragment_capacity_failed"
  | "lifecycle_cancelled";

export interface LiveOpenDto {
  session_id: string;
  camera_id: string;
  url: string;
  state: LiveState;
}

export interface LiveStatus {
  session_id: string;
  camera_id: string;
  state: LiveState;
  failure_category: LiveFailureCategory | null;
  reconnect_attempt: number;
}

export type PtzDirection = "up" | "down" | "left" | "right" | "zoom_in" | "zoom_out";

export type PtzRuntimeState = "ready" | "moving" | "degraded";

export interface PtzCapabilities {
  camera_id: string;
  configured: boolean;
  ptz_supported: boolean;
  pan_tilt_supported: boolean;
  zoom_supported: boolean;
  state: PtzRuntimeState | null;
  error: string | null;
}

export interface PtzMovement {
  camera_id: string;
  generation: number;
  lease_ms: number;
  renew_after_ms: number;
}

export interface PtzMutation<T> {
  value: T;
  warning: "orphan_credential_cleanup_failed" | null;
}

export type EventRuntimeState =
  | "disabled"
  | "starting"
  | "subscribing"
  | "polling"
  | "backoff"
  | "stopping"
  | "failed";

export interface LocalMotionPreference {
  camera_id: string;
  enabled: boolean;
  // Optional during desktop/worker upgrades. Desired state is not runtime state.
  runtime_state?: string | null;
  motion_active?: boolean | null;
  sampled_frames?: number | null;
  last_error_code?: string | null;
}

export interface EventStatus {
  camera_id: string;
  configured: boolean;
  desired: boolean;
  state: EventRuntimeState;
  motion_active: boolean | null;
  last_event_at: string | null;
  last_error_code: string | null;
}

export type EventHistoryKind = "motion_started" | "motion_ended" | "person_started" | "person_ended";

export interface EventHistory {
  event_id: number;
  camera_id: string;
  kind: EventHistoryKind;
  device_time_utc: string | null;
  received_time_utc: string;
}

export interface EventReviewRow {
  event_id: number;
  camera_id: string;
  camera_display_name: string;
  kind: EventHistoryKind;
  device_time_utc: string | null;
  received_time_utc: string;
  recording_available: boolean;
}

export interface EventReviewPage {
  rows: EventReviewRow[];
  next_cursor: string | null;
}

export interface EventRecordingContext {
  available: boolean;
  camera_id: string;
  seek_offset_ms: number | null;
  clip_count: number;
}

export interface EventMutation<T> {
  value: T;
  warning: "orphan_credential_cleanup_failed" | null;
}

export interface ProbeResult {
  reachable: boolean;
  video_stream_found: boolean;
  codec: string | null;
  width: number | null;
  height: number | null;
  audio_stream_count: number;
}

export interface OnvifDiscoveredDevice {
  device_id: string;
  endpoint_reference: string;
  label: string;
  network_address: string;
}

export interface OnvifDiscovery {
  session_id: string;
  devices: OnvifDiscoveredDevice[];
}

export interface OnvifMediaProfile {
  token: string;
  name: string | null;
  video_codec: string | null;
  width: number | null;
  height: number | null;
  framerate: number | null;
  bitrate_kbps: number | null;
  audio_codec: string | null;
  supported: boolean;
  recommended: boolean;
}

export interface OnvifConnection {
  session_id: string;
  device_id: string;
  manufacturer: string | null;
  model: string | null;
  firmware_version: string | null;
  serial_number: string | null;
  hostname: string | null;
  profiles: OnvifMediaProfile[];
  proposed_camera_id: string;
  proposed_display_name: string;
}

export interface OnvifPreparedProfile {
  session_id: string;
  device_id: string;
  profile_token: string;
  host: string;
  port: number;
  path: string;
  host_mismatch: boolean;
  sub_profile_token: string | null;
  sub_host: string | null;
  sub_port: number | null;
  sub_path: string | null;
}

export interface ApplicationSettings {
  storage_root: string | null;
  segment_target_secs: number;
  max_age_days: number | null;
  max_storage_bytes: number | null;
  cleanup_target_bytes: number | null;
  launch_at_login: boolean;
}

export interface StorageUsage {
  configured: boolean;
  storage_root: string | null;
  manual_recording_bytes: number;
  event_clip_bytes: number;
  managed_bytes: number;
  manual_recording_count: number;
  event_clip_count: number;
  filesystem_total_bytes: number | null;
  filesystem_available_bytes: number | null;
  max_storage_bytes: number | null;
  cleanup_target_bytes: number | null;
}

export interface PerformanceSnapshot {
  sample_ready: boolean;
  process_count: number;
  app_cpu_percent: number;
  root_process_cpu_percent: number;
  system_cpu_percent: number;
  app_memory_bytes: number;
  root_process_memory_bytes: number;
  child_process_memory_bytes: number;
  system_memory_used_bytes: number;
  system_memory_total_bytes: number;
  system_network_rx_bps: number;
  system_network_tx_bps: number;
  app_network_rx_bps: number | null;
  app_network_tx_bps: number | null;
  app_gpu_percent: number | null;
  system_gpu_percent: number | null;
  processes: PerformanceProcess[];
}

export interface PerformanceProcess {
  pid: number;
  name: string;
  role: string;
  cpu_percent: number;
  memory_bytes: number;
  is_root: boolean;
}

export interface CameraMediaConsumerCounts {
  recording: number;
  live: number;
  motion: number;
  event: number;
  other: number;
}

export interface CameraMediaSourceDiagnostics {
  profile: string;
  lifecycle: string;
  retainers: number;
  subscribers: number;
  reliable_subscribers: number;
  realtime_subscribers: number;
  consumers: CameraMediaConsumerCounts;
  queued_packets: number;
  queued_bytes: number;
  dropped_packets: number;
  pre_roll_packets: number;
  pre_roll_bytes: number;
  pre_roll_dropped_packets: number;
  video_frame_rate: { num: number; den: number } | null;
  video_codec: string | null;
  video_width: number | null;
  video_height: number | null;
}

export interface CameraMediaDiagnostics {
  camera_id: string;
  worker_available: boolean;
  generation_starts: number;
  source_count: number;
  main_sources: number;
  sub_sources: number;
  connecting_sources: number;
  ready_sources: number;
  failed_sources: number;
  subscribers: number;
  reliable_subscribers: number;
  realtime_subscribers: number;
  consumers: CameraMediaConsumerCounts;
  queued_packets: number;
  queued_bytes: number;
  dropped_packets: number;
  pre_roll_packets: number;
  pre_roll_bytes: number;
  pre_roll_dropped_packets: number;
  sources: CameraMediaSourceDiagnostics[];
}

export interface RecordingIntent {
  camera_ids: string[];
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
  /** Opaque PCM WAV capability when G.711 audio is delivered outside MP4. */
  audio_url?: string | null;
  recording: RecordingDto;
  inspect: PlaybackInspectDto;
  adjacent: AdjacentRecordingsDto;
}

export interface EventPlaybackOpenDto {
  playback: PlaybackOpenDto;
  seek_offset_ms: number | null;
  clip_index: number;
  clip_count: number;
}

export interface NotificationSettings {
  motion_notifications_enabled: boolean;
  supported: boolean;
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
