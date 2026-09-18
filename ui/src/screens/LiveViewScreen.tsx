import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { EmptyState } from "../components/EmptyState";
import { PtzControls } from "../components/PtzControls";
import { SelectControl } from "../components/SelectControl";
import { detectLiveVideoConfiguration, splitLiveMp4ForMse } from "../lib/liveMp4";
import { teardownLiveMse } from "../lib/mediaLifecycle";
import { retainMseSession } from "../lib/mediaTelemetry";
import { desktopError, invokeDesktop, isTauri } from "../lib/tauri";
import type {
  CameraSummary,
  DesktopError,
  EventStatus,
  LiveFailureCategory,
  LiveOpenDto,
  LiveState,
  LiveStatus,
  PtzCapabilities,
  RecordingIntent,
  RecordingState,
  RecordingStatus,
} from "../lib/tauri";

const LIVE_LAYOUT_SIZES = [1, 4, 8, 16] as const;
type LiveLayoutSize = (typeof LIVE_LAYOUT_SIZES)[number];
type LiveStreamProfile = "grid" | "focus";
const MAX_LIVE_VIEWS_PER_PAGE = 16;
const STATUS_POLL_MS = 1_000;
const EVENT_STATUS_POLL_MS = 5_000;
const KEEPALIVE_MS = 30_000;
const MAX_LIVE_LATENCY_SECONDS = 2.5;
const LIVE_EDGE_OFFSET_SECONDS = 0.75;
// Live View is a realtime surface, not a browser-side DVR. Keeping a long MSE
// history pins decoded frames and GPU-backed surfaces for no visible benefit.
// The backend retains a slightly larger compressed window to absorb poll jitter.
const LIVE_BUFFER_HISTORY_SECONDS = 3;
const LIVE_MANIFEST_POLL_MS = 250;
const LIVE_STABLE_RESET_MS = 10_000;
const MAX_LIVE_AUTO_RECOVERY_ATTEMPTS = 3;
const LIVE_RECOVERY_BACKOFF_MS = [250, 750, 1_500] as const;
const LIVE_VIEW_PREFERENCES_KEY = "nian.live-view.preferences.v1";

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

function recordingStateLabel(state: RecordingState): string {
  if (state === "backoff") return "Reconnecting";
  return state.charAt(0).toUpperCase() + state.slice(1).replaceAll("_", " ");
}

function recordingFailureLabel(category: string | null | undefined): string | null {
  switch (category) {
    case "source_open_failed": return "recording stream unavailable";
    case "source_read_failed": return "stream disconnected";
    case "source_timed_out": return "stream timeout";
    case "output_write_failed": return "output write failed";
    case "storage_failed": return "storage unavailable";
    default: return null;
  }
}

function liveFailureLabel(category: LiveFailureCategory | null | undefined): string | null {
  switch (category) {
    case "source_open_failed": return "Camera stream unavailable";
    case "unsupported_codec": return "Camera codec is not supported for live view";
    case "worker_unavailable": return "Live media worker is unavailable";
    case "media_read_failed": return "Camera stream disconnected";
    case "media_fragment_create_failed": return "Could not create the live buffer";
    case "media_fragment_write_failed": return "Could not write the live buffer";
    case "media_packet_too_large": return "Camera sent an oversized media packet";
    case "media_fragment_limit_exceeded": return "Live buffer reached its safety limit";
    case "media_mux_write_failed": return "Could not package the live stream";
    case "media_fragment_finalize_failed": return "Could not finalize the live buffer";
    case "media_fragment_capacity_failed": return "Live buffer capacity is exhausted";
    case "lifecycle_cancelled": return "Live session was interrupted by app lifecycle";
    case "media_failed": return "Live media pipeline failed";
    default: return null;
  }
}

function hasDedicatedSubstream(camera: CameraSummary | undefined): boolean {
  return Boolean(camera?.sub_host && camera.sub_port && camera.sub_path);
}

type LiveManifest = {
  session_id: string;
  fragments: number[];
};

type LiveScaleMode = "fit" | "native";

type LiveViewPreferences = {
  cameraIds: string[];
  scaleMode: LiveScaleMode;
  layoutSize: LiveLayoutSize;
  page: number;
};

function readLiveViewPreferences(): LiveViewPreferences {
  const fallback: LiveViewPreferences = { cameraIds: [], scaleMode: "fit", layoutSize: 4, page: 0 };
  try {
    const raw = window.localStorage.getItem(LIVE_VIEW_PREFERENCES_KEY);
    if (!raw) return fallback;
    const parsed = JSON.parse(raw) as Partial<LiveViewPreferences>;
    const cameraIds = Array.isArray(parsed.cameraIds)
      ? [...new Set(parsed.cameraIds.filter((cameraId): cameraId is string => typeof cameraId === "string" && cameraId.length > 0))]
      : [];
    const scaleMode: LiveScaleMode = parsed.scaleMode === "native" ? "native" : "fit";
    const layoutSize = LIVE_LAYOUT_SIZES.includes(parsed.layoutSize as LiveLayoutSize)
      ? parsed.layoutSize as LiveLayoutSize
      : 4;
    const page = Number.isSafeInteger(parsed.page) && (parsed.page ?? 0) >= 0 ? parsed.page as number : 0;
    return { cameraIds, scaleMode, layoutSize, page };
  } catch {
    return fallback;
  }
}

function persistLiveViewPreferences(preferences: LiveViewPreferences): void {
  try {
    window.localStorage.setItem(LIVE_VIEW_PREFERENCES_KEY, JSON.stringify(preferences));
  } catch {
    // A disabled/full WebView storage area must not make live viewing fail.
  }
}

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

function appendMseBytes(sourceBuffer: SourceBuffer, bytes: Uint8Array): void {
  if (bytes.buffer instanceof ArrayBuffer) {
    const arrayBufferView = bytes as Uint8Array<ArrayBuffer>;
    sourceBuffer.appendBuffer(arrayBufferView);
    return;
  }
  sourceBuffer.appendBuffer(Uint8Array.from(bytes).buffer);
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
  scaleMode,
  showDiagnostics,
  onError,
  onStable,
}: {
  session: LiveOpenDto;
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
    const releaseMseSession = retainMseSession();
    const abort = new AbortController();
    let disposed = false;
    let timer: number | null = null;
    let sourceBuffer: SourceBuffer | null = null;
    let activeVideoSignature: string | null = null;
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

      const configuration = detectLiveVideoConfiguration(mseParts.initialization);
      if (!configuration || !MediaSource.isTypeSupported(configuration.mime)) {
        throw new LivePipelineError("unsupported_codec", "The WebView cannot decode this AVC/HEVC stream. For H.265, install the OS HEVC codec or select an H.264 substream.");
      }
      if (activeVideoSignature !== null && activeVideoSignature !== configuration.signature) {
        // A codec, SPS/PPS/VPS or resolution switch needs a fresh MSE decoder.
        // Preserve existing backend retry handling rather than appending corrupt media.
        throw new LivePipelineError("stream_configuration_changed", "The camera video configuration changed. Reopening live with a fresh decoder.");
      }
      if (!sourceBuffer) {
        sourceBuffer = mediaSource.addSourceBuffer(configuration.mime);
        activeVideoSignature = configuration.signature;
        sourceBuffer.mode = "segments";
      }
      if (!initializationAppended) {
        await waitForSourceBuffer(sourceBuffer);
        appendMseBytes(sourceBuffer, mseParts.initialization);
        await waitForSourceBuffer(sourceBuffer);
        initializationAppended = true;
      }
      await waitForSourceBuffer(sourceBuffer);
      sourceBuffer.timestampOffset = nextTimestampOffset;
      appendMseBytes(sourceBuffer, mseParts.media);
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
      teardownLiveMse({ mediaSource, sourceBuffer, video, objectUrl });
      releaseMseSession();
    };
  }, [session.session_id, session.url]);

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
  const initialPreferencesRef = useRef<LiveViewPreferences | null>(null);
  if (initialPreferencesRef.current === null) {
    initialPreferencesRef.current = readLiveViewPreferences();
  }
  const initialPreferences = initialPreferencesRef.current;
  const [cameras, setCameras] = useState<CameraSummary[]>([]);
  const [selected, setSelected] = useState<string[]>(initialPreferences.cameraIds);
  const [pickerCameraId, setPickerCameraId] = useState("");
  const [scaleMode, setScaleMode] = useState<LiveScaleMode>(initialPreferences.scaleMode);
  const [layoutSize, setLayoutSize] = useState<LiveLayoutSize>(initialPreferences.layoutSize);
  const [page, setPage] = useState(initialPreferences.page);
  const [showVideoDiagnostics, setShowVideoDiagnostics] = useState(false);
  const [focusedCameraId, setFocusedCameraId] = useState<string | null>(null);
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
  const pageCount = Math.max(1, Math.ceil(selected.length / layoutSize));
  const effectivePage = Math.min(page, pageCount - 1);
  const visibleCameraIds = useMemo(
    () => selected.slice(effectivePage * layoutSize, (effectivePage + 1) * layoutSize),
    [effectivePage, layoutSize, selected],
  );
  const visibleSlots = useMemo(
    () => Array.from({ length: layoutSize }, (_, index) => visibleCameraIds[index] ?? null),
    [layoutSize, visibleCameraIds],
  );
  const sessionsRef = useRef(sessions);
  const camerasRef = useRef(cameras);
  camerasRef.current = cameras;
  const focusedCameraIdRef = useRef<string | null>(focusedCameraId);
  focusedCameraIdRef.current = focusedCameraId;
  const previousFocusedCameraRef = useRef<string | null>(null);
  const sessionProfileRef = useRef<Map<string, LiveStreamProfile>>(new Map());
  const selectedRef = useRef<Set<string>>(new Set(initialPreferences.cameraIds));
  const activeCameraIdsRef = useRef<Set<string>>(new Set(visibleCameraIds));
  activeCameraIdsRef.current = new Set(visibleCameraIds);
  const mountedRef = useRef(true);
  const generationRef = useRef<Map<string, number>>(new Map());
  const pendingOpenRef = useRef<Map<string, number>>(new Map());
  const recoveryAttemptsRef = useRef<Map<string, number>>(new Map());
  const recoveryTimersRef = useRef<Map<string, number>>(new Map());
  const refreshInFlightRef = useRef(false);
  const eventStatusInFlightRef = useRef(false);
  const restoredSelectionOpenedRef = useRef(initialPreferences.cameraIds.length === 0);

  useEffect(() => {
    persistLiveViewPreferences({ cameraIds: selected, scaleMode, layoutSize, page: effectivePage });
  }, [effectivePage, layoutSize, scaleMode, selected]);

  useEffect(() => {
    if (page !== effectivePage) setPage(effectivePage);
  }, [effectivePage, page]);

  useEffect(() => {
    if (!focusedCameraId) return;
    if (!visibleCameraIds.includes(focusedCameraId)) {
      setFocusedCameraId(null);
      return;
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") setFocusedCameraId(null);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [focusedCameraId, visibleCameraIds]);

  const setSessionForCamera = useCallback((cameraId: string, session: LiveOpenDto | null) => {
    const next = new Map(sessionsRef.current);
    if (session) next.set(cameraId, session);
    else {
      next.delete(cameraId);
      sessionProfileRef.current.delete(cameraId);
    }
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
    for (const cameraId of visibleCameraIds) {
      if (ptzCapabilities.has(cameraId)) continue;
      void invokeDesktop<PtzCapabilities>("ptz_capabilities", { cameraId })
        .then((capabilities) => {
          if (disposed || !activeCameraIdsRef.current.has(cameraId)) return;
          setPtzCapabilities((current) => new Map(current).set(cameraId, capabilities));
          setPtzErrors((current) => {
            const next = new Map(current);
            next.delete(cameraId);
            return next;
          });
        })
        .catch((cause) => {
          if (disposed || !activeCameraIdsRef.current.has(cameraId)) return;
          setPtzErrors((current) => new Map(current).set(cameraId, desktopError(cause)));
        });
    }
    return () => { disposed = true; };
  }, [ptzCapabilities, visibleCameraIds]);

  useEffect(() => {
    if (!isTauri() || visibleCameraIds.length === 0) {
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
        const selectedIds = activeCameraIdsRef.current;
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
  }, [visibleCameraIds]);

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
      activeCameraIdsRef.current.has(cameraId) &&
      generationRef.current.get(cameraId) === generation
    );
  }, []);

  const startOpenGeneration = useCallback(async (cameraId: string, generation: number) => {
    if (!isTauri() || !mountedRef.current || pendingOpenRef.current.has(cameraId)) return;
    const camera = camerasRef.current.find((candidate) => candidate.camera_id === cameraId);
    const profile: LiveStreamProfile =
      focusedCameraIdRef.current === cameraId && hasDedicatedSubstream(camera) ? "focus" : "grid";
    pendingOpenRef.current.set(cameraId, generation);
    setOpening((current) => new Set(current).add(cameraId));
    setTileErrors((current) => {
      const next = new Map(current);
      next.delete(cameraId);
      return next;
    });

    try {
      const opened = await invokeDesktop<LiveOpenDto>("live_open", { cameraId, profile });
      if (!ownsGeneration(cameraId, generation)) {
        await invokeDesktop<void>("live_close", { sessionId: opened.session_id }).catch(() => undefined);
        return;
      }
      sessionProfileRef.current.set(cameraId, profile);
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
        activeCameraIdsRef.current.has(cameraId) &&
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

  const switchLiveProfile = useCallback(async (cameraId: string) => {
    if (!isTauri() || !mountedRef.current || !activeCameraIdsRef.current.has(cameraId)) return;
    const camera = camerasRef.current.find((candidate) => candidate.camera_id === cameraId);
    const desiredProfile: LiveStreamProfile =
      focusedCameraIdRef.current === cameraId && hasDedicatedSubstream(camera) ? "focus" : "grid";
    const session = sessionsRef.current.get(cameraId);
    if (session && sessionProfileRef.current.get(cameraId) === desiredProfile) return;

    const recoveryTimer = recoveryTimersRef.current.get(cameraId);
    if (recoveryTimer !== undefined) window.clearTimeout(recoveryTimer);
    recoveryTimersRef.current.delete(cameraId);
    recoveryAttemptsRef.current.delete(cameraId);

    const generation = nextGeneration(cameraId);
    if (session) {
      setSessionForCamera(cameraId, null);
      await invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
    }
    if (
      mountedRef.current
      && activeCameraIdsRef.current.has(cameraId)
      && !pendingOpenRef.current.has(cameraId)
    ) {
      void startOpenGeneration(cameraId, generation);
    }
  }, [nextGeneration, setSessionForCamera, startOpenGeneration]);

  useEffect(() => {
    const previous = previousFocusedCameraRef.current;
    if (previous === focusedCameraId) return;
    previousFocusedCameraRef.current = focusedCameraId;
    if (previous) void switchLiveProfile(previous);
    if (focusedCameraId) void switchLiveProfile(focusedCameraId);
  }, [focusedCameraId, switchLiveProfile]);


  useEffect(() => {
    if (!isTauri() || loading || restoredSelectionOpenedRef.current) return;
    const configured = new Set(cameras.map((camera) => camera.camera_id));
    const restored = [...selectedRef.current].filter((cameraId) => configured.has(cameraId));
    selectedRef.current = new Set(restored);
    if (restored.length !== selected.length || restored.some((cameraId, index) => selected[index] !== cameraId)) {
      setSelected(restored);
    }
    restoredSelectionOpenedRef.current = true;
  }, [cameras, loading, selected]);

  useEffect(() => {
    if (!isTauri() || loading || !restoredSelectionOpenedRef.current) return;
    const desired = new Set(visibleCameraIds);

    setOpening((current) => new Set([...current].filter((cameraId) => desired.has(cameraId))));
    for (const [cameraId, session] of [...sessionsRef.current.entries()]) {
      if (desired.has(cameraId)) continue;
      const recoveryTimer = recoveryTimersRef.current.get(cameraId);
      if (recoveryTimer !== undefined) window.clearTimeout(recoveryTimer);
      recoveryTimersRef.current.delete(cameraId);
      recoveryAttemptsRef.current.delete(cameraId);
      nextGeneration(cameraId);
      setSessionForCamera(cameraId, null);
      void invokeDesktop<void>("live_close", { sessionId: session.session_id }).catch(() => undefined);
    }

    for (const cameraId of visibleCameraIds) {
      if (!sessionsRef.current.has(cameraId) && !pendingOpenRef.current.has(cameraId)) requestOpen(cameraId);
    }
  }, [loading, nextGeneration, requestOpen, setSessionForCamera, visibleCameraIds]);

  function addSelectedCamera() {
    if (!pickerCameraId || selectedRef.current.has(pickerCameraId)) return;
    const nextSelected = new Set(selectedRef.current);
    nextSelected.add(pickerCameraId);
    selectedRef.current = nextSelected;
    const next = [...nextSelected];
    setSelected(next);
    setPage(Math.max(0, Math.ceil(next.length / layoutSize) - 1));
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
    if (focusedCameraId === cameraId) setFocusedCameraId(null);
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
    if (!mountedRef.current || !activeCameraIdsRef.current.has(cameraId)) return;

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
      if (!mountedRef.current || !activeCameraIdsRef.current.has(cameraId)) {
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

  function changeLayout(nextLayout: LiveLayoutSize) {
    const anchorIndex = effectivePage * layoutSize;
    setLayoutSize(nextLayout);
    setPage(Math.floor(anchorIndex / nextLayout));
  }

  function focusCamera(cameraId: string) {
    if (!visibleCameraIds.includes(cameraId)) return;
    setFocusedCameraId(cameraId);
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
          <p className="muted">1/4/8/{MAX_LIVE_VIEWS_PER_PAGE}-zone layouts. Double-click a camera for focus view; only the current page opens live sessions.</p>
        </div>
        <div className="live-picker">
          <div className="live-layout-controls" role="group" aria-label="Live view layout">
            <span className="live-control-label">Layout</span>
            {LIVE_LAYOUT_SIZES.map((size) => (
              <button
                type="button"
                key={size}
                aria-label={`${size} camera layout`}
                aria-pressed={layoutSize === size}
                onClick={() => changeLayout(size)}
              >
                {size}
              </button>
            ))}
          </div>
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
          <div className="live-add-control">
            <SelectControl
              ariaLabel="Camera to add"
              value={pickerCameraId}
              onChange={setPickerCameraId}
              disabled={!availableCameras.length}
              options={availableCameras.map((camera) => ({ value: camera.camera_id, label: camera.display_name, description: `${camera.host}:${camera.port}${camera.path}` }))}
              placeholder="Choose camera"
            />
            <button
              className="primary-button"
              type="button"
              onClick={addSelectedCamera}
              disabled={!pickerCameraId}
            >
              Add to live view
            </button>
          </div>
        </div>
      </div>

      {error && <div className="error-banner" role="alert"><strong>{error.code}</strong>: {error.message}</div>}

      {loading ? (
        <p className="muted">Loading cameras…</p>
      ) : !cameras.length ? (
        <EmptyState title="No cameras configured" hint="Add a camera first, then select it here for live viewing." />
      ) : !selected.length ? (
        <EmptyState title="No live cameras selected" hint="Choose configured cameras above. Cameras on inactive pages consume no live-view capacity." />
      ) : (
        <>
          <div className="live-pagebar" aria-label="Live view pages">
            <div className="live-page-summary">
              <strong>{selected.length}</strong> camera{selected.length === 1 ? "" : "s"}
              <span>{layoutSize}-zone layout</span>
              <span>Page {effectivePage + 1} / {pageCount}</span>
            </div>
            <div className="live-page-actions">
              <button type="button" aria-label="Previous live page" onClick={() => setPage((current) => Math.max(0, current - 1))} disabled={effectivePage === 0}>←</button>
              <button type="button" aria-label="Next live page" onClick={() => setPage((current) => Math.min(pageCount - 1, current + 1))} disabled={effectivePage >= pageCount - 1}>→</button>
            </div>
          </div>
          {focusedCameraId && typeof document !== "undefined" && createPortal(
            <button
              type="button"
              className="live-focus-backdrop"
              aria-label="Close focused camera"
              onClick={() => setFocusedCameraId(null)}
            />,
            document.body,
          )}
          <div className={`live-grid live-grid-layout-${layoutSize} ${layoutSize > 1 ? "live-grid-multi" : "live-grid-single"}`}>
          {visibleSlots.map((cameraId, slotIndex) => {
            if (!cameraId) {
              return (
                <div className="live-empty-slot" key={`empty-${effectivePage}-${slotIndex}`} aria-label={`Empty live view slot ${slotIndex + 1}`}>
                  <span className="live-empty-slot-number">{slotIndex + 1}</span>
                  <div className="live-empty-slot-copy">
                    <strong>Empty view</strong>
                    <span>Add a camera to use this zone</span>
                  </div>
                </div>
              );
            }
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
            const recordingLabel = recordingStateLabel(recording?.state ?? "stopped");
            const recordingReason = recordingFailureLabel(recording?.failure_category);
            const liveReason = liveFailureLabel(backendStatus?.failure_category);
            const recordingConvergingOff = !desiredOn && recordingActive;
            const events = eventStatuses.get(cameraId);
            const isFocused = focusedCameraId === cameraId;
            const canRenderVideo = Boolean(
              session && !tileError && state !== "failed" && state !== "stopping",
            );
            const motionLabel = events?.motion_active === true
              ? "Motion detected"
              : events?.configured
                ? `Motion ${events.desired ? events.state : "off"}`
                : "Motion unpaired";
            const tile = (
              <article
                className={`live-tile${isFocused ? " is-focus-viewer" : ""}`}
                key={cameraId}
                role={isFocused ? "dialog" : undefined}
                aria-modal={isFocused ? true : undefined}
                aria-label={`${camera.display_name} live camera`}
                title={!isFocused ? "Double-click to open focus view" : undefined}
                onDoubleClick={() => { if (!isFocused) focusCamera(cameraId); }}
              >
                <div className="live-tile-head" onDoubleClick={(event) => event.stopPropagation()}>
                  <div className="live-tile-identity">
                    <h3>{camera.display_name}</h3>
                    <span className={`chip ${liveChipClass(tileError ? "failed" : state)}`} title={liveReason ?? undefined}>
                      {tileError ? "Failed" : isOpening ? "Starting" : stateLabel(state)}
                    </span>
                  </div>
                  <div className="live-tile-head-actions">
                    <button
                      type="button"
                      className="live-focus-button"
                      aria-label={isFocused ? `Close ${camera.display_name} focus view` : `Open ${camera.display_name} focus view`}
                      title={isFocused ? "Close focus view" : "Open focus view"}
                      onClick={() => setFocusedCameraId(isFocused ? null : cameraId)}
                    >
                      <span aria-hidden="true">{isFocused ? "×" : "↗"}</span>
                    </button>
                    {!isFocused && <button type="button" onClick={() => void removeCamera(cameraId)}>Remove</button>}
                  </div>
                </div>

                <div className="live-tile-body">
                  <div className="live-media-frame">
                    {canRenderVideo && session ? (
                      <LiveMedia
                        key={session.session_id}
                        session={session}
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
                            ? `Reconnecting${liveReason ? ` · ${liveReason}` : ""} · attempt ${backendStatus?.reconnect_attempt ?? 0}`
                            : state === "failed"
                              ? liveReason ?? "Live stream failed"
                              : "Connecting to camera…"}
                      </div>
                    )}
                  </div>

                  {isFocused && (
                    <aside className="live-focus-inspector" onDoubleClick={(event) => event.stopPropagation()}>
                      <div className="live-focus-inspector-head">
                        <strong>Camera controls</strong>
                        <span>Live session stays on the current page</span>
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
                      <div className="live-focus-status-grid">
                        <div><span>Recording</span><strong>{desiredOn ? "Desired on" : "Manual off"}</strong><small>{recordingLabel}{recordingReason ? ` · ${recordingReason}` : ""}</small></div>
                        <div className={events?.motion_active === true ? "is-motion-active" : ""}><span>Motion</span><strong>{motionLabel}</strong><small>{events?.last_error_code ?? "No event error"}</small></div>
                      </div>
                      <div className="live-focus-actions">
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
                    </aside>
                  )}
                </div>

                {!isFocused && (
                  <div className="live-tile-quickbar" onDoubleClick={(event) => event.stopPropagation()}>
                    <div className="live-tile-meta">
                      <span title={recordingReason ?? undefined}>Rec <strong>{recordingLabel}{recordingReason && recording?.state === "backoff" ? ` · ${recordingReason}` : ""}</strong></span>
                      <span className={events?.motion_active === true ? "motion-indicator" : ""}>{motionLabel}</span>
                      {events?.last_error_code && <span>Event error: <strong>{events.last_error_code}</strong></span>}
                    </div>
                    <div className="live-tile-quick-actions">
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
                  </div>
                )}
              </article>
            );
            return isFocused && typeof document !== "undefined"
              ? [
                  <div className="live-focus-origin-placeholder" key={`${cameraId}-focus-origin`} aria-hidden="true" />,
                  createPortal(tile, document.body, `${cameraId}-focus-viewer`),
                ]
              : tile;
          })}
          </div>
        </>
      )}
    </section>
  );
}
