import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { EmptyState } from "../components/EmptyState";
import { PasswordInput } from "../components/PasswordInput";
import { SelectControl } from "../components/SelectControl";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type {
  AudioPolicy,
  CameraCommandInput,
  CameraMutation,
  CameraSummary,
  DesktopError,
  EventMutation,
  EventStatus,
  OnvifConnection,
  OnvifDiscoveredDevice,
  OnvifDiscovery,
  OnvifMediaProfile,
  OnvifPreparedProfile,
  ProbeResult,
  PtzCapabilities,
  PtzMutation,
  RecordingState,
  RecordingIntent,
  RecordingStatus,
} from "../lib/tauri";

type CameraFormState = CameraCommandInput;
type OnvifStep = "idle" | "scanning" | "devices" | "credentials" | "profiles" | "ready";

const ACTIVE_STATES = new Set<RecordingState>([
  "starting",
  "recovering",
  "connecting",
  "recording",
  "backoff",
  "stopping",
]);

function generatedCameraId(): string {
  return `camera-${Date.now().toString(36)}`;
}

function blankCamera(): CameraFormState {
  return {
    camera_id: generatedCameraId(),
    display_name: "",
    host: "",
    port: 554,
    path: "/stream1",
    audio_policy: "copy_all",
    username: "",
    password: "",
  };
}

function editCamera(camera: CameraSummary): CameraFormState {
  return { ...camera, username: "", password: "" };
}

function validateCamera(form: CameraFormState, creating: boolean): string | null {
  if (!form.display_name.trim()) return "Display name is required.";
  if (form.display_name.length > 128) return "Display name must be at most 128 characters.";
  if (!form.host.trim()) return "Host or IP address is required.";
  if (!Number.isInteger(form.port) || form.port < 1 || form.port > 65535) return "RTSP port must be between 1 and 65535.";
  if (!form.path.startsWith("/") || /\s|@/.test(form.path)) return "RTSP path must start with / and contain no spaces or @.";
  if (form.path.length > 4096) return "RTSP path is too long.";
  if (form.username.length > 256) return "Username is too long.";
  if (form.password.length > 512) return "Password is too long.";
  if (creating && (!form.username.trim() || !form.password)) return "Username and password are required for a new camera.";
  if (form.password && !form.username.trim()) return "Username is required when replacing the password.";
  if (form.username.trim() && !form.password) return "Password is required when replacing the username.";
  return null;
}

function statusLabel(state: RecordingState): string {
  switch (state) {
    case "backoff": return "Reconnecting";
    case "starting": return "Starting";
    case "recovering": return "Recovering";
    case "connecting": return "Connecting";
    case "recording": return "Recording";
    case "stopping": return "Stopping";
    case "failed": return "Failed";
    case "stopped": return "Stopped";
  }
}

function recordingFailureLabel(category: string | null | undefined): string | null {
  switch (category) {
    case "source_open_failed": return "Recorder could not open the camera stream";
    case "source_read_failed": return "Camera stream disconnected";
    case "source_timed_out": return "Camera stream timed out";
    case "output_write_failed": return "Could not write recording output";
    case "storage_failed": return "Recording storage is unavailable";
    case "camera_in_use": return "Camera is already owned by another recorder";
    case "permanent_configuration": return "Recording configuration is not usable";
    default: return null;
  }
}

function ProbeSummary({ result }: { result: ProbeResult }) {
  return (
    <p className="success-message" role="status">
      Connected. {result.video_stream_found ? `Video ${result.codec ?? "unknown"}` : "No video stream"}
      {result.width && result.height ? ` · ${result.width}×${result.height}` : ""}
      {` · ${result.audio_stream_count} audio stream${result.audio_stream_count === 1 ? "" : "s"}`}
    </p>
  );
}

function profileSummary(profile: OnvifMediaProfile): string {
  const resolution = profile.width && profile.height ? `${profile.width}×${profile.height}` : "resolution unknown";
  const fps = profile.framerate ? ` · ${profile.framerate} fps` : "";
  const bitrate = profile.bitrate_kbps ? ` · ${profile.bitrate_kbps} kbps` : "";
  return `${profile.video_codec ?? "codec unknown"} · ${resolution}${fps}${bitrate}`;
}

export function CamerasScreen() {
  const [cameras, setCameras] = useState<CameraSummary[]>([]);
  const [recordings, setRecordings] = useState<RecordingStatus[]>([]);
  const [intent, setIntent] = useState<RecordingIntent>({ camera_ids: [] });
  const [loading, setLoading] = useState(isTauri());
  const [error, setError] = useState<DesktopError | null>(null);
  const [form, setForm] = useState<CameraFormState | null>(null);
  const [creating, setCreating] = useState(false);
  const [showAddChoice, setShowAddChoice] = useState(false);
  const [saving, setSaving] = useState(false);
  const [probing, setProbing] = useState(false);
  const [probeResult, setProbeResult] = useState<ProbeResult | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<CameraSummary | null>(null);
  const [busyCamera, setBusyCamera] = useState<string | null>(null);
  const [ptzConfigured, setPtzConfigured] = useState<Map<string, boolean>>(() => new Map());
  const [eventStatuses, setEventStatuses] = useState<Map<string, EventStatus>>(() => new Map());
  const [ptzTarget, setPtzTarget] = useState<CameraSummary | null>(null);
  const [eventTarget, setEventTarget] = useState<CameraSummary | null>(null);
  const recordingBusyRef = useRef<Set<string>>(new Set());
  const [recordingBusyCameras, setRecordingBusyCameras] = useState<Set<string>>(() => new Set());

  const [onvifStep, setOnvifStep] = useState<OnvifStep>("idle");
  const [onvifBusy, setOnvifBusy] = useState(false);
  const [onvifSessionId, setOnvifSessionId] = useState<string | null>(null);
  const [onvifDevices, setOnvifDevices] = useState<OnvifDiscoveredDevice[]>([]);
  const [onvifDevice, setOnvifDevice] = useState<OnvifDiscoveredDevice | null>(null);
  const [onvifUsername, setOnvifUsername] = useState("");
  const [onvifPassword, setOnvifPassword] = useState("");
  const [onvifConnection, setOnvifConnection] = useState<OnvifConnection | null>(null);
  const [onvifProfileToken, setOnvifProfileToken] = useState<string | null>(null);
  const [onvifPrepared, setOnvifPrepared] = useState<OnvifPreparedProfile | null>(null);
  const [onvifCameraId, setOnvifCameraId] = useState("");
  const [onvifDisplayName, setOnvifDisplayName] = useState("");
  const [onvifAudioPolicy, setOnvifAudioPolicy] = useState<AudioPolicy>("exclude");

  const loadCameras = useCallback(async () => {
    if (!isTauri()) {
      setLoading(false);
      return;
    }
    try {
      const [rows, eventRows] = await Promise.all([
        invokeDesktop<CameraSummary[]>("camera_list"),
        invokeDesktop<EventStatus[]>("event_statuses").catch(() => []),
      ]);
      setCameras(rows);
      const configured = await Promise.all(
        rows.map(async (camera) => {
          const ptz = await invokeDesktop<boolean>("ptz_configured", { cameraId: camera.camera_id }).catch(() => false);
          return [camera.camera_id, ptz] as const;
        }),
      );
      setPtzConfigured(new Map(configured));
      setEventStatuses(new Map(eventRows.map((status) => [status.camera_id, status])));
      setError(null);
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  const refreshStatus = useCallback(async () => {
    if (!isTauri()) return;
    try {
      const [runtime, desired] = await Promise.all([
        invokeDesktop<RecordingStatus[]>("recording_statuses"),
        invokeDesktop<RecordingIntent>("recording_intent"),
      ]);
      setRecordings(runtime);
      setIntent(desired);
    } catch (cause) {
      setError(desktopError(cause));
    }
  }, []);

  useEffect(() => {
    void loadCameras();
    void refreshStatus();
    if (!isTauri()) return;
    const timer = window.setInterval(() => void refreshStatus(), 1_000);
    return () => window.clearInterval(timer);
  }, [loadCameras, refreshStatus]);

  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void getCurrentWindow()
      .onCloseRequested(() => {
        resetOnvifLocal();
        setError(null);
      })
      .then((cleanup) => {
        if (disposed) cleanup();
        else unlisten = cleanup;
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    return () => {
      if (isTauri()) void invokeDesktop<void>("onvif_cancel").catch(() => undefined);
    };
  }, []);

  const statusByCamera = useMemo(
    () => new Map(recordings.filter((status) => status.camera_id).map((status) => [status.camera_id as string, status])),
    [recordings],
  );
  const formStatus = form ? statusByCamera.get(form.camera_id) : undefined;
  const formCameraActive = formStatus ? ACTIVE_STATES.has(formStatus.state) : false;
  const criticalFieldsDisabled = formCameraActive;
  const activeCount = recordings.filter((status) => ACTIVE_STATES.has(status.state)).length;
  const reconnectingCount = recordings.filter((status) => status.state === "backoff").length;
  const sortedCameras = useMemo(
    () => [...cameras].sort((a, b) => a.display_name.localeCompare(b.display_name)),
    [cameras],
  );
  const selectedOnvifProfile = onvifConnection?.profiles.find((profile) => profile.token === onvifProfileToken) ?? null;
  const compatibleOnvifAudio = selectedOnvifProfile?.audio_codec?.toLowerCase() === "aac";

  function resetOnvifLocal() {
    setOnvifStep("idle");
    setOnvifBusy(false);
    setOnvifSessionId(null);
    setOnvifDevices([]);
    setOnvifDevice(null);
    setOnvifUsername("");
    setOnvifPassword("");
    setOnvifConnection(null);
    setOnvifProfileToken(null);
    setOnvifPrepared(null);
    setOnvifCameraId("");
    setOnvifDisplayName("");
    setOnvifAudioPolicy("exclude");
    setPtzTarget(null);
    setEventTarget(null);
  }

  async function closeOnvif() {
    const sessionId = onvifSessionId;
    resetOnvifLocal();
    setError(null);
    if (!isTauri()) return;
    try {
      await invokeDesktop<void>("onvif_cancel", sessionId ? { sessionId } : undefined);
    } catch {
      // Closing is best-effort; lifecycle cleanup is authoritative in Rust.
    }
  }

  function openCreate() {
    setShowAddChoice(true);
    setForm(null);
    setProbeResult(null);
    setError(null);
  }

  function openManualCreate() {
    setShowAddChoice(false);
    if (onvifStep !== "idle") void closeOnvif();
    else resetOnvifLocal();
    setCreating(true);
    setForm(blankCamera());
    setProbeResult(null);
    setError(null);
  }

  function openEdit(camera: CameraSummary) {
    setShowAddChoice(false);
    if (onvifStep !== "idle") void closeOnvif();
    else resetOnvifLocal();
    setCreating(false);
    setForm(editCamera(camera));
    setProbeResult(null);
    setError(null);
  }

  async function startOnvifDiscovery(
    target?: CameraSummary | null,
    purpose: "camera" | "ptz" | "events" = target ? "ptz" : "camera",
  ) {
    if (!isTauri() || onvifBusy) return;
    if (target !== undefined) {
      setPtzTarget(purpose === "ptz" ? target : null);
      setEventTarget(purpose === "events" ? target : null);
    }
    setShowAddChoice(false);
    setForm(null);
    setError(null);
    setOnvifBusy(true);
    setOnvifStep("scanning");
    setOnvifDevices([]);
    setOnvifDevice(null);
    setOnvifConnection(null);
    setOnvifPrepared(null);
    try {
      const discovery = await invokeDesktop<OnvifDiscovery>("onvif_discover");
      setOnvifSessionId(discovery.session_id);
      setOnvifDevices(discovery.devices);
      setOnvifStep("devices");
    } catch (cause) {
      setError(desktopError(cause));
      setOnvifStep("devices");
    } finally {
      setOnvifBusy(false);
    }
  }

  function chooseOnvifDevice(device: OnvifDiscoveredDevice) {
    setOnvifDevice(device);
    setOnvifUsername("");
    setOnvifPassword("");
    setOnvifConnection(null);
    setOnvifProfileToken(null);
    setOnvifPrepared(null);
    setError(null);
    setOnvifStep("credentials");
  }

  async function connectOnvifDevice(event: FormEvent) {
    event.preventDefault();
    if (!onvifSessionId || !onvifDevice || onvifBusy) return;
    if (!onvifUsername.trim() || !onvifPassword) {
      setError({ code: "validation", message: "Username and password are required." });
      return;
    }
    const username = onvifUsername;
    const password = onvifPassword;
    setOnvifPassword("");
    setOnvifBusy(true);
    setError(null);
    try {
      if (eventTarget) {
        await invokeDesktop<void>("onvif_connect_events", {
          input: {
            session_id: onvifSessionId,
            device_id: onvifDevice.device_id,
            username,
            password,
          },
        });
        const paired = await invokeDesktop<EventMutation<EventStatus>>("event_pair", {
          input: {
            camera_id: eventTarget.camera_id,
            session_id: onvifSessionId,
            device_id: onvifDevice.device_id,
          },
        });
        const warning = paired.warning;
        resetOnvifLocal();
        await loadCameras();
        if (warning) {
          setError({ code: "credential_store", message: "Motion Events were paired, but an obsolete Events credential could not be removed." });
        }
        return;
      }
      const connection = await invokeDesktop<OnvifConnection>("onvif_connect", {
        input: {
          session_id: onvifSessionId,
          device_id: onvifDevice.device_id,
          username,
          password,
        },
      });
      if (ptzTarget) {
        const paired = await invokeDesktop<PtzMutation<PtzCapabilities>>("ptz_pair", {
          input: {
            camera_id: ptzTarget.camera_id,
            session_id: onvifSessionId,
            device_id: onvifDevice.device_id,
          },
        });
        const warning = paired.warning;
        resetOnvifLocal();
        await loadCameras();
        if (warning) {
          setError({ code: "credential_store", message: "PTZ was paired, but an obsolete PTZ credential could not be removed." });
        }
        return;
      }
      setOnvifConnection(connection);
      const supported = connection.profiles.filter((profile) => profile.supported);
      setOnvifProfileToken(supported.length === 1 ? (supported[0]?.token ?? null) : null);
      setOnvifPrepared(null);
      setOnvifCameraId(connection.proposed_camera_id);
      setOnvifDisplayName(connection.proposed_display_name);
      setOnvifStep("profiles");
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setOnvifBusy(false);
    }
  }

  async function prepareOnvifProfile(profile: OnvifMediaProfile) {
    if (!onvifSessionId || !onvifDevice || onvifBusy || !profile.supported) return;
    setOnvifBusy(true);
    setError(null);
    setOnvifProfileToken(profile.token);
    setOnvifPrepared(null);
    try {
      const prepared = await invokeDesktop<OnvifPreparedProfile>("onvif_prepare_profile", {
        input: {
          session_id: onvifSessionId,
          device_id: onvifDevice.device_id,
          profile_token: profile.token,
        },
      });
      setOnvifPrepared(prepared);
      setOnvifAudioPolicy(profile.audio_codec?.toLowerCase() === "aac" ? "copy_all" : "exclude");
      setOnvifStep("ready");
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setOnvifBusy(false);
    }
  }

  async function addOnvifCamera() {
    if (!onvifSessionId || !onvifDevice || !onvifProfileToken || !onvifPrepared || onvifBusy) return;
    if (!onvifCameraId.trim() || !onvifDisplayName.trim()) {
      setError({ code: "validation", message: "Camera ID and display name are required." });
      return;
    }
    setOnvifBusy(true);
    setError(null);
    try {
      await invokeDesktop<CameraMutation<CameraSummary>>("onvif_add_camera", {
        input: {
          session_id: onvifSessionId,
          device_id: onvifDevice.device_id,
          profile_token: onvifProfileToken,
          camera_id: onvifCameraId,
          display_name: onvifDisplayName,
          audio_policy: onvifAudioPolicy,
        },
      });
      resetOnvifLocal();
      await loadCameras();
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setOnvifBusy(false);
    }
  }

  function patchForm<K extends keyof CameraFormState>(key: K, value: CameraFormState[K]) {
    setForm((current) => (current ? { ...current, [key]: value } : current));
    setProbeResult(null);
  }

  async function pairPtz(camera: CameraSummary) {
    if (!isTauri() || busyCamera || onvifBusy) return;
    setBusyCamera(camera.camera_id);
    setError(null);
    try {
      const result = await invokeDesktop<PtzMutation<PtzCapabilities>>("ptz_pair_saved", {
        cameraId: camera.camera_id,
      });
      await loadCameras();
      if (result.warning) {
        setError({
          code: "credential_store",
          message: "PTZ was paired, but an obsolete PTZ credential could not be removed.",
        });
      }
    } catch (cause) {
      const failure = desktopError(cause);
      if (failure.code === "onvif_auth_failed") {
        setBusyCamera(null);
        await startOnvifDiscovery(camera, "ptz");
        return;
      }
      setError(failure);
    } finally {
      setBusyCamera(null);
    }
  }

  async function unpairPtz(camera: CameraSummary) {
    if (!isTauri() || busyCamera) return;
    setBusyCamera(camera.camera_id);
    setError(null);
    try {
      const result = await invokeDesktop<PtzMutation<PtzCapabilities>>("ptz_unpair", { cameraId: camera.camera_id });
      await loadCameras();
      if (result.warning) {
        setError({ code: "credential_store", message: "PTZ was unpaired, but its obsolete credential could not be removed." });
      }
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setBusyCamera(null);
    }
  }

  async function toggleEvents(camera: CameraSummary) {
    if (!isTauri() || busyCamera) return;
    const current = eventStatuses.get(camera.camera_id);
    if (!current?.configured) return;
    setBusyCamera(camera.camera_id);
    setError(null);
    try {
      const command = current.desired ? "event_disable" : "event_enable";
      const next = await invokeDesktop<EventStatus>(command, { cameraId: camera.camera_id });
      setEventStatuses((statuses) => new Map(statuses).set(camera.camera_id, next));
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setBusyCamera(null);
    }
  }

  async function pairEvents(camera: CameraSummary) {
    if (!isTauri() || busyCamera || onvifBusy) return;
    setBusyCamera(camera.camera_id);
    setError(null);
    try {
      const result = await invokeDesktop<EventMutation<EventStatus>>("event_pair_saved", {
        cameraId: camera.camera_id,
      });
      await loadCameras();
      if (result.warning) {
        setError({
          code: "credential_store",
          message: "Motion Events were paired, but an obsolete Events credential could not be removed.",
        });
      }
    } catch (cause) {
      const failure = desktopError(cause);
      if (failure.code === "onvif_auth_failed") {
        setBusyCamera(null);
        await startOnvifDiscovery(camera, "events");
        return;
      }
      setError(failure);
    } finally {
      setBusyCamera(null);
    }
  }

  async function unpairEvents(camera: CameraSummary) {
    if (!isTauri() || busyCamera) return;
    setBusyCamera(camera.camera_id);
    setError(null);
    try {
      const result = await invokeDesktop<EventMutation<EventStatus>>("event_unpair", { cameraId: camera.camera_id });
      setEventStatuses((statuses) => new Map(statuses).set(camera.camera_id, result.value));
      if (result.warning) {
        setError({ code: "credential_store", message: "Motion Events were unpaired, but their obsolete credential could not be removed." });
      }
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setBusyCamera(null);
    }
  }

  async function submitCamera(event: FormEvent) {
    event.preventDefault();
    if (!form || saving) return;
    const validation = validateCamera(form, creating);
    if (validation) {
      setError({ code: "validation", message: validation });
      return;
    }
    if (!isTauri()) {
      setError({ code: "internal", message: "Camera changes require the desktop host." });
      return;
    }
    setSaving(true);
    setError(null);
    try {
      const command = creating ? "camera_create" : "camera_update";
      const result = await invokeDesktop<CameraMutation<CameraSummary>>(command, { input: form });
      if (result.warning === "orphan_credential_cleanup_failed") {
        setError({ code: "credential_store", message: "Camera saved, but an obsolete credential could not be cleaned up." });
      }
      setForm(null);
      setProbeResult(null);
      await loadCameras();
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setSaving(false);
    }
  }

  async function testConnection() {
    if (!form || probing) return;
    const validation = validateCamera(form, creating);
    if (validation) {
      setError({ code: "validation", message: validation });
      return;
    }
    if (!isTauri()) {
      setError({ code: "internal", message: "Connection tests require the desktop host." });
      return;
    }
    setProbing(true);
    setProbeResult(null);
    setError(null);
    try {
      const result = await invokeDesktop<ProbeResult>("camera_probe", {
        input: { camera: form, timeout_ms: 10_000 },
      });
      setProbeResult(result);
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setProbing(false);
    }
  }

  async function toggleRecording(camera: CameraSummary) {
    if (recordingBusyRef.current.has(camera.camera_id)) return;
    const desiredOn = intent.camera_ids.includes(camera.camera_id);
    const runtime = statusByCamera.get(camera.camera_id);
    if (!desiredOn && runtime && ACTIVE_STATES.has(runtime.state)) return;
    recordingBusyRef.current.add(camera.camera_id);
    setRecordingBusyCameras(new Set(recordingBusyRef.current));
    setError(null);
    try {
      if (desiredOn) {
        await invokeDesktop<RecordingStatus>("recording_stop", { cameraId: camera.camera_id });
      } else {
        await invokeDesktop<RecordingStatus>("recording_start", { cameraId: camera.camera_id });
      }
      await refreshStatus();
    } catch (cause) {
      setError(desktopError(cause));
      await refreshStatus();
    } finally {
      recordingBusyRef.current.delete(camera.camera_id);
      setRecordingBusyCameras(new Set(recordingBusyRef.current));
    }
  }

  async function confirmDelete() {
    if (!deleteTarget || busyCamera) return;
    const target = deleteTarget;
    setBusyCamera(target.camera_id);
    setError(null);
    try {
      const result = await invokeDesktop<CameraMutation<CameraSummary>>("camera_delete", {
        cameraId: target.camera_id,
      });
      setDeleteTarget(null);
      if (result.warning === "orphan_credential_cleanup_failed") {
        setError({ code: "credential_store", message: "Camera deleted, but its obsolete credential could not be cleaned up." });
      }
      await loadCameras();
    } catch (cause) {
      setError(desktopError(cause));
    } finally {
      setBusyCamera(null);
    }
  }

  const desiredRecordingCount = sortedCameras.filter((camera) => intent.camera_ids.includes(camera.camera_id)).length;
  const eventMonitoringCount = sortedCameras.filter((camera) => eventStatuses.get(camera.camera_id)?.desired).length;
  const configuredCount = sortedCameras.length;

  return (
    <section className="screen-stack" aria-label="Camera management">
      <div className="screen-toolbar">
        <div>
          <h2>Cameras</h2>
          <p className="muted">Saved RTSP cameras. ONVIF can provision streams and independently pair PTZ or motion-event monitoring.</p>
        </div>
        <button className="primary-button" onClick={openCreate} disabled={saving || onvifBusy}>Add camera</button>
      </div>

      <div className="camera-summary" aria-label="Camera overview">
        <div className="summary-stat">
          <span className="summary-stat-label">Configured</span>
          <strong>{configuredCount}</strong>
          <small>camera{configuredCount === 1 ? "" : "s"}</small>
        </div>
        <div className="summary-stat">
          <span className="summary-stat-label">Runtime active</span>
          <strong>{activeCount}</strong>
          <small>{reconnectingCount ? `${reconnectingCount} reconnecting` : `Active ${activeCount} camera${activeCount === 1 ? "" : "s"}`}</small>
        </div>
        <div className="summary-stat">
          <span className="summary-stat-label">Manual recording</span>
          <strong>{desiredRecordingCount}</strong>
          <small>desired on</small>
        </div>
        <div className="summary-stat">
          <span className="summary-stat-label">Motion monitoring</span>
          <strong>{eventMonitoringCount}</strong>
          <small>enabled</small>
        </div>
      </div>

      {error && <div className="error-banner" role="alert"><strong>{error.code}</strong>: {error.message}</div>}

      {loading ? (
        <p className="muted" role="status">Loading cameras…</p>
      ) : sortedCameras.length === 0 ? (
        <EmptyState title="No cameras configured" hint="Add an RTSP camera manually or discover a local ONVIF camera." />
      ) : (
        <div className="camera-grid">
          {sortedCameras.map((camera) => {
            const runtime = statusByCamera.get(camera.camera_id);
            const state = runtime?.state ?? "stopped";
            const ownActive = runtime ? ACTIVE_STATES.has(runtime.state) : false;
            const desiredOn = intent.camera_ids.includes(camera.camera_id);
            const desiredOffRuntimeActive = !desiredOn && ownActive;
            const rowBusy = recordingBusyCameras.has(camera.camera_id);
            const events = eventStatuses.get(camera.camera_id);
            const failureLabel = recordingFailureLabel(runtime?.failure_category);
            const recordingControlDisabled = busyCamera !== null || rowBusy || state === "stopping" || desiredOffRuntimeActive;
            const recordingControlLabel = desiredOn
              ? (state === "stopping" ? "Stopping…" : "Stop")
              : (desiredOffRuntimeActive ? "Stopping…" : "Start");
            return (
              <article className="camera-card" key={camera.camera_id}>
                <div className="camera-card-head">
                  <div>
                    <h3>{camera.display_name}</h3>
                    <code>{camera.host}:{camera.port}{camera.path}</code>
                  </div>
                  <span className={`chip chip-${state}`}>{statusLabel(state)}</span>
                </div>
                <div className="camera-status-grid">
                  <div className="camera-status-item">
                    <span className="camera-status-label">Recording</span>
                    <strong>{desiredOn ? "Manual on" : "Manual off"}</strong>
                    <small>Runtime {statusLabel(state)}</small>
                  </div>
                  <div className={`camera-status-item ${events?.motion_active === true ? "is-motion-active" : ""}`}>
                    <span className="camera-status-label">Motion monitoring</span>
                    <strong>Motion Events: {events?.configured ? (events.desired ? "On" : "Off") : "Unpaired"}</strong>
                    <small>
                      {events?.configured ? events.state.replaceAll("_", " ") : "Pair ONVIF events"}
                      {events?.motion_active === true ? " · Motion detected" : ""}
                      {events?.last_error_code ? ` · ${events.last_error_code}` : ""}
                    </small>
                  </div>
                  <div className="camera-status-item">
                    <span className="camera-status-label">Stream health</span>
                    <strong>{state === "backoff" ? "Reconnecting" : state === "failed" ? "Recording failed" : ownActive ? "Healthy" : "Idle"}</strong>
                    <small>
                      {runtime
                        ? state === "backoff"
                          ? `${failureLabel ?? "Camera stream unavailable"} · retry ${runtime.reconnect_attempt} · ${runtime.finalized_segments} segments`
                          : state === "failed"
                            ? `${failureLabel ?? "Recorder stopped because of an error"} · ${runtime.finalized_segments} segments`
                            : `${runtime.finalized_segments} segments · retry ${runtime.reconnect_attempt}`
                        : "No active recorder"}
                    </small>
                  </div>
                </div>
                <div className="camera-actions">
                  <div className="camera-actions-main">
                    <div className="camera-action-block">
                      <span className="action-group-label">Recording</span>
                      <button
                        className={desiredOn || desiredOffRuntimeActive ? "danger-button" : "primary-button"}
                        disabled={recordingControlDisabled}
                        onClick={() => void toggleRecording(camera)}
                      >
                        {recordingControlLabel}
                      </button>
                    </div>
                    <div className="camera-action-block">
                      <span className="action-group-label">Camera</span>
                      <button className="secondary-action-button" onClick={() => openEdit(camera)} disabled={busyCamera !== null || rowBusy}>Edit settings</button>
                    </div>
                  </div>

                  <div className="camera-integration-groups" aria-label={`${camera.display_name} integrations`}>
                    <div className="integration-action-group">
                      <span className="integration-label"><strong>PTZ</strong><small>{ptzConfigured.get(camera.camera_id) ? "Paired" : "Not paired"}</small></span>
                      <button
                        className="feature-button"
                        aria-label={ptzConfigured.get(camera.camera_id) ? "Replace PTZ" : "Pair PTZ"}
                        onClick={() => void (ptzConfigured.get(camera.camera_id) ? startOnvifDiscovery(camera, "ptz") : pairPtz(camera))}
                        disabled={busyCamera !== null || rowBusy || onvifBusy}
                      >{ptzConfigured.get(camera.camera_id) ? "Re-pair" : "Pair"}</button>
                      {ptzConfigured.get(camera.camera_id) && (
                        <button className="quiet-button" aria-label="Unpair PTZ" onClick={() => void unpairPtz(camera)} disabled={busyCamera !== null || rowBusy}>Remove</button>
                      )}
                    </div>
                    <div className="integration-action-group">
                      <span className="integration-label"><strong>Motion</strong><small>{events?.configured ? (events.desired ? "Monitoring" : "Paired · off") : "Not paired"}</small></span>
                      <button
                        className="feature-button"
                        aria-label={events?.configured ? "Replace Motion Events" : "Pair Motion Events"}
                        onClick={() => void (events?.configured ? startOnvifDiscovery(camera, "events") : pairEvents(camera))}
                        disabled={busyCamera !== null || rowBusy || onvifBusy}
                      >{events?.configured ? "Re-pair" : "Pair"}</button>
                      {events?.configured && (
                        <>
                          <button
                            className={events.desired ? "toggle-action-button is-on" : "toggle-action-button"}
                            aria-label={events.desired ? "Disable Events" : "Enable Events"}
                            onClick={() => void toggleEvents(camera)}
                            disabled={busyCamera !== null || rowBusy || events.state === "stopping"}
                          >{events.desired ? "Turn off" : "Turn on"}</button>
                          <button className="quiet-button" aria-label="Unpair Motion Events" onClick={() => void unpairEvents(camera)} disabled={busyCamera !== null || rowBusy}>Remove</button>
                        </>
                      )}
                    </div>
                  </div>

                  <button
                    className="subtle-danger-button camera-delete-button"
                    onClick={() => setDeleteTarget(camera)}
                    disabled={busyCamera !== null || rowBusy || ownActive || desiredOn}
                    title={ownActive || desiredOn ? "Turn off recording intent before deleting this camera" : undefined}
                  >Delete camera</button>
                </div>
              </article>
            );
          })}
        </div>
      )}

      {showAddChoice && (
        <div className="panel add-camera-dialog" role="dialog" aria-modal="true" aria-label="Add camera method">
          <div className="panel-heading dialog-heading">
            <div>
              <span className="dialog-kicker">New camera</span>
              <h3>How do you want to add it?</h3>
              <p className="muted">ONVIF is the easiest path for supported cameras. Manual RTSP remains available for everything else.</p>
            </div>
            <button className="quiet-button" onClick={() => setShowAddChoice(false)}>Close</button>
          </div>
          <div className="add-method-grid">
            <button className="add-method-card is-recommended" aria-label="Discover ONVIF cameras" onClick={() => void startOnvifDiscovery(null)}>
              <span className="add-method-icon" aria-hidden="true">ON</span>
              <span className="add-method-copy"><strong>Discover with ONVIF</strong><small>Find local cameras, authenticate, inspect media profiles and create the RTSP configuration automatically.</small></span>
              <span className="method-badge">Recommended</span>
            </button>
            <button className="add-method-card" aria-label="Add RTSP manually" onClick={openManualCreate}>
              <span className="add-method-icon" aria-hidden="true">RT</span>
              <span className="add-method-copy"><strong>Enter RTSP manually</strong><small>Use a known host, port, path and camera credentials without ONVIF discovery.</small></span>
            </button>
          </div>
        </div>
      )}

      {onvifStep !== "idle" && (
        <div className="panel onvif-dialog" role="dialog" aria-modal="true" aria-label={eventTarget ? "ONVIF motion event pairing" : ptzTarget ? "ONVIF PTZ pairing" : "ONVIF camera onboarding"}>
          <div className="panel-heading dialog-heading">
            <div>
              <span className="dialog-kicker">ONVIF setup</span>
              <h3>{eventTarget ? `Pair motion events · ${eventTarget.display_name}` : ptzTarget ? `Pair PTZ · ${ptzTarget.display_name}` : "Add camera with ONVIF"}</h3>
              <p className="muted">Discover locally, authenticate once, then choose the stream Nian Vision should use.</p>
            </div>
            <button type="button" className="quiet-button" onClick={() => void closeOnvif()}>Close</button>
          </div>

          <div className="onvif-progress" aria-label="ONVIF setup progress">
            {[
              ["devices", "Discover"],
              ["credentials", "Sign in"],
              ["profiles", "Stream"],
              ["ready", "Review"],
            ].map(([step, label], index) => {
              const order: OnvifStep[] = ["scanning", "devices", "credentials", "profiles", "ready"];
              const current = order.indexOf(onvifStep);
              const target = order.indexOf(step as OnvifStep);
              return (
                <span key={step} className={`onvif-progress-step ${current >= target ? "is-reached" : ""} ${onvifStep === step || (onvifStep === "scanning" && step === "devices") ? "is-current" : ""}`}>
                  <i aria-hidden="true">{index + 1}</i>{label}
                </span>
              );
            })}
          </div>

          <div className="dialog-scroll onvif-dialog-body">
            {onvifStep === "scanning" && (
              <div className="onvif-loading-state">
                <span className="activity-ring" aria-hidden="true" />
                <h4>Looking for local cameras</h4>
                <p className="muted" role="status">Scanning the local network for ONVIF devices…</p>
                <button type="button" className="quiet-button" onClick={() => void closeOnvif()}>Cancel discovery</button>
              </div>
            )}

            {onvifStep === "devices" && (
              <div className="onvif-step-content">
                <div className="section-heading">
                  <div><h4>Discovered devices</h4><p className="muted">Choose the camera you want to configure.</p></div>
                  <button type="button" onClick={() => void startOnvifDiscovery()} disabled={onvifBusy}>{onvifBusy ? "Scanning…" : "Scan again"}</button>
                </div>
                {onvifDevices.length === 0 ? (
                  <div className="inline-empty" role="status"><strong>No ONVIF cameras found</strong><span>Check that the camera and this computer are on the same local network.</span></div>
                ) : (
                  <div className="onvif-device-list">
                    {onvifDevices.map((device) => (
                      <article className="onvif-device-row" key={device.device_id}>
                        <div className="device-avatar" aria-hidden="true">ON</div>
                        <div className="onvif-device-copy">
                          <strong>{device.label}</strong>
                          <span>{device.network_address}</span>
                          <code>{device.endpoint_reference}</code>
                        </div>
                        <button type="button" className="primary-button" onClick={() => chooseOnvifDevice(device)}>Use device</button>
                      </article>
                    ))}
                  </div>
                )}
              </div>
            )}

            {onvifStep === "credentials" && onvifDevice && (
              <form className="onvif-credentials-form" onSubmit={(event) => void connectOnvifDevice(event)}>
                <div className="selected-device-summary">
                  <div className="device-avatar" aria-hidden="true">ON</div>
                  <div><span className="eyebrow">Selected camera</span><strong>{onvifDevice.label}</strong><small>{onvifDevice.network_address}</small></div>
                </div>
                <div className="credential-grid">
                  <label>ONVIF username
                    <input aria-label="ONVIF username" autoComplete="off" value={onvifUsername} onChange={(event) => setOnvifUsername(event.target.value)} placeholder="Camera account username" />
                  </label>
                  <label>ONVIF password
                    <PasswordInput ariaLabel="ONVIF password" value={onvifPassword} onChange={(event) => setOnvifPassword(event.target.value)} placeholder="Camera account password" />
                  </label>
                </div>
                <p className="field-help">These credentials are used to authenticate with this camera. They are not displayed again after pairing.</p>
                <div className="dialog-inline-actions">
                  <button type="button" className="quiet-button" onClick={() => setOnvifStep("devices")} disabled={onvifBusy}>Back</button>
                  <button className="primary-button" type="submit" disabled={onvifBusy}>{onvifBusy ? (ptzTarget ? "Validating PTZ…" : eventTarget ? "Validating events…" : "Connecting…") : (ptzTarget || eventTarget ? "Authenticate & pair" : "Continue")}</button>
                </div>
              </form>
            )}

            {onvifStep === "profiles" && onvifConnection && (
              <div className="onvif-step-content">
                <div className="section-heading">
                  <div>
                    <h4>Choose a video stream</h4>
                    <p className="muted">{[onvifConnection.manufacturer, onvifConnection.model, onvifConnection.hostname].filter(Boolean).join(" · ")}</p>
                  </div>
                </div>
                <div className="onvif-profile-list">
                  {onvifConnection.profiles.map((profile) => (
                    <article className={`onvif-profile-card ${profile.supported ? "" : "is-unavailable"}`} key={profile.token}>
                      <div className="onvif-profile-main">
                        <div className="onvif-profile-title">
                          <strong>{profile.name ?? profile.token}</strong>
                          {profile.recommended && <span className="chip chip-recording">Recommended</span>}
                        </div>
                        <span className="profile-spec">{profileSummary(profile)}</span>
                        <span className="profile-audio">Audio: {profile.audio_codec ?? "none / unknown"}</span>
                        {!profile.supported && (
                          <p className="capability-note"><strong>Recording unavailable for this profile.</strong> Nian Vision currently records H.264 video; this profile advertises {profile.video_codec ?? "an unsupported video codec"}.</p>
                        )}
                      </div>
                      <button
                        type="button"
                        className={profile.supported ? "primary-button" : "quiet-button"}
                        disabled={!profile.supported || onvifBusy}
                        onClick={() => void prepareOnvifProfile(profile)}
                      >
                        {onvifBusy && onvifProfileToken === profile.token ? "Resolving…" : profile.supported ? "Use stream" : "Unavailable"}
                      </button>
                    </article>
                  ))}
                </div>
              </div>
            )}

            {onvifStep === "ready" && onvifPrepared && selectedOnvifProfile && (
              <div className="onvif-ready">
                <p className="success-message" role="status">Stream verified. Review the camera details before adding it.</p>
                <div className="onvif-review-summary">
                  <div><span>Stream endpoint</span><strong>{onvifPrepared.host}:{onvifPrepared.port}{onvifPrepared.path}</strong></div>
                  <div><span>Selected profile</span><strong>{selectedOnvifProfile.name ?? selectedOnvifProfile.token}</strong><small>{profileSummary(selectedOnvifProfile)}</small></div>
                </div>
                {onvifPrepared.host_mismatch && (
                  <p className="warning-message">The stream host differs from the ONVIF device-service host but is still a local address. Verify it before saving.</p>
                )}
                <div className="credential-grid">
                  <label>Camera ID
                    <input aria-label="ONVIF camera ID" value={onvifCameraId} onChange={(event) => setOnvifCameraId(event.target.value)} />
                  </label>
                  <label>Display name
                    <input aria-label="ONVIF display name" value={onvifDisplayName} onChange={(event) => setOnvifDisplayName(event.target.value)} />
                  </label>
                </div>
                <label className="onvif-audio-policy">Audio policy
                  <SelectControl
                    ariaLabel="ONVIF audio policy"
                    value={onvifAudioPolicy}
                    onChange={(value) => setOnvifAudioPolicy(value as AudioPolicy)}
                    options={[{ value: "exclude", label: "Video only", description: "Always available" }, { value: "copy_all", label: "Record AAC audio", description: compatibleOnvifAudio ? "Available for this profile" : "Camera profile is not AAC", disabled: !compatibleOnvifAudio }]}
                  />
                </label>
                {!compatibleOnvifAudio && selectedOnvifProfile.audio_codec && (
                  <p className="capability-note"><strong>Video-only for this profile.</strong> The camera advertises {selectedOnvifProfile.audio_codec} audio. This onboarding path currently copies AAC audio only, so the video stream can still be added without audio.</p>
                )}
                <div className="dialog-inline-actions">
                  <button type="button" className="quiet-button" onClick={() => setOnvifStep("profiles")} disabled={onvifBusy}>Back</button>
                  <button className="primary-button" type="button" onClick={() => void addOnvifCamera()} disabled={onvifBusy}>{onvifBusy ? "Testing & adding…" : "Test & add camera"}</button>
                </div>
              </div>
            )}
          </div>
        </div>
      )}

      {form && (
        <form className="panel camera-editor-dialog" role="dialog" aria-modal="true" aria-label={creating ? "Add RTSP camera" : `Edit ${form.display_name}`} onSubmit={(event) => void submitCamera(event)}>
          <div className="panel-heading dialog-heading">
            <div>
              <span className="dialog-kicker">{creating ? "Manual RTSP setup" : "Camera settings"}</span>
              <h3>{creating ? "Add RTSP camera" : `Edit ${form.display_name}`}</h3>
              {!creating && <p className="muted">Camera ID <code>{form.camera_id}</code></p>}
            </div>
            <button type="button" className="quiet-button" onClick={() => setForm(null)} disabled={saving || probing}>Close</button>
          </div>

          <div className="dialog-scroll camera-editor-body">
            {formCameraActive && (
              <p className="warning-message" role="status">Recording is active. Stream endpoint, credentials and audio policy are locked until recording stops.</p>
            )}

            <section className="form-section">
              <div className="form-section-heading"><div><h4>Identity</h4><p className="muted">How this camera appears in Nian Vision.</p></div></div>
              <label className="full-width">Display name
                <input aria-label="Display name" value={form.display_name} maxLength={128} onChange={(e) => patchForm("display_name", e.target.value)} placeholder="Front door, Garage, Office…" />
              </label>
            </section>

            <section className="form-section">
              <div className="form-section-heading"><div><h4>RTSP stream</h4><p className="muted">Network endpoint used for recording and live view.</p></div></div>
              <div className="camera-editor-grid endpoint-grid">
                <label>Host / IP
                  <input aria-label="Host / IP" value={form.host} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("host", e.target.value)} placeholder="192.168.1.20" />
                </label>
                <label className="port-field">RTSP port
                  <input aria-label="RTSP port" type="number" min={1} max={65535} value={form.port} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("port", Number(e.target.value))} />
                </label>
                <label className="full-width">RTSP path
                  <input aria-label="RTSP path" value={form.path} maxLength={4096} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("path", e.target.value)} placeholder="/stream1" />
                </label>
              </div>
            </section>

            <section className="form-section">
              <div className="form-section-heading"><div><h4>Camera credentials</h4><p className="muted">Used only when connecting to the stream. Existing credentials stay saved when both fields are left blank.</p></div></div>
              <div className="credential-grid">
                <label>Username
                  <input aria-label="Username" autoComplete="off" value={form.username} maxLength={256} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("username", e.target.value)} placeholder={creating ? "Camera username" : "Keep saved username"} />
                </label>
                <label>Password
                  <PasswordInput ariaLabel="Password" value={form.password} maxLength={512} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("password", e.target.value)} placeholder={creating ? "Camera password" : "Keep saved password"} />
                </label>
              </div>
            </section>

            <section className="form-section">
              <div className="form-section-heading"><div><h4>Recording media</h4><p className="muted">Choose whether compatible audio tracks are copied with the video.</p></div></div>
              <label className="audio-policy-field">Audio policy
                <SelectControl
                  ariaLabel="Audio policy"
                  value={form.audio_policy}
                  disabled={criticalFieldsDisabled}
                  onChange={(value) => patchForm("audio_policy", value as CameraFormState["audio_policy"])}
                  options={[{ value: "copy_all", label: "Record compatible audio", description: "Copy supported audio tracks when present" }, { value: "exclude", label: "Video only", description: "Ignore camera audio" }]}
                />
              </label>
            </section>

            {probeResult && <ProbeSummary result={probeResult} />}
          </div>

          <div className="dialog-footer">
            <button type="button" className="secondary-action-button" onClick={() => void testConnection()} disabled={probing || saving || formCameraActive}>{probing ? "Testing connection…" : "Test connection"}</button>
            <button className="primary-button" type="submit" disabled={saving || probing}>{saving ? "Saving…" : creating ? "Add camera" : "Save changes"}</button>
          </div>
        </form>
      )}

      {deleteTarget && (
        <div className="panel confirmation" role="dialog" aria-label="Delete camera confirmation">
          <h3>Delete {deleteTarget.display_name}?</h3>
          <p>Only the camera configuration and credential are removed. Existing footage stays on disk.</p>
          <div className="button-row">
            <button onClick={() => setDeleteTarget(null)} disabled={busyCamera !== null}>Cancel</button>
            <button className="danger-button" onClick={() => void confirmDelete()} disabled={busyCamera !== null}>Delete camera</button>
          </div>
        </div>
      )}
    </section>
  );
}
