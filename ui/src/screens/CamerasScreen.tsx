import { useCallback, useEffect, useMemo, useState } from "react";
import type { FormEvent } from "react";
import { EmptyState } from "../components/EmptyState";
import { STOPPED_STATUS, desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type {
  CameraCommandInput,
  CameraMutation,
  CameraSummary,
  DesktopError,
  ProbeResult,
  RecordingState,
  RecordingStatus,
} from "../lib/tauri";

type CameraFormState = CameraCommandInput;

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

function cameraState(cameraId: string, status: RecordingStatus): RecordingState {
  return status.camera_id === cameraId ? status.state : "stopped";
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

export function CamerasScreen() {
  const [cameras, setCameras] = useState<CameraSummary[]>([]);
  const [recording, setRecording] = useState<RecordingStatus>(STOPPED_STATUS);
  const [loading, setLoading] = useState(isTauri());
  const [error, setError] = useState<DesktopError | null>(null);
  const [form, setForm] = useState<CameraFormState | null>(null);
  const [creating, setCreating] = useState(false);
  const [saving, setSaving] = useState(false);
  const [probing, setProbing] = useState(false);
  const [probeResult, setProbeResult] = useState<ProbeResult | null>(null);
  const [deleteTarget, setDeleteTarget] = useState<CameraSummary | null>(null);
  const [busyCamera, setBusyCamera] = useState<string | null>(null);

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
      setRecording(await invokeDesktop<RecordingStatus>("recording_status"));
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

  const globalActive = ACTIVE_STATES.has(recording.state);
  const formCameraActive = form !== null && recording.camera_id === form.camera_id && globalActive;
  const criticalFieldsDisabled = formCameraActive;
  const sortedCameras = useMemo(
    () => [...cameras].sort((a, b) => a.display_name.localeCompare(b.display_name)),
    [cameras],
  );

  function openCreate() {
    setCreating(true);
    setForm(blankCamera());
    setProbeResult(null);
    setError(null);
  }

  function openEdit(camera: CameraSummary) {
    setCreating(false);
    setForm(editCamera(camera));
    setProbeResult(null);
    setError(null);
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
    if (busyCamera) return;
    setBusyCamera(camera.camera_id);
    setError(null);
    try {
      const ownState = cameraState(camera.camera_id, recording);
      const next = ACTIVE_STATES.has(ownState)
        ? await invokeDesktop<RecordingStatus>("recording_stop")
        : await invokeDesktop<RecordingStatus>("recording_start", { cameraId: camera.camera_id });
      setRecording(next);
    } catch (cause) {
      setError(desktopError(cause));
      await refreshStatus();
    } finally {
      setBusyCamera(null);
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
          <p className="muted">Saved RTSP cameras. M5 records one camera at a time.</p>
        </div>
        <button className="primary-button" onClick={openCreate} disabled={saving}>Add camera</button>
      </div>

      {error && <div className="error-banner" role="alert"><strong>{error.code}</strong>: {error.message}</div>}

      {loading ? (
        <p className="muted" role="status">Loading cameras…</p>
      ) : sortedCameras.length === 0 ? (
        <EmptyState title="No cameras configured" hint="Add an RTSP camera, test the connection, then start recording." />
      ) : (
        <div className="camera-grid">
          {sortedCameras.map((camera) => {
            const state = cameraState(camera.camera_id, recording);
            const ownActive = recording.camera_id === camera.camera_id && ACTIVE_STATES.has(recording.state);
            const startDisabled = busyCamera !== null || (!ownActive && globalActive);
            return (
              <article className="camera-card" key={camera.camera_id}>
                <div className="camera-card-head">
                  <div>
                    <h3>{camera.display_name}</h3>
                    <code>{camera.host}:{camera.port}{camera.path}</code>
                  </div>
                  <span className={`chip chip-${state}`}>{statusLabel(state)}</span>
                </div>
                <div className="camera-placeholder" aria-label="Live view unavailable">
                  <span>No live view in M5</span>
                </div>
                {recording.camera_id === camera.camera_id && (
                  <p className="camera-metrics muted">
                    Segments: {recording.finalized_segments} · Reconnect attempt: {recording.reconnect_attempt}
                    {recording.failure_category ? ` · ${recording.failure_category}` : ""}
                  </p>
                )}
                <div className="button-row">
                  <button
                    className={ownActive ? "danger-button" : "primary-button"}
                    disabled={startDisabled || state === "stopping"}
                    onClick={() => void toggleRecording(camera)}
                  >
                    {ownActive ? (state === "stopping" ? "Stopping…" : "Stop") : "Start"}
                  </button>
                  <button onClick={() => openEdit(camera)} disabled={busyCamera !== null}>Edit</button>
                  <button
                    onClick={() => setDeleteTarget(camera)}
                    disabled={busyCamera !== null || ownActive}
                    title={ownActive ? "Stop recording before deleting this camera" : undefined}
                  >Delete</button>
                </div>
              </article>
            );
          })}
        </div>
      )}

      {form && (
        <form className="panel form-grid" onSubmit={(event) => void submitCamera(event)}>
          <div className="panel-heading">
            <div>
              <h3>{creating ? "Add camera" : `Edit ${form.display_name}`}</h3>
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
