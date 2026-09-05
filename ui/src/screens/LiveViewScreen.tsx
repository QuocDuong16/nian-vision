import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { EmptyState } from "../components/EmptyState";
import { PtzControls } from "../components/PtzControls";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type {
  CameraSummary,
  DesktopError,
  EventStatus,
  LiveOpenDto,
  LiveState,
  LiveStatus,
  PtzCapabilities,
  RecordingIntent,
  RecordingState,
  RecordingStatus,
} from "../lib/tauri";

const MAX_LIVE_VIEWS = 4;
const STATUS_POLL_MS = 1_000;
const EVENT_STATUS_POLL_MS = 5_000;
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

type LiveManifest = {
  session_id: string;
  fragments: number[];
};

function fragmentName(sequence: number): string {
  return `fragment-${String(sequence).padStart(12, "0")}.mp4`;
}

function detectAvcMime(bytes: Uint8Array): string | null {
  for (let index = 0; index + 8 < bytes.length; index += 1) {
    if (
      bytes[index] === 0x61 &&
      bytes[index + 1] === 0x76 &&
      bytes[index + 2] === 0x63 &&
      bytes[index + 3] === 0x43
    ) {
      const profile = bytes[index + 5]!;
      const compatibility = bytes[index + 6]!;
      const level = bytes[index + 7]!;
      const hex = [profile, compatibility, level]
        .map((value) => value.toString(16).padStart(2, "0"))
        .join("");
      return `video/mp4; codecs="avc1.${hex}"`;
    }
  }
  return null;
}

function waitForSourceBuffer(sourceBuffer: SourceBuffer): Promise<void> {
  if (!sourceBuffer.updating) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const onEnd = () => {
      cleanup();
      resolve();
    };
    const onError = () => {
      cleanup();
      reject(new Error("live source buffer failed"));
    };
    const cleanup = () => {
      sourceBuffer.removeEventListener("updateend", onEnd);
      sourceBuffer.removeEventListener("error", onError);
    };
    sourceBuffer.addEventListener("updateend", onEnd, { once: true });
    sourceBuffer.addEventListener("error", onError, { once: true });
  });
}

function LiveMedia({
  session,
  reconnectAttempt,
  onError,
}: {
  session: LiveOpenDto;
  reconnectAttempt: number;
  onError: () => void;
}) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const onErrorRef = useRef(onError);

  useEffect(() => {
    onErrorRef.current = onError;
  }, [onError]);

  useEffect(() => {
    const video = videoRef.current;
    if (!video || typeof window.MediaSource === "undefined") return;

    const mediaSource = new MediaSource();
    const objectUrl = URL.createObjectURL(mediaSource);
    const abort = new AbortController();
    let disposed = false;
    let timer: number | null = null;
    let sourceBuffer: SourceBuffer | null = null;
    let lastAppendedSequence = -1;

    video.src = objectUrl;

    const fail = () => {
      if (!disposed && !abort.signal.aborted) onErrorRef.current();
    };

    const appendFragment = async (sequence: number) => {
      const response = await fetch(`${session.url}/fragment/${fragmentName(sequence)}`, {
        cache: "no-store",
        signal: abort.signal,
      });
      if (response.status === 404 || response.status === 410) return;
      if (!response.ok) throw new Error(`live fragment failed: ${response.status}`);
      const bytes = new Uint8Array(await response.arrayBuffer());
      if (!bytes.length || disposed || abort.signal.aborted) return;

      if (!sourceBuffer) {
        const mime = detectAvcMime(bytes);
        if (!mime || !MediaSource.isTypeSupported(mime)) {
          throw new Error("live H.264 MediaSource type is unsupported");
        }
        sourceBuffer = mediaSource.addSourceBuffer(mime);
        sourceBuffer.mode = "sequence";
      }
      await waitForSourceBuffer(sourceBuffer);
      sourceBuffer.appendBuffer(bytes);
      await waitForSourceBuffer(sourceBuffer);
      lastAppendedSequence = Math.max(lastAppendedSequence, sequence);

      if (video.buffered.length > 0) {
        const end = video.buffered.end(video.buffered.length - 1);
        if (end - video.currentTime > 8) video.currentTime = Math.max(0, end - 3);
        const removeBefore = Math.max(0, end - 20);
        if (removeBefore > 0 && !sourceBuffer.updating) {
          sourceBuffer.remove(0, removeBefore);
          await waitForSourceBuffer(sourceBuffer);
        }
      }
      void video.play().catch(() => undefined);
    };

    const pump = async () => {
      try {
        const response = await fetch(`${session.url}/manifest`, {
          cache: "no-store",
          signal: abort.signal,
        });
        if (!response.ok) throw new Error(`live manifest failed: ${response.status}`);
        const manifest = (await response.json()) as LiveManifest;
        if (manifest.session_id !== session.session_id || !Array.isArray(manifest.fragments)) {
          throw new Error("live manifest identity mismatch");
        }
        for (const sequence of manifest.fragments) {
          if (!Number.isSafeInteger(sequence) || sequence < 0 || sequence <= lastAppendedSequence) continue;
          await appendFragment(sequence);
        }
        if (!disposed) timer = window.setTimeout(() => void pump(), 500);
      } catch (cause) {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        fail();
      }
    };

    const onSourceOpen = () => void pump();
    mediaSource.addEventListener("sourceopen", onSourceOpen, { once: true });

    return () => {
      disposed = true;
      abort.abort();
      if (timer !== null) window.clearTimeout(timer);
      mediaSource.removeEventListener("sourceopen", onSourceOpen);
      video.removeAttribute("src");
      video.load();
      URL.revokeObjectURL(objectUrl);
    };
  }, [reconnectAttempt, session.session_id, session.url]);

  const fallback = typeof window.MediaSource === "undefined";
  return (
    <video
      ref={videoRef}
      className="live-video"
      src={fallback ? session.url : undefined}
      autoPlay
      muted
      playsInline
      onError={() => onErrorRef.current()}
    />
  );
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
  const [ptzCapabilities, setPtzCapabilities] = useState<Map<string, PtzCapabilities>>(() => new Map());
  const [ptzErrors, setPtzErrors] = useState<Map<string, DesktopError>>(() => new Map());
  const [eventStatuses, setEventStatuses] = useState<Map<string, EventStatus>>(() => new Map());
  const [recordingBusy, setRecordingBusy] = useState<Set<string>>(() => new Set());
  const [loading, setLoading] = useState(isTauri());
  const [error, setError] = useState<DesktopError | null>(null);
  const sessionsRef = useRef(sessions);
  const selectedRef = useRef<Set<string>>(new Set());
  const mountedRef = useRef(true);
  const generationRef = useRef<Map<string, number>>(new Map());
  const pendingOpenRef = useRef<Map<string, number>>(new Map());
  const refreshInFlightRef = useRef(false);
  const eventStatusInFlightRef = useRef(false);

  const setSessionForCamera = useCallback((cameraId: string, session: LiveOpenDto | null) => {
    const next = new Map(sessionsRef.current);
    if (session) next.set(cameraId, session);
    else next.delete(cameraId);
    sessionsRef.current = next;
    if (mountedRef.current) setSessions(next);
  }, []);

  const nextGeneration = useCallback((cameraId: string) => {
    const generation = (generationRef.current.get(cameraId) ?? 0) + 1;
    generationRef.current.set(cameraId, generation);
    return generation;
  }, []);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const refreshStatuses = useCallback(async () => {
    if (!isTauri() || !mountedRef.current || refreshInFlightRef.current) return;
    refreshInFlightRef.current = true;
    try {
      const [live, runtime, desired] = await Promise.all([
        invokeDesktop<LiveStatus[]>("live_statuses"),
        invokeDesktop<RecordingStatus[]>("recording_statuses"),
        invokeDesktop<RecordingIntent>("recording_intent"),
      ]);
      if (!mountedRef.current) return;
      setStatuses(live);
      setRecordings(runtime);
      setIntent(desired);

      const backendSessions = new Set(live.map((status) => status.session_id));
      const staleCameras: string[] = [];
      for (const [cameraId, session] of sessionsRef.current) {
        if (!backendSessions.has(session.session_id) && !pendingOpenRef.current.has(cameraId)) {
          staleCameras.push(cameraId);
        }
      }
      if (staleCameras.length) {
        const nextSessions = new Map(sessionsRef.current);
        for (const cameraId of staleCameras) nextSessions.delete(cameraId);
        sessionsRef.current = nextSessions;
        setSessions(nextSessions);
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
      if (mountedRef.current) setError(desktopError(cause));
    } finally {
      refreshInFlightRef.current = false;
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
    let disposed = false;
    for (const cameraId of selected) {
      if (ptzCapabilities.has(cameraId)) continue;
      void invokeDesktop<PtzCapabilities>("ptz_capabilities", { cameraId })
        .then((capabilities) => {
          if (disposed || !selectedRef.current.has(cameraId)) return;
          setPtzCapabilities((current) => new Map(current).set(cameraId, capabilities));
          setPtzErrors((current) => {
            const next = new Map(current);
            next.delete(cameraId);
            return next;
          });
        })
        .catch((cause) => {
          if (disposed || !selectedRef.current.has(cameraId)) return;
          setPtzErrors((current) => new Map(current).set(cameraId, desktopError(cause)));
        });
    }
    return () => { disposed = true; };
  }, [selected, ptzCapabilities]);

  useEffect(() => {
    if (!isTauri() || selected.length === 0) {
      setEventStatuses(new Map());
      return;
    }
    let disposed = false;
    const poll = async () => {
      if (eventStatusInFlightRef.current) return;
      eventStatusInFlightRef.current = true;
      try {
        const rows = await invokeDesktop<EventStatus[]>("event_statuses");
        if (disposed) return;
        const selectedIds = selectedRef.current;
        setEventStatuses(
          new Map(
            rows
              .filter((status) => selectedIds.has(status.camera_id))
              .map((status) => [status.camera_id, status]),
          ),
        );
      } catch {
        if (!disposed) setEventStatuses(new Map());
      } finally {
        eventStatusInFlightRef.current = false;
      }
    };
    void poll();
    const timer = window.setInterval(() => void poll(), EVENT_STATUS_POLL_MS);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [selected]);

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

  const ownsGeneration = useCallback((cameraId: string, generation: number) => {
    return (
      mountedRef.current &&
      selectedRef.current.has(cameraId) &&
      generationRef.current.get(cameraId) === generation
    );
  }, []);

  const startOpenGeneration = useCallback(async (cameraId: string, generation: number) => {
    if (!isTauri() || !mountedRef.current || pendingOpenRef.current.has(cameraId)) return;
    pendingOpenRef.current.set(cameraId, generation);
    setOpening((current) => new Set(current).add(cameraId));
    setTileErrors((current) => {
      const next = new Map(current);
      next.delete(cameraId);
      return next;
    });

    try {
      const opened = await invokeDesktop<LiveOpenDto>("live_open", { cameraId });
      if (!ownsGeneration(cameraId, generation)) {
        await invokeDesktop<void>("live_close", { sessionId: opened.session_id }).catch(() => undefined);
        return;
      }
      setSessionForCamera(cameraId, opened);
      await refreshStatuses();
    } catch (cause) {
      if (ownsGeneration(cameraId, generation)) {
        setTileErrors((current) => new Map(current).set(cameraId, desktopError(cause)));
      }
    } finally {
      if (pendingOpenRef.current.get(cameraId) === generation) {
        pendingOpenRef.current.delete(cameraId);
      }
      const desiredGeneration = generationRef.current.get(cameraId);
      const shouldRestart =
        mountedRef.current &&
        selectedRef.current.has(cameraId) &&
        desiredGeneration !== undefined &&
        desiredGeneration !== generation &&
        !sessionsRef.current.has(cameraId) &&
        !pendingOpenRef.current.has(cameraId);
      if (shouldRestart) {
        void startOpenGeneration(cameraId, desiredGeneration);
      } else if (mountedRef.current && !pendingOpenRef.current.has(cameraId)) {
        setOpening((current) => {
          const next = new Set(current);
          next.delete(cameraId);
          return next;
        });
      }
    }
  }, [ownsGeneration, refreshStatuses, setSessionForCamera]);

  const requestOpen = useCallback((cameraId: string) => {
    const generation = nextGeneration(cameraId);
    if (!pendingOpenRef.current.has(cameraId)) {
      void startOpenGeneration(cameraId, generation);
    }
    return generation;
  }, [nextGeneration, startOpenGeneration]);

  function addSelectedCamera() {
    if (!pickerCameraId || selectedRef.current.has(pickerCameraId) || selectedRef.current.size >= MAX_LIVE_VIEWS) return;
    const nextSelected = new Set(selectedRef.current);
    nextSelected.add(pickerCameraId);
    selectedRef.current = nextSelected;
    setSelected([...nextSelected]);
    requestOpen(pickerCameraId);
  }

  async function removeCamera(cameraId: string) {
    const nextSelected = new Set(selectedRef.current);
    nextSelected.delete(cameraId);
    selectedRef.current = nextSelected;
    nextGeneration(cameraId);
    const session = sessionsRef.current.get(cameraId);
    setSelected([...nextSelected]);
    setSessionForCamera(cameraId, null);
    setTileErrors((current) => {
      const next = new Map(current);
      next.delete(cameraId);
      return next;
    });
    setPtzCapabilities((current) => {
      const next = new Map(current);
      next.delete(cameraId);
      return next;
    });
    setPtzErrors((current) => {
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
    nextGeneration(cameraId);
    setSessionForCamera(cameraId, null);
    if (session && isTauri()) {
      await invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
    }
    requestOpen(cameraId);
  }

  async function handleMediaError(cameraId: string) {
    const session = sessionsRef.current.get(cameraId);
    nextGeneration(cameraId);
    setSessionForCamera(cameraId, null);
    if (session && isTauri()) {
      await invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
    }
    if (!mountedRef.current || !selectedRef.current.has(cameraId)) return;
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
            const events = eventStatuses.get(cameraId);
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
                    <LiveMedia
                      key={`${session.session_id}-${backendStatus?.reconnect_attempt ?? 0}`}
                      session={session}
                      reconnectAttempt={backendStatus?.reconnect_attempt ?? 0}
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

                <PtzControls
                  cameraId={cameraId}
                  capabilities={ptzCapabilities.get(cameraId) ?? null}
                  error={ptzErrors.get(cameraId) ?? null}
                  onError={(nextError) => setPtzErrors((current) => {
                    const next = new Map(current);
                    if (nextError) next.set(cameraId, nextError);
                    else next.delete(cameraId);
                    return next;
                  })}
                />

                <div className="live-tile-meta">
                  <span>Recording desired: <strong>{desiredOn ? "On" : "Off"}</strong></span>
                  <span>Runtime: <strong>{recording?.state ?? "stopped"}</strong></span>
                  <span>Motion events: <strong>{events?.configured ? (events.desired ? events.state : "off") : "unpaired"}</strong></span>
                  {events?.motion_active === true && <span className="motion-indicator">Motion detected</span>}
                  {events?.last_error_code && <span>Event error: <strong>{events.last_error_code}</strong></span>}
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
