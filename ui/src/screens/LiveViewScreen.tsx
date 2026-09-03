import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { EmptyState } from "../components/EmptyState";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type {
  CameraSummary,
  DesktopError,
  LiveOpenDto,
  LiveState,
  LiveStatus,
  RecordingIntent,
  RecordingState,
  RecordingStatus,
} from "../lib/tauri";

const MAX_LIVE_VIEWS = 4;
const STATUS_POLL_MS = 1_000;
const KEEPALIVE_MS = 30_000;

const ACTIVE_RECORDING_STATES = new Set<RecordingState>([
  "starting",
  "recovering",
  "connecting",
  "recording",
  "backoff",
  "stopping",
]);

function stateLabel(state: LiveState): string {
  if (state === "backoff") return "Reconnecting";
  return state.replaceAll("_", " ");
}

function liveChipClass(state: LiveState): string {
  if (state === "live") return "chip-recording";
  if (state === "backoff") return "chip-reconnecting";
  if (state === "failed") return "chip-failed";
  return `chip-${state}`;
}

export function LiveViewScreen() {
  const [cameras, setCameras] = useState<CameraSummary[]>([]);
  const [selected, setSelected] = useState<string[]>([]);
  const [pickerCameraId, setPickerCameraId] = useState("");
  const [sessions, setSessions] = useState<Map<string, LiveOpenDto>>(() => new Map());
  const [statuses, setStatuses] = useState<LiveStatus[]>([]);
  const [recordings, setRecordings] = useState<RecordingStatus[]>([]);
  const [intent, setIntent] = useState<RecordingIntent>({ camera_ids: [] });
  const [opening, setOpening] = useState<Set<string>>(() => new Set());
  const [tileErrors, setTileErrors] = useState<Map<string, DesktopError>>(() => new Map());
  const [recordingBusy, setRecordingBusy] = useState<Set<string>>(() => new Set());
  const [loading, setLoading] = useState(isTauri());
  const [error, setError] = useState<DesktopError | null>(null);
  const sessionsRef = useRef(sessions);
  const openingRef = useRef(opening);

  useEffect(() => {
    sessionsRef.current = sessions;
  }, [sessions]);

  useEffect(() => {
    openingRef.current = opening;
  }, [opening]);

  const refreshStatuses = useCallback(async () => {
    if (!isTauri()) return;
    try {
      const [live, runtime, desired] = await Promise.all([
        invokeDesktop<LiveStatus[]>("live_statuses"),
        invokeDesktop<RecordingStatus[]>("recording_statuses"),
        invokeDesktop<RecordingIntent>("recording_intent"),
      ]);
      setStatuses(live);
      setRecordings(runtime);
      setIntent(desired);

      const backendSessions = new Set(live.map((status) => status.session_id));
      const staleCameras: string[] = [];
      for (const [cameraId, session] of sessionsRef.current) {
        if (!backendSessions.has(session.session_id) && !openingRef.current.has(cameraId)) {
          staleCameras.push(cameraId);
        }
      }
      if (staleCameras.length) {
        setSessions((current) => {
          const next = new Map(current);
          for (const cameraId of staleCameras) next.delete(cameraId);
          return next;
        });
        setTileErrors((current) => {
          const next = new Map(current);
          for (const cameraId of staleCameras) {
            next.set(cameraId, {
              code: "session_expired",
              message: "Live session ended. Retry to reconnect this camera.",
            });
          }
          return next;
        });
      }
    } catch (cause) {
      setError(desktopError(cause));
    }
  }, []);

  useEffect(() => {
    if (!isTauri()) {
      setLoading(false);
      return;
    }
    let disposed = false;
    void invokeDesktop<CameraSummary[]>("camera_list")
      .then((rows) => {
        if (!disposed) {
          setCameras(rows);
          setPickerCameraId(rows[0]?.camera_id ?? "");
          setError(null);
        }
      })
      .catch((cause) => {
        if (!disposed) setError(desktopError(cause));
      })
      .finally(() => {
        if (!disposed) setLoading(false);
      });
    void refreshStatuses();
    const timer = window.setInterval(() => void refreshStatuses(), STATUS_POLL_MS);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [refreshStatuses]);

  useEffect(() => {
    if (!isTauri()) return;
    const timer = window.setInterval(() => {
      for (const session of sessionsRef.current.values()) {
        void invokeDesktop<void>("live_keepalive", { sessionId: session.session_id }).catch(() => undefined);
      }
    }, KEEPALIVE_MS);
    return () => window.clearInterval(timer);
  }, []);

  useEffect(() => {
    return () => {
      if (!isTauri()) return;
      for (const session of sessionsRef.current.values()) {
        void invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
      }
    };
  }, []);

  const cameraById = useMemo(
    () => new Map(cameras.map((camera) => [camera.camera_id, camera])),
    [cameras],
  );
  const statusBySession = useMemo(
    () => new Map(statuses.map((status) => [status.session_id, status])),
    [statuses],
  );
  const recordingByCamera = useMemo(
    () => new Map(recordings.filter((status) => status.camera_id).map((status) => [status.camera_id as string, status])),
    [recordings],
  );
  const availableCameras = useMemo(
    () => cameras.filter((camera) => !selected.includes(camera.camera_id)),
    [cameras, selected],
  );

  useEffect(() => {
    if (availableCameras.some((camera) => camera.camera_id === pickerCameraId)) return;
    setPickerCameraId(availableCameras[0]?.camera_id ?? "");
  }, [availableCameras, pickerCameraId]);

  async function openCamera(cameraId: string) {
    if (!isTauri() || openingRef.current.has(cameraId)) return;
    setOpening((current) => new Set(current).add(cameraId));
    setTileErrors((current) => {
      const next = new Map(current);
      next.delete(cameraId);
      return next;
    });
    try {
      const opened = await invokeDesktop<LiveOpenDto>("live_open", { cameraId });
      setSessions((current) => new Map(current).set(cameraId, opened));
      await refreshStatuses();
    } catch (cause) {
      setTileErrors((current) => new Map(current).set(cameraId, desktopError(cause)));
    } finally {
      setOpening((current) => {
        const next = new Set(current);
        next.delete(cameraId);
        return next;
      });
    }
  }

  function addSelectedCamera() {
    if (!pickerCameraId || selected.includes(pickerCameraId) || selected.length >= MAX_LIVE_VIEWS) return;
    setSelected((current) => [...current, pickerCameraId]);
    void openCamera(pickerCameraId);
  }

  async function removeCamera(cameraId: string) {
    const session = sessionsRef.current.get(cameraId);
    setSelected((current) => current.filter((id) => id !== cameraId));
    setSessions((current) => {
      const next = new Map(current);
      next.delete(cameraId);
      return next;
    });
    setTileErrors((current) => {
      const next = new Map(current);
      next.delete(cameraId);
      return next;
    });
    if (session && isTauri()) {
      await invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
    }
  }

  async function retryCamera(cameraId: string) {
    const session = sessionsRef.current.get(cameraId);
    if (session && isTauri()) {
      await invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
      setSessions((current) => {
        const next = new Map(current);
        next.delete(cameraId);
        return next;
      });
    }
    await openCamera(cameraId);
  }

  async function handleMediaError(cameraId: string) {
    const session = sessionsRef.current.get(cameraId);
    if (session && isTauri()) {
      await invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
    }
    setSessions((current) => {
      const next = new Map(current);
      next.delete(cameraId);
      return next;
    });
    setTileErrors((current) =>
      new Map(current).set(cameraId, {
        code: "media_failed",
        message: "The live media element failed. Retry to create a fresh session.",
      }),
    );
  }

  async function toggleRecording(cameraId: string) {
    if (recordingBusy.has(cameraId) || !isTauri()) return;
    const desiredOn = intent.camera_ids.includes(cameraId);
    const runtime = recordingByCamera.get(cameraId);
    if (!desiredOn && runtime && ACTIVE_RECORDING_STATES.has(runtime.state)) return;
    setRecordingBusy((current) => new Set(current).add(cameraId));
    setError(null);
    try {
      await invokeDesktop<RecordingStatus>(desiredOn ? "recording_stop" : "recording_start", { cameraId });
      await refreshStatuses();
    } catch (cause) {
      setError(desktopError(cause));
      await refreshStatuses();
    } finally {
      setRecordingBusy((current) => {
        const next = new Set(current);
        next.delete(cameraId);
        return next;
      });
    }
  }

  return (
    <section className="screen-stack" aria-label="Live view">
      <div className="screen-toolbar">
        <div>
          <h2>Live View</h2>
          <p className="muted">Up to {MAX_LIVE_VIEWS} independent H.264 live sessions. Recording remains separate.</p>
        </div>
        <div className="live-picker">
          <select
            aria-label="Camera to add"
            value={pickerCameraId}
            onChange={(event) => setPickerCameraId(event.target.value)}
            disabled={!availableCameras.length || selected.length >= MAX_LIVE_VIEWS}
          >
            {availableCameras.map((camera) => (
              <option key={camera.camera_id} value={camera.camera_id}>{camera.display_name}</option>
            ))}
          </select>
          <button
            className="primary-button"
            type="button"
            onClick={addSelectedCamera}
            disabled={!pickerCameraId || selected.length >= MAX_LIVE_VIEWS}
          >
            Add to live view
          </button>
        </div>
      </div>

      {error && <div className="error-banner" role="alert"><strong>{error.code}</strong>: {error.message}</div>}

      {loading ? (
        <p className="muted">Loading cameras…</p>
      ) : !cameras.length ? (
        <EmptyState title="No cameras configured" hint="Add a camera first, then select it here for live viewing." />
      ) : !selected.length ? (
        <EmptyState title="No live cameras selected" hint="Choose a configured camera above. Unselected cameras consume no live-view capacity." />
      ) : (
        <div className={`live-grid live-grid-${Math.min(selected.length, MAX_LIVE_VIEWS)}`}>
          {selected.map((cameraId) => {
            const camera = cameraById.get(cameraId);
            if (!camera) return null;
            const session = sessions.get(cameraId);
            const backendStatus = session ? statusBySession.get(session.session_id) : undefined;
            const state: LiveState = backendStatus?.state ?? session?.state ?? "starting";
            const tileError = tileErrors.get(cameraId);
            const isOpening = opening.has(cameraId);
            const recording = recordingByCamera.get(cameraId);
            const desiredOn = intent.camera_ids.includes(cameraId);
            const recordingActive = recording ? ACTIVE_RECORDING_STATES.has(recording.state) : false;
            const recordingConvergingOff = !desiredOn && recordingActive;
            const canRenderVideo = Boolean(session && backendStatus?.state === "live" && !tileError);
            return (
              <article className="live-tile" key={cameraId} aria-label={`${camera.display_name} live camera`}>
                <div className="live-tile-head">
                  <div>
                    <h3>{camera.display_name}</h3>
                    <span className={`chip ${liveChipClass(tileError ? "failed" : state)}`}>
                      {tileError ? "Failed" : isOpening ? "Starting" : stateLabel(state)}
                    </span>
                  </div>
                  <button type="button" onClick={() => void removeCamera(cameraId)}>Remove</button>
                </div>

                <div className="live-media-frame">
                  {canRenderVideo && session ? (
                    <video
                      key={`${session.session_id}-${backendStatus?.reconnect_attempt ?? 0}`}
                      className="live-video"
                      src={session.url}
                      autoPlay
                      muted
                      playsInline
                      onError={() => void handleMediaError(cameraId)}
                    />
                  ) : (
                    <div className="live-placeholder">
                      {tileError
                        ? tileError.message
                        : state === "backoff"
                          ? `Reconnecting · attempt ${backendStatus?.reconnect_attempt ?? 0}`
                          : state === "failed"
                            ? backendStatus?.failure_category ?? "Live stream failed"
                            : "Connecting to camera…"}
                    </div>
                  )}
                </div>

                <div className="live-tile-meta">
                  <span>Recording desired: <strong>{desiredOn ? "On" : "Off"}</strong></span>
                  <span>Runtime: <strong>{recording?.state ?? "stopped"}</strong></span>
                </div>

                <div className="button-row">
                  <button
                    type="button"
                    onClick={() => void toggleRecording(cameraId)}
                    disabled={recordingBusy.has(cameraId) || recordingConvergingOff}
                  >
                    {desiredOn ? "Stop recording" : recordingConvergingOff ? "Stopping…" : "Start recording"}
                  </button>
                  {(tileError || state === "failed") && (
                    <button type="button" className="primary-button" onClick={() => void retryCamera(cameraId)} disabled={isOpening}>
                      Retry live
                    </button>
                  )}
                </div>
              </article>
            );
          })}
        </div>
      )}
    </section>
  );
}
