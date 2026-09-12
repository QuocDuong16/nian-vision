import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { EmptyState } from "../components/EmptyState";
import { PtzControls } from "../components/PtzControls";
import { splitLiveMp4ForMse } from "../lib/liveMp4";
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
const MAX_LIVE_LATENCY_SECONDS = 2.5;
const LIVE_EDGE_OFFSET_SECONDS = 0.75;
const LIVE_BUFFER_HISTORY_SECONDS = 12;
const LIVE_MANIFEST_POLL_MS = 250;
const LIVE_STABLE_RESET_MS = 10_000;
const MAX_LIVE_AUTO_RECOVERY_ATTEMPTS = 3;
const LIVE_RECOVERY_BACKOFF_MS = [250, 750, 1_500] as const;

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

type LiveScaleMode = "fit" | "native";

type LiveRenderStats = {
  sourceWidth: number;
  sourceHeight: number;
  displayDeviceWidth: number;
  displayDeviceHeight: number;
  devicePixelRatio: number;
  scale: number;
};

function fragmentName(sequence: number): string {
  return `fragment-${String(sequence).padStart(12, "0")}.mp4`;
}

class LivePipelineError extends Error {
  constructor(readonly code: string, message: string) {
    super(message);
    this.name = "LivePipelineError";
  }
}

function livePipelineFailure(cause: unknown): DesktopError {
  if (cause instanceof LivePipelineError) return { code: cause.code, message: cause.message };
  if (cause instanceof Error) return { code: "live_pipeline_failed", message: cause.message };
  return { code: "live_pipeline_failed", message: "The live media pipeline failed." };
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
      reject(new LivePipelineError("source_buffer_failed", "The live SourceBuffer reported an append/remove failure."));
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
  scaleMode,
  showDiagnostics,
  onError,
  onStable,
}: {
  session: LiveOpenDto;
  reconnectAttempt: number;
  scaleMode: LiveScaleMode;
  showDiagnostics: boolean;
  onError: (failure: DesktopError) => void;
  onStable: () => void;
}) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const onErrorRef = useRef(onError);
  const onStableRef = useRef(onStable);
  const [renderStats, setRenderStats] = useState<LiveRenderStats | null>(null);

  useEffect(() => {
    onErrorRef.current = onError;
  }, [onError]);

  useEffect(() => {
    onStableRef.current = onStable;
  }, [onStable]);

  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;

    const update = () => {
      const sourceWidth = video.videoWidth;
      const sourceHeight = video.videoHeight;
      if (!sourceWidth || !sourceHeight) return;
      const rect = video.getBoundingClientRect();
      const dpr = Math.max(window.devicePixelRatio || 1, 1);
      const sourceAspect = sourceWidth / sourceHeight;
      const boxAspect = rect.width > 0 && rect.height > 0 ? rect.width / rect.height : sourceAspect;
      const renderedCssWidth = boxAspect > sourceAspect ? rect.height * sourceAspect : rect.width;
      const renderedCssHeight = boxAspect > sourceAspect ? rect.height : rect.width / sourceAspect;
      const displayDeviceWidth = Math.max(1, Math.round(renderedCssWidth * dpr));
      const displayDeviceHeight = Math.max(1, Math.round(renderedCssHeight * dpr));
      setRenderStats({
        sourceWidth,
        sourceHeight,
        displayDeviceWidth,
        displayDeviceHeight,
        devicePixelRatio: dpr,
        scale: Math.max(displayDeviceWidth / sourceWidth, displayDeviceHeight / sourceHeight),
      });
    };

    video.addEventListener("loadedmetadata", update);
    video.addEventListener("resize", update);
    window.addEventListener("resize", update);
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(update);
    observer?.observe(video);
    update();
    return () => {
      video.removeEventListener("loadedmetadata", update);
      video.removeEventListener("resize", update);
      window.removeEventListener("resize", update);
      observer?.disconnect();
    };
  }, [scaleMode, session.session_id]);

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
    let initializationAppended = false;
    let nextMovieFragmentSequence = 1;
    let nextTimestampOffset = 0;
    let stableReported = false;
    const startedAt = performance.now();

    video.src = objectUrl;

    const fail = (cause: unknown) => {
      if (!disposed && !abort.signal.aborted) onErrorRef.current(livePipelineFailure(cause));
    };

    const reportStable = () => {
      if (stableReported || performance.now() - startedAt < LIVE_STABLE_RESET_MS) return;
      stableReported = true;
      onStableRef.current();
    };

    const requestLiveResource = async (url: string, code: string, label: string) => {
      try {
        return await fetch(url, { cache: "no-store", signal: abort.signal });
      } catch (cause) {
        if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
        throw new LivePipelineError(code, `${label} request could not reach the local live session.`);
      }
    };

    const appendFragment = async (sequence: number) => {
      const response = await requestLiveResource(
        `${session.url}/fragment/${fragmentName(sequence)}`,
        "fragment_fetch_failed",
        "Live fragment",
      );
      if (response.status === 404 || response.status === 410) {
        throw new LivePipelineError("fragment_expired", "A live fragment expired before WebView could append it.");
      }
      if (!response.ok) throw new LivePipelineError("fragment_fetch_failed", `Live fragment request failed with HTTP ${response.status}.`);
      let bytes: Uint8Array;
      try {
        bytes = new Uint8Array(await response.arrayBuffer());
      } catch {
        throw new LivePipelineError("fragment_fetch_failed", "The live fragment response ended before it could be read.");
      }
      if (!bytes.length || disposed || abort.signal.aborted) return;
      const mseParts = splitLiveMp4ForMse(bytes, nextMovieFragmentSequence);
      if (!mseParts) {
        throw new LivePipelineError("fragment_invalid", "A live fragment was not a valid fragmented MP4 stream.");
      }

      if (!sourceBuffer) {
        const mime = detectAvcMime(bytes);
        if (!mime || !MediaSource.isTypeSupported(mime)) {
          throw new LivePipelineError("unsupported_codec", "This H.264 stream is not supported by the WebView MediaSource decoder.");
        }
        sourceBuffer = mediaSource.addSourceBuffer(mime);
        sourceBuffer.mode = "segments";
      }
      if (!initializationAppended) {
        await waitForSourceBuffer(sourceBuffer);
        sourceBuffer.appendBuffer(Uint8Array.from(mseParts.initialization).buffer);
        await waitForSourceBuffer(sourceBuffer);
        initializationAppended = true;
      }
      await waitForSourceBuffer(sourceBuffer);
      sourceBuffer.timestampOffset = nextTimestampOffset;
      sourceBuffer.appendBuffer(Uint8Array.from(mseParts.media).buffer);
      await waitForSourceBuffer(sourceBuffer);
      nextMovieFragmentSequence += mseParts.movieFragmentCount;
      lastAppendedSequence = Math.max(lastAppendedSequence, sequence);

      if (sourceBuffer.buffered.length > 0) {
        const range = sourceBuffer.buffered.length - 1;
        const start = sourceBuffer.buffered.start(range);
        const end = sourceBuffer.buffered.end(range);
        nextTimestampOffset = end;
        const latency = end - video.currentTime;
        if (video.currentTime < start || latency > MAX_LIVE_LATENCY_SECONDS) {
          video.currentTime = Math.max(start, end - LIVE_EDGE_OFFSET_SECONDS);
        }
        const removeBefore = Math.max(0, video.currentTime - LIVE_BUFFER_HISTORY_SECONDS);
        if (removeBefore > 0 && !sourceBuffer.updating) {
          sourceBuffer.remove(0, removeBefore);
          await waitForSourceBuffer(sourceBuffer);
        }
      }
      void video.play().catch(() => undefined);
      reportStable();
    };

    const pump = async () => {
      try {
        const response = await requestLiveResource(
          `${session.url}/manifest`,
          "manifest_fetch_failed",
          "Live manifest",
        );
        if (!response.ok) throw new LivePipelineError("manifest_fetch_failed", `Live manifest request failed with HTTP ${response.status}.`);
        let manifest: LiveManifest;
        try {
          manifest = (await response.json()) as LiveManifest;
        } catch {
          throw new LivePipelineError("manifest_invalid", "The live manifest was not valid JSON.");
        }
        if (manifest.session_id !== session.session_id || !Array.isArray(manifest.fragments)) {
          throw new LivePipelineError("manifest_invalid", "The live manifest did not match the active session.");
        }
        for (const sequence of manifest.fragments) {
          if (!Number.isSafeInteger(sequence) || sequence < 0) {
            throw new LivePipelineError("manifest_invalid", "The live manifest contained an invalid fragment sequence.");
          }
          if (sequence <= lastAppendedSequence) continue;
          if (sequence !== lastAppendedSequence + 1) {
            throw new LivePipelineError("fragment_gap", "Live fragment continuity was lost before WebView could append the next fragment.");
          }
          await appendFragment(sequence);
        }
        if (!disposed) timer = window.setTimeout(() => void pump(), LIVE_MANIFEST_POLL_MS);
      } catch (cause) {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        fail(cause);
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
  const nativeStyle = scaleMode === "native" && renderStats
    ? {
        maxWidth: `min(100%, ${Math.max(1, Math.round(renderStats.sourceWidth / renderStats.devicePixelRatio))}px)`,
        maxHeight: `min(100%, ${Math.max(1, Math.round(renderStats.sourceHeight / renderStats.devicePixelRatio))}px)`,
      }
    : undefined;
  const scaleLabel = renderStats
    ? renderStats.scale > 1.02
      ? `${renderStats.scale.toFixed(2)}× upscale`
      : renderStats.scale < 0.98
        ? `${renderStats.scale.toFixed(2)}× downscale`
        : "1:1 pixels"
    : null;
  return (
    <>
      <video
        ref={videoRef}
        className={`live-video ${scaleMode === "native" ? "live-video-native" : "live-video-fit"}`}
        style={nativeStyle}
        src={fallback ? session.url : undefined}
        autoPlay
        muted
        playsInline
        onError={() => onErrorRef.current({
          code: "media_element_failed",
          message: "The live video element reported a decode or playback failure.",
        })}
      />
      {showDiagnostics && renderStats && (
        <div className="live-video-diagnostics" aria-label="Live video render diagnostics">
          <span>{renderStats.sourceWidth}×{renderStats.sourceHeight} source</span>
          <span>{renderStats.displayDeviceWidth}×{renderStats.displayDeviceHeight} display px</span>
          <span>{scaleLabel}</span>
          <span>DPR {renderStats.devicePixelRatio.toFixed(2)}</span>
        </div>
      )}
    </>
  );
}

export function LiveViewScreen() {
  const [cameras, setCameras] = useState<CameraSummary[]>([]);
  const [selected, setSelected] = useState<string[]>([]);
  const [pickerCameraId, setPickerCameraId] = useState("");
  const [scaleMode, setScaleMode] = useState<LiveScaleMode>("fit");
  const [showVideoDiagnostics, setShowVideoDiagnostics] = useState(false);
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
  const recoveryAttemptsRef = useRef<Map<string, number>>(new Map());
  const recoveryTimersRef = useRef<Map<string, number>>(new Map());
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
      for (const timer of recoveryTimersRef.current.values()) window.clearTimeout(timer);
      recoveryTimersRef.current.clear();
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
    const recoveryTimer = recoveryTimersRef.current.get(cameraId);
    if (recoveryTimer !== undefined) window.clearTimeout(recoveryTimer);
    recoveryTimersRef.current.delete(cameraId);
    recoveryAttemptsRef.current.delete(cameraId);
    setOpening((current) => {
      const next = new Set(current);
      next.delete(cameraId);
      return next;
    });
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
    const recoveryTimer = recoveryTimersRef.current.get(cameraId);
    if (recoveryTimer !== undefined) window.clearTimeout(recoveryTimer);
    recoveryTimersRef.current.delete(cameraId);
    recoveryAttemptsRef.current.delete(cameraId);
    const session = sessionsRef.current.get(cameraId);
    nextGeneration(cameraId);
    setSessionForCamera(cameraId, null);
    if (session && isTauri()) {
      await invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
    }
    requestOpen(cameraId);
  }

  function markLiveStable(cameraId: string) {
    recoveryAttemptsRef.current.delete(cameraId);
  }

  async function handleMediaError(cameraId: string, failure: DesktopError) {
    if (recoveryTimersRef.current.has(cameraId)) return;
    const session = sessionsRef.current.get(cameraId);
    const generation = nextGeneration(cameraId);
    setSessionForCamera(cameraId, null);
    if (session && isTauri()) {
      await invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
    }
    if (!mountedRef.current || !selectedRef.current.has(cameraId)) return;

    const attempt = (recoveryAttemptsRef.current.get(cameraId) ?? 0) + 1;
    if (attempt > MAX_LIVE_AUTO_RECOVERY_ATTEMPTS) {
      recoveryAttemptsRef.current.set(cameraId, MAX_LIVE_AUTO_RECOVERY_ATTEMPTS);
      setOpening((current) => {
        const next = new Set(current);
        next.delete(cameraId);
        return next;
      });
      setTileErrors((current) => new Map(current).set(cameraId, {
        code: failure.code,
        message: `${failure.message} Automatic recovery stopped after ${MAX_LIVE_AUTO_RECOVERY_ATTEMPTS} attempts. Retry live to start a fresh recovery cycle.`,
      }));
      return;
    }

    recoveryAttemptsRef.current.set(cameraId, attempt);
    setTileErrors((current) => {
      const next = new Map(current);
      next.delete(cameraId);
      return next;
    });
    setOpening((current) => new Set(current).add(cameraId));
    const delay = LIVE_RECOVERY_BACKOFF_MS[attempt - 1] ?? LIVE_RECOVERY_BACKOFF_MS[LIVE_RECOVERY_BACKOFF_MS.length - 1];
    const timer = window.setTimeout(() => {
      recoveryTimersRef.current.delete(cameraId);
      if (!mountedRef.current || !selectedRef.current.has(cameraId)) {
        setOpening((current) => {
          const next = new Set(current);
          next.delete(cameraId);
          return next;
        });
        return;
      }
      void startOpenGeneration(cameraId, generation);
    }, delay);
    recoveryTimersRef.current.set(cameraId, timer);
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
          <div className="live-quality-controls" role="group" aria-label="Live video quality">
            <button
              type="button"
              aria-pressed={scaleMode === "fit"}
              onClick={() => setScaleMode("fit")}
            >
              Fit tile
            </button>
            <button
              type="button"
              aria-pressed={scaleMode === "native"}
              onClick={() => setScaleMode("native")}
            >
              Native pixels
            </button>
            <label className="live-diagnostics-toggle">
              <input
                type="checkbox"
                checked={showVideoDiagnostics}
                onChange={(event) => setShowVideoDiagnostics(event.target.checked)}
              />
              Diagnostics
            </label>
          </div>
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
                      scaleMode={scaleMode}
                      showDiagnostics={showVideoDiagnostics}
                      onError={(failure) => void handleMediaError(cameraId, failure)}
                      onStable={() => markLiveStable(cameraId)}
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
