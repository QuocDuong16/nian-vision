import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  desktopError,
  invokeDesktop,
  isTauri,
  type CameraSummary,
  type EventHistoryKind,
  type EventPlaybackOpenDto,
  type EventRecordingContext,
  type EventReviewPage,
  type EventReviewRow,
} from "../lib/tauri";

type RangePreset = "hour" | "day" | "week" | "custom";
type EventKindFilter = "all" | EventHistoryKind;

type EventDatasetSnapshot = {
  generation: number;
  query: {
    camera_ids: string[];
    kind: EventHistoryKind | null;
    from_utc: string;
    to_utc: string;
    limit: number;
  };
};

const PAGE_SIZE = 50;
const REFRESH_INTERVAL_MS = 10_000;
const PLAYBACK_KEEPALIVE_INTERVAL_MS = 45_000;

function dateTimeLocalValue(value: Date): string {
  const local = new Date(value.getTime() - value.getTimezoneOffset() * 60_000);
  return local.toISOString().slice(0, 16);
}

function queryBounds(
  preset: RangePreset,
  customFrom: string,
  customTo: string,
): { fromUtc: string; toUtc: string } | null {
  const now = new Date();
  if (preset === "custom") {
    const from = new Date(customFrom);
    const to = new Date(customTo);
    if (!customFrom || !customTo || !Number.isFinite(from.getTime()) || !Number.isFinite(to.getTime()) || from >= to) {
      return null;
    }
    return { fromUtc: from.toISOString(), toUtc: to.toISOString() };
  }
  const durationMs = preset === "hour" ? 60 * 60_000 : preset === "week" ? 7 * 24 * 60 * 60_000 : 24 * 60 * 60_000;
  return {
    fromUtc: new Date(now.getTime() - durationMs).toISOString(),
    toUtc: now.toISOString(),
  };
}

function eventTime(value: string): string {
  const parsed = new Date(value);
  return Number.isFinite(parsed.getTime()) ? parsed.toLocaleString() : value;
}

function eventKindLabel(event: EventReviewRow): string {
  return event.kind === "motion_started" ? "Motion detected" : "Motion ended";
}

export function EventReviewScreen() {
  const initialNow = useMemo(() => new Date(), []);
  const [cameras, setCameras] = useState<CameraSummary[]>([]);
  const [cameraId, setCameraId] = useState("all");
  const [eventKind, setEventKind] = useState<EventKindFilter>("all");
  const [rangePreset, setRangePreset] = useState<RangePreset>("day");
  const [customFrom, setCustomFrom] = useState(() => dateTimeLocalValue(new Date(initialNow.getTime() - 24 * 60 * 60_000)));
  const [customTo, setCustomTo] = useState(() => dateTimeLocalValue(initialNow));
  const [rows, setRows] = useState<EventReviewRow[]>([]);
  const [nextCursor, setNextCursor] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [selected, setSelected] = useState<EventReviewRow | null>(null);
  const [recordingContext, setRecordingContext] = useState<EventRecordingContext | null>(null);
  const [recordingLoading, setRecordingLoading] = useState(false);
  const [playback, setPlayback] = useState<EventPlaybackOpenDto | null>(null);
  const [playbackLoading, setPlaybackLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [selectionMessage, setSelectionMessage] = useState<string | null>(null);

  const datasetGenerationRef = useRef(0);
  const currentDatasetRef = useRef<EventDatasetSnapshot | null>(null);
  const selectionGenerationRef = useRef(0);
  const reloadInFlightRef = useRef(false);
  const queuedReloadRef = useRef<EventDatasetSnapshot | null>(null);
  const paginationInFlightRef = useRef<number | null>(null);
  const playbackSessionRef = useRef<string | null>(null);
  const videoRef = useRef<HTMLVideoElement | null>(null);

  const closePlayback = useCallback(async () => {
    const sessionId = playbackSessionRef.current;
    playbackSessionRef.current = null;
    setPlayback(null);
    if (!sessionId || !isTauri()) return;
    try {
      await invokeDesktop<void>("playback_close", { sessionId });
    } catch {
      // Cleanup is best effort; an expired session is already closed.
    }
  }, []);

  const buildDatasetSnapshot = useCallback((generation: number): EventDatasetSnapshot | null => {
    const bounds = queryBounds(rangePreset, customFrom, customTo);
    if (!bounds) return null;
    return {
      generation,
      query: {
        camera_ids: cameraId === "all" ? [] : [cameraId],
        kind: eventKind === "all" ? null : eventKind,
        from_utc: bounds.fromUtc,
        to_utc: bounds.toUtc,
        limit: PAGE_SIZE,
      },
    };
  }, [cameraId, customFrom, customTo, eventKind, rangePreset]);

  const executeReload = useCallback(async () => {
    if (reloadInFlightRef.current || !isTauri()) return;
    reloadInFlightRef.current = true;
    try {
      while (queuedReloadRef.current) {
        const current = queuedReloadRef.current;
        queuedReloadRef.current = null;
        if (current.generation === datasetGenerationRef.current) {
          setLoading(true);
          setError(null);
        }
        try {
          const page = await invokeDesktop<EventReviewPage>("event_query", {
            input: { ...current.query, cursor: null },
          });
          if (current.generation !== datasetGenerationRef.current) continue;
          setRows(page.rows);
          setNextCursor(page.next_cursor);
        } catch (cause) {
          if (current.generation !== datasetGenerationRef.current) continue;
          setRows([]);
          setNextCursor(null);
          setError(desktopError(cause).message);
        }
      }
    } finally {
      reloadInFlightRef.current = false;
      setLoading(false);
    }
  }, []);

  const queueRootReload = useCallback(() => {
    const generation = ++datasetGenerationRef.current;
    const dataset = buildDatasetSnapshot(generation);
    setLoadingMore(false);
    if (!dataset) {
      currentDatasetRef.current = null;
      queuedReloadRef.current = null;
      setLoading(false);
      setError("Choose a valid custom time range.");
      setRows([]);
      setNextCursor(null);
      return;
    }
    currentDatasetRef.current = dataset;
    queuedReloadRef.current = dataset;
    setLoading(true);
    setError(null);
    void executeReload();
  }, [buildDatasetSnapshot, executeReload]);

  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    void invokeDesktop<CameraSummary[]>("camera_list")
      .then((value) => {
        if (!cancelled) setCameras(value);
      })
      .catch((cause) => {
        if (!cancelled) setError(desktopError(cause).message);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    selectionGenerationRef.current += 1;
    setSelected(null);
    setRecordingContext(null);
    setSelectionMessage(null);
    void closePlayback();
    queueRootReload();
  }, [cameraId, closePlayback, customFrom, customTo, eventKind, queueRootReload, rangePreset]);

  useEffect(() => {
    if (!isTauri()) return;
    // Polling intentionally creates a new root dataset and resets pagination.
    // Mixing a refreshed page 1 with an older cursor snapshot is never allowed.
    const timer = setInterval(() => queueRootReload(), REFRESH_INTERVAL_MS);
    return () => clearInterval(timer);
  }, [queueRootReload]);

  useEffect(() => () => {
    datasetGenerationRef.current += 1;
    currentDatasetRef.current = null;
    selectionGenerationRef.current += 1;
    const sessionId = playbackSessionRef.current;
    if (sessionId && isTauri()) {
      void invokeDesktop<void>("playback_close", { sessionId }).catch(() => undefined);
    }
  }, []);

  useEffect(() => {
    const sessionId = playback?.playback.session_id;
    if (!sessionId || !isTauri()) return;
    let cancelled = false;
    let inFlight = false;
    const timer = setInterval(() => {
      if (cancelled || inFlight || playbackSessionRef.current !== sessionId) return;
      inFlight = true;
      void invokeDesktop<void>("playback_keepalive", { sessionId })
        .catch((cause) => {
          if (cancelled || playbackSessionRef.current !== sessionId) return;
          if (desktopError(cause).code === "playback_session_expired") {
            playbackSessionRef.current = null;
            setPlayback(null);
            setSelectionMessage("Playback session expired. Reopen this event recording.");
          }
        })
        .finally(() => {
          inFlight = false;
        });
    }, PLAYBACK_KEEPALIVE_INTERVAL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [playback?.playback.session_id]);

  const loadMore = useCallback(async () => {
    if (!isTauri() || !nextCursor || loading || loadingMore || paginationInFlightRef.current !== null) return;
    const dataset = currentDatasetRef.current;
    if (!dataset || dataset.generation !== datasetGenerationRef.current) return;
    const generation = dataset.generation;
    const cursor = nextCursor;
    paginationInFlightRef.current = generation;
    setLoadingMore(true);
    setError(null);
    try {
      const page = await invokeDesktop<EventReviewPage>("event_query", {
        input: { ...dataset.query, cursor },
      });
      if (generation !== datasetGenerationRef.current) return;
      setRows((current) => {
        const known = new Set(current.map((row) => row.event_id));
        return [...current, ...page.rows.filter((row) => !known.has(row.event_id))];
      });
      setNextCursor(page.next_cursor);
    } catch (cause) {
      if (generation === datasetGenerationRef.current) setError(desktopError(cause).message);
    } finally {
      if (paginationInFlightRef.current === generation) paginationInFlightRef.current = null;
      if (generation === datasetGenerationRef.current) setLoadingMore(false);
    }
  }, [loading, loadingMore, nextCursor]);

  const selectEvent = useCallback(async (event: EventReviewRow) => {
    const generation = ++selectionGenerationRef.current;
    setSelected(event);
    setRecordingContext(null);
    setSelectionMessage(null);
    setRecordingLoading(true);
    await closePlayback();
    if (!isTauri()) return;
    try {
      const context = await invokeDesktop<EventRecordingContext>("event_recording_context", { eventId: event.event_id });
      if (generation !== selectionGenerationRef.current) return;
      setRecordingContext(context);
    } catch (cause) {
      if (generation !== selectionGenerationRef.current) return;
      const failure = desktopError(cause);
      if (failure.code === "event_not_found") {
        setSelectionMessage("This event is no longer available.");
      } else {
        setSelectionMessage(failure.message);
      }
    } finally {
      if (generation === selectionGenerationRef.current) setRecordingLoading(false);
    }
  }, [closePlayback]);

  useEffect(() => {
    if (!selected || recordingContext?.available || !isTauri()) return;
    const refreshed = rows.find((row) => row.event_id === selected.event_id);
    if (!refreshed?.recording_available) return;
    const generation = selectionGenerationRef.current;
    let cancelled = false;
    void invokeDesktop<EventRecordingContext>("event_recording_context", { eventId: selected.event_id })
      .then((context) => {
        if (!cancelled && generation === selectionGenerationRef.current) {
          setRecordingContext(context);
        }
      })
      .catch((cause) => {
        if (!cancelled && generation === selectionGenerationRef.current) {
          setSelectionMessage(desktopError(cause).message);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [recordingContext?.available, rows, selected]);

  const openEventPlayback = useCallback(async (clipIndex = 0) => {
    if (!selected || playbackLoading || !isTauri()) return;
    const eventId = selected.event_id;
    const generation = selectionGenerationRef.current;
    setPlaybackLoading(true);
    setSelectionMessage(null);
    await closePlayback();
    try {
      const opened = await invokeDesktop<EventPlaybackOpenDto | null>("event_playback_open", { eventId, clipIndex });
      if (generation !== selectionGenerationRef.current || selected.event_id !== eventId) {
        if (opened) {
          void invokeDesktop<void>("playback_close", { sessionId: opened.playback.session_id }).catch(() => undefined);
        }
        return;
      }
      if (!opened) {
        setRecordingContext({ available: false, camera_id: selected.camera_id, seek_offset_ms: null, clip_count: 0 });
        return;
      }
      playbackSessionRef.current = opened.playback.session_id;
      setPlayback(opened);
      setRecordingContext({
        available: true,
        camera_id: opened.playback.recording.camera_id,
        seek_offset_ms: opened.seek_offset_ms,
        clip_count: opened.clip_count,
      });
    } catch (cause) {
      if (generation !== selectionGenerationRef.current) return;
      const failure = desktopError(cause);
      setSelectionMessage(failure.code === "event_not_found" ? "This event is no longer available." : failure.message);
    } finally {
      if (generation === selectionGenerationRef.current) setPlaybackLoading(false);
    }
  }, [closePlayback, playbackLoading, selected]);

  const selectedPlayback = playback?.playback ?? null;

  if (!isTauri()) {
    return (
      <div className="empty-state">
        <h2>Event Review</h2>
        <p className="muted">Event history is available in the desktop app.</p>
      </div>
    );
  }

  return (
    <section className="screen-stack event-review-screen">
      <div className="screen-toolbar">
        <div>
          <h2>Event Review</h2>
          <p className="muted">Persisted motion history. Reviewing an event never starts recording.</p>
        </div>
        <button type="button" disabled={loading} onClick={queueRootReload}>
          {loading ? "Refreshing…" : "Refresh"}
        </button>
      </div>

      {error && <p className="error-banner" role="alert">{error}</p>}

      <div className="panel event-review-filters">
        <label>
          Camera
          <select value={cameraId} onChange={(event) => setCameraId(event.target.value)}>
            <option value="all">All cameras</option>
            {cameras.map((camera) => (
              <option key={camera.camera_id} value={camera.camera_id}>{camera.display_name}</option>
            ))}
          </select>
        </label>
        <label>
          Event type
          <select value={eventKind} onChange={(event) => setEventKind(event.target.value as EventKindFilter)}>
            <option value="all">All motion events</option>
            <option value="motion_started">Motion started</option>
            <option value="motion_ended">Motion ended</option>
          </select>
        </label>
        <label>
          Time range
          <select value={rangePreset} onChange={(event) => setRangePreset(event.target.value as RangePreset)}>
            <option value="hour">Last hour</option>
            <option value="day">Last 24 hours</option>
            <option value="week">Last 7 days</option>
            <option value="custom">Custom range</option>
          </select>
        </label>
        {rangePreset === "custom" && (
          <>
            <label>
              From
              <input type="datetime-local" value={customFrom} onChange={(event) => setCustomFrom(event.target.value)} />
            </label>
            <label>
              To
              <input type="datetime-local" value={customTo} onChange={(event) => setCustomTo(event.target.value)} />
            </label>
          </>
        )}
      </div>

      <div className="event-review-layout">
        <div className="panel event-list-panel">
          <div className="panel-heading">
            <div>
              <h3>Events</h3>
              <p className="muted">{rows.length} loaded</p>
            </div>
            {loading && <span className="muted">Loading…</span>}
          </div>
          {rows.length === 0 && !loading ? (
            <div className="camera-placeholder">No motion events in this range.</div>
          ) : (
            <ul className="event-list" aria-label="Motion events">
              {rows.map((event) => (
                <li key={event.event_id}>
                  <button
                    type="button"
                    className={`event-row${selected?.event_id === event.event_id ? " selected" : ""}`}
                    onClick={() => void selectEvent(event)}
                  >
                    <span className="event-row-time">{eventTime(event.received_time_utc)}</span>
                    <strong>{event.camera_display_name}</strong>
                    <span>{eventKindLabel(event)}</span>
                    <small>{event.recording_available ? "Recording available" : "No recording"}</small>
                  </button>
                </li>
              ))}
            </ul>
          )}
          {nextCursor && (
            <button type="button" disabled={loading || loadingMore} onClick={() => void loadMore()}>
              {loadingMore ? "Loading…" : "Load more"}
            </button>
          )}
        </div>

        <div className="panel event-detail-panel">
          <div className="panel-heading">
            <div>
              <h3>Event details</h3>
              <p className="muted">{selected ? `Event ${selected.event_id}` : "Select an event."}</p>
            </div>
          </div>
          {selected ? (
            <>
              <dl className="event-detail-grid">
                <div><dt>Camera</dt><dd>{selected.camera_display_name}</dd></div>
                <div><dt>Event</dt><dd>{eventKindLabel(selected)}</dd></div>
                <div><dt>Received</dt><dd>{eventTime(selected.received_time_utc)}</dd></div>
                {selected.device_time_utc && <div><dt>Device time</dt><dd>{eventTime(selected.device_time_utc)}</dd></div>}
              </dl>
              {recordingLoading ? (
                <p className="muted">Checking recording availability…</p>
              ) : recordingContext?.available ? (
                <div className="button-row">
                  <button
                    type="button"
                    className="primary-button"
                    disabled={playbackLoading}
                    onClick={() => void openEventPlayback(0)}
                  >
                    {playbackLoading ? "Preparing…" : selectedPlayback ? "Reopen recording" : "Open recording"}
                  </button>
                  <span className="muted">Dedicated event clips include about 5 seconds of real pre-roll before motion.</span>
                </div>
              ) : (
                <p className="muted">No recording available yet. Motion-triggered clips appear here after the segment is finalized.</p>
              )}
              {selectionMessage && <p className="warning-message" role="status">{selectionMessage}</p>}
            </>
          ) : (
            <div className="camera-placeholder">Select an event to inspect recording context.</div>
          )}

          {playback && (
            <div className="event-playback">
              <video
                ref={videoRef}
                key={playback.playback.session_id}
                className="playback-video"
                src={playback.playback.url}
                controls
                preload="metadata"
                onLoadedMetadata={() => {
                  if (videoRef.current) videoRef.current.currentTime = playback.seek_offset_ms / 1000;
                }}
                onError={() => {
                  setSelectionMessage("Playback could not be loaded by the desktop webview.");
                  void closePlayback();
                }}
              />
              <div className="playback-meta">
                <span>{playback.playback.inspect.video_codec.toUpperCase()}</span>
                <span>{playback.playback.inspect.audio_available ? "Audio available" : "Video only"}</span>
                <span>Event clip pre-roll included</span>
                <span>Clip {playback.clip_index + 1} of {playback.clip_count}</span>
              </div>
              {playback.clip_count > 1 && (
                <div className="button-row">
                  <button
                    type="button"
                    disabled={playbackLoading || playback.clip_index === 0}
                    onClick={() => void openEventPlayback(playback.clip_index - 1)}
                  >
                    Previous clip
                  </button>
                  <button
                    type="button"
                    disabled={playbackLoading || playback.clip_index + 1 >= playback.clip_count}
                    onClick={() => void openEventPlayback(playback.clip_index + 1)}
                  >
                    Next clip
                  </button>
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}
