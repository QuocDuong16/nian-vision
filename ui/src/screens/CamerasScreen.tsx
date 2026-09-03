import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { EmptyState } from "../components/EmptyState";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type {
  AudioPolicy,
  CameraCommandInput,
  CameraMutation,
  CameraSummary,
  DesktopError,
  OnvifConnection,
  OnvifDiscoveredDevice,
  OnvifDiscovery,
  OnvifMediaProfile,
  OnvifPreparedProfile,
  ProbeResult,
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
  return state.replaceAll("_", " ");
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
      setCameras(await invokeDesktop<CameraSummary[]>("camera_list"));
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
    void import("@tauri-apps/api/window")
      .then(({ getCurrentWindow }) =>
        getCurrentWindow().onCloseRequested(() => {
          resetOnvifLocal();
          setError(null);
        }),
      )
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

  async function startOnvifDiscovery() {
    if (!isTauri() || onvifBusy) return;
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
      const connection = await invokeDesktop<OnvifConnection>("onvif_connect", {
        input: {
          session_id: onvifSessionId,
          device_id: onvifDevice.device_id,
          username,
          password,
        },
      });
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

  return (
    <section className="screen-stack" aria-label="Camera management">
      <div className="screen-toolbar">
        <div>
          <h2>Cameras</h2>
          <p className="muted">Saved RTSP cameras. ONVIF is used only for local discovery and provisioning.</p>
        </div>
        <button className="primary-button" onClick={openCreate} disabled={saving || onvifBusy}>Add camera</button>
      </div>

      <p className="muted">Active {activeCount} camera{activeCount === 1 ? "" : "s"}{reconnectingCount ? ` · ${reconnectingCount} reconnecting` : ""}</p>

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
                <p className="camera-metrics muted">Desired: {desiredOn ? "On" : "Off"} · Runtime: {statusLabel(state)}</p>
                {runtime && (
                  <p className="camera-metrics muted">
                    Segments: {runtime.finalized_segments} · Reconnect attempt: {runtime.reconnect_attempt}
                    {runtime.failure_category ? ` · ${runtime.failure_category}` : ""}
                  </p>
                )}
                <div className="button-row">
                  <button
                    className={desiredOn || desiredOffRuntimeActive ? "danger-button" : "primary-button"}
                    disabled={recordingControlDisabled}
                    onClick={() => void toggleRecording(camera)}
                  >
                    {recordingControlLabel}
                  </button>
                  <button onClick={() => openEdit(camera)} disabled={busyCamera !== null || rowBusy}>Edit</button>
                  <button
                    onClick={() => setDeleteTarget(camera)}
                    disabled={busyCamera !== null || rowBusy || ownActive || desiredOn}
                    title={ownActive || desiredOn ? "Turn off recording intent before deleting this camera" : undefined}
                  >Delete</button>
                </div>
              </article>
            );
          })}
        </div>
      )}

      {showAddChoice && (
        <div className="panel" role="dialog" aria-label="Add camera method">
          <div className="panel-heading">
            <div>
              <h3>Add camera</h3>
              <p className="muted">Use ONVIF for local discovery, or enter an RTSP endpoint manually.</p>
            </div>
            <button onClick={() => setShowAddChoice(false)}>Close</button>
          </div>
          <div className="button-row">
            <button className="primary-button" onClick={() => void startOnvifDiscovery()}>Discover ONVIF cameras</button>
            <button onClick={openManualCreate}>Add RTSP manually</button>
          </div>
        </div>
      )}

      {onvifStep !== "idle" && (
        <div className="panel form-grid" role="dialog" aria-label="ONVIF camera onboarding">
          <div className="panel-heading">
            <div>
              <h3>Discover ONVIF camera</h3>
              <p className="muted">Discovery and provisioning only. Recording will use the resolved RTSP stream.</p>
            </div>
            <button type="button" onClick={() => void closeOnvif()}>Close</button>
          </div>

          {onvifStep === "scanning" && (
            <div>
              <p role="status">Scanning for ONVIF cameras…</p>
              <button type="button" onClick={() => void closeOnvif()}>Cancel discovery</button>
            </div>
          )}

          {onvifStep === "devices" && (
            <div>
              {onvifDevices.length === 0 ? (
                <p role="status">No ONVIF cameras found.</p>
              ) : (
                <div className="camera-grid">
                  {onvifDevices.map((device) => (
                    <article className="camera-card" key={device.device_id}>
                      <h4>{device.label}</h4>
                      <p className="muted">{device.network_address}</p>
                      <code>{device.endpoint_reference}</code>
                      <div className="button-row">
                        <button type="button" onClick={() => chooseOnvifDevice(device)}>Select</button>
                      </div>
                    </article>
                  ))}
                </div>
              )}
              <div className="button-row">
                <button type="button" onClick={() => void startOnvifDiscovery()} disabled={onvifBusy}>
                  {onvifBusy ? "Scanning…" : "Refresh"}
                </button>
              </div>
            </div>
          )}

          {onvifStep === "credentials" && onvifDevice && (
            <form className="form-grid" onSubmit={(event) => void connectOnvifDevice(event)}>
              <p>Authenticate to <strong>{onvifDevice.label}</strong>.</p>
              <label>ONVIF username
                <input aria-label="ONVIF username" autoComplete="off" value={onvifUsername} onChange={(event) => setOnvifUsername(event.target.value)} />
              </label>
              <label>ONVIF password
                <input aria-label="ONVIF password" type="password" autoComplete="new-password" value={onvifPassword} onChange={(event) => setOnvifPassword(event.target.value)} />
              </label>
              <div className="button-row">
                <button type="button" onClick={() => setOnvifStep("devices")} disabled={onvifBusy}>Back</button>
                <button className="primary-button" type="submit" disabled={onvifBusy}>{onvifBusy ? "Connecting…" : "Connect"}</button>
              </div>
            </form>
          )}

          {onvifStep === "profiles" && onvifConnection && (
            <div>
              <p className="muted">
                {[onvifConnection.manufacturer, onvifConnection.model, onvifConnection.hostname].filter(Boolean).join(" · ")}
              </p>
              <h4>Media profiles</h4>
              <div className="camera-grid">
                {onvifConnection.profiles.map((profile) => (
                  <article className="camera-card" key={profile.token}>
                    <div className="camera-card-head">
                      <h4>{profile.name ?? profile.token}</h4>
                      {profile.recommended && <span className="chip">Recommended</span>}
                    </div>
                    <p>{profileSummary(profile)}</p>
                    <p className="muted">Audio: {profile.audio_codec ?? "none/unknown"}</p>
                    {!profile.supported && <p className="warning-message">Not supported for M10 recording. H.264 is required.</p>}
                    <button
                      type="button"
                      className={profile.supported ? "primary-button" : undefined}
                      disabled={!profile.supported || onvifBusy}
                      onClick={() => void prepareOnvifProfile(profile)}
                    >
                      {onvifBusy && onvifProfileToken === profile.token ? "Resolving…" : "Use profile"}
                    </button>
                  </article>
                ))}
              </div>
            </div>
          )}

          {onvifStep === "ready" && onvifPrepared && selectedOnvifProfile && (
            <div className="form-grid">
              <p className="success-message" role="status">ONVIF profile resolved to a safe RTSP endpoint.</p>
              <p><strong>Stream:</strong> <code>{onvifPrepared.host}:{onvifPrepared.port}{onvifPrepared.path}</code></p>
              {onvifPrepared.host_mismatch && (
                <p className="warning-message">The stream host differs from the ONVIF device-service host but is still a local address. Verify it before saving.</p>
              )}
              <p><strong>Profile:</strong> {selectedOnvifProfile.name ?? selectedOnvifProfile.token} · {profileSummary(selectedOnvifProfile)}</p>
              <label>Camera ID
                <input aria-label="ONVIF camera ID" value={onvifCameraId} onChange={(event) => setOnvifCameraId(event.target.value)} />
              </label>
              <label>Display name
                <input aria-label="ONVIF display name" value={onvifDisplayName} onChange={(event) => setOnvifDisplayName(event.target.value)} />
              </label>
              <label>Audio policy
                <select aria-label="ONVIF audio policy" value={onvifAudioPolicy} onChange={(event) => setOnvifAudioPolicy(event.target.value as AudioPolicy)}>
                  <option value="exclude">Video only</option>
                  <option value="copy_all" disabled={!compatibleOnvifAudio}>Record AAC audio</option>
                </select>
              </label>
              {!compatibleOnvifAudio && selectedOnvifProfile.audio_codec && (
                <p className="warning-message">Audio codec {selectedOnvifProfile.audio_codec} is not enabled by this onboarding flow; video-only remains available.</p>
              )}
              <div className="button-row">
                <button type="button" onClick={() => setOnvifStep("profiles")} disabled={onvifBusy}>Back</button>
                <button className="primary-button" type="button" onClick={() => void addOnvifCamera()} disabled={onvifBusy}>
                  {onvifBusy ? "Testing & adding…" : "Test & Add"}
                </button>
              </div>
            </div>
          )}
        </div>
      )}

      {form && (
        <form className="panel form-grid" onSubmit={(event) => void submitCamera(event)}>
          <div className="panel-heading">
            <div>
              <h3>{creating ? "Add camera manually" : `Edit ${form.display_name}`}</h3>
              {!creating && <p className="muted">Camera ID: <code>{form.camera_id}</code></p>}
            </div>
            <button type="button" onClick={() => setForm(null)} disabled={saving || probing}>Close</button>
          </div>

          {formCameraActive && (
            <p className="warning-message" role="status">
              Recording is active. Endpoint, credentials and audio policy are locked; display name can still be changed.
            </p>
          )}

          <label>Display name
            <input aria-label="Display name" value={form.display_name} maxLength={128} onChange={(e) => patchForm("display_name", e.target.value)} />
          </label>
          <label>Host / IP
            <input aria-label="Host / IP" value={form.host} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("host", e.target.value)} />
          </label>
          <label>RTSP port
            <input aria-label="RTSP port" type="number" min={1} max={65535} value={form.port} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("port", Number(e.target.value))} />
          </label>
          <label>RTSP path
            <input aria-label="RTSP path" value={form.path} maxLength={4096} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("path", e.target.value)} />
          </label>
          <label>Username
            <input aria-label="Username" autoComplete="off" value={form.username} maxLength={256} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("username", e.target.value)} placeholder={creating ? "Camera username" : "Leave blank to keep saved credentials"} />
          </label>
          <label>Password
            <input aria-label="Password" type="password" autoComplete="new-password" value={form.password} maxLength={512} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("password", e.target.value)} placeholder={creating ? "Camera password" : "Leave blank to keep saved credentials"} />
          </label>
          <label>Audio policy
            <select aria-label="Audio policy" value={form.audio_policy} disabled={criticalFieldsDisabled} onChange={(e) => patchForm("audio_policy", e.target.value as CameraFormState["audio_policy"])}>
              <option value="copy_all">Record audio</option>
              <option value="exclude">Video only</option>
            </select>
          </label>

          <div className="form-actions">
            <button type="button" onClick={() => void testConnection()} disabled={probing || saving || formCameraActive}>
              {probing ? "Testing…" : "Test connection"}
            </button>
            <button className="primary-button" type="submit" disabled={saving || probing}>
              {saving ? "Saving…" : "Save camera"}
            </button>
          </div>
          {probeResult && <ProbeSummary result={probeResult} />}
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
