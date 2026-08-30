import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  desktopError,
  invokeDesktop,
  isTauri,
  type CameraSummary,
  type PlaybackOpenDto,
  type RecordingDto,
} from "../lib/tauri";

function nextCalendarDay(day: string): string {
  const [year, month, date] = day.split("-").map(Number);
  if (year === undefined || month === undefined || date === undefined) {
    return day;
  }
  const value = new Date(year, month - 1, date + 1);
  const pad = (part: number) => String(part).padStart(2, "0");
  return `${value.getFullYear()}-${pad(value.getMonth() + 1)}-${pad(value.getDate())}`;
}

function clock(value: string): string {
  return value.slice(11, 19);
}

function durationLabel(durationMs: number | null): string {
  if (durationMs === null) return "Duration unknown";
  const seconds = Math.max(0, Math.round(durationMs / 1000));
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  return minutes > 0 ? `${minutes}m ${rest}s` : `${rest}s`;
}

function gapLabel(ms: number): string {
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s gap`;
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  return rest === 0 ? `${minutes}m gap` : `${minutes}m ${rest}s gap`;
}

function knownGap(previous: RecordingDto, current: RecordingDto): number | null {
  if (!previous.end_at) return null;
  const end = new Date(previous.end_at).getTime();
  const start = new Date(current.started_at).getTime();
  const gap = start - end;
  return Number.isFinite(gap) && gap > 1000 ? gap : null;
}

type TimelineLoadOptions = {
  preserveError?: boolean;
  keepSessionForRecordingId?: string | null;
};

const PLAYBACK_KEEPALIVE_INTERVAL_MS = 45_000;
const PLAYBACK_KEEPALIVE_FAILURE_THRESHOLD = 3;
const PLAYBACK_EXPIRED_MESSAGE = "Playback session expired. Reopen the recording.";
const PLAYBACK_KEEPALIVE_WARNING = "Playback session keepalive is unavailable. Reopen the recording if playback stops.";

export function TimelineScreen() {
  const [cameras, setCameras] = useState<CameraSummary[]>([]);
  const [cameraId, setCameraId] = useState("");
  const [days, setDays] = useState<string[]>([]);
  const [day, setDay] = useState("");
  const [recordings, setRecordings] = useState<RecordingDto[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [playback, setPlayback] = useState<PlaybackOpenDto | null>(null);
  const [loading, setLoading] = useState(false);
  const [playbackLoading, setPlaybackLoading] = useState(false);
  const [ended, setEnded] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [playbackError, setPlaybackError] = useState<string | null>(null);
  const sessionRef = useRef<string | null>(null);
  const sessionRecordingRef = useRef<string | null>(null);

  const heartbeatTimerRef = useRef<ReturnType<typeof setInterval> | null>(null);
  const heartbeatFailuresRef = useRef(0);

  const stopPlaybackHeartbeat = useCallback(() => {
    if (heartbeatTimerRef.current !== null) {
      clearInterval(heartbeatTimerRef.current);
      heartbeatTimerRef.current = null;
    }
    heartbeatFailuresRef.current = 0;
  }, []);

  const closePlayback = useCallback(async (clearPlaybackError = true) => {
    stopPlaybackHeartbeat();
    const sessionId = sessionRef.current;
    sessionRef.current = null;
    sessionRecordingRef.current = null;
    setPlayback(null);
    setEnded(false);
    if (clearPlaybackError) setPlaybackError(null);
    if (!sessionId || !isTauri()) return;
    try {
      await invokeDesktop<void>("playback_close", { sessionId });
    } catch {
      // Expired/closed sessions need no UI escalation during cleanup.
    }
  }, [stopPlaybackHeartbeat]);

  const loadTimeline = useCallback(async (
    nextCameraId: string,
    nextDay: string,
    options: TimelineLoadOptions = {},
  ) => {
    if (!isTauri() || !nextCameraId || !nextDay) {
      setRecordings([]);
      return;
    }
    setLoading(true);
    if (!options.preserveError) setError(null);
    try {
      const rows = await invokeDesktop<RecordingDto[]>("recording_timeline", {
        cameraId: nextCameraId,
        start: `${nextDay}T00:00:00`,
        end: `${nextCalendarDay(nextDay)}T00:00:00`,
      });
      setRecordings(rows);
      setSelectedId((current) => (current && rows.some((row) => row.recording_id === current) ? current : null));
      const activeRecordingId = sessionRecordingRef.current;
      const keepSession = options.keepSessionForRecordingId;
      if (
        sessionRef.current
        && activeRecordingId
        && activeRecordingId !== keepSession
        && !rows.some((row) => row.recording_id === activeRecordingId)
      ) {
        void closePlayback();
      }
    } catch (cause) {
      setError(desktopError(cause).message);
      setRecordings([]);
    } finally {
      setLoading(false);
    }
  }, [closePlayback]);

  useEffect(() => {
    if (!isTauri()) return;
    let cancelled = false;
    void invokeDesktop<CameraSummary[]>("camera_list")
      .then((rows) => {
        if (cancelled) return;
        setCameras(rows);
        setCameraId((current) => current || rows[0]?.camera_id || "");
      })
      .catch((cause) => {
        if (!cancelled) setError(desktopError(cause).message);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    if (!isTauri() || !cameraId) {
      setDays([]);
      setDay("");
      setRecordings([]);
      return;
    }
    let cancelled = false;
    void closePlayback();
    setSelectedId(null);
    setError(null);
    void (async () => {
      try {
        await invokeDesktop<void>("recordings_refresh");
        const available = await invokeDesktop<string[]>("recording_days", { cameraId });
        if (cancelled) return;
        setDays(available);
        const latest = available.at(-1) ?? "";
        setDay(latest);
        if (latest) {
          await loadTimeline(cameraId, latest);
        } else {
          setRecordings([]);
        }
      } catch (cause) {
        if (!cancelled) setError(desktopError(cause).message);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [cameraId, closePlayback, loadTimeline]);

  useEffect(() => {
    const sessionId = playback?.session_id;
    if (!sessionId || !isTauri()) {
      stopPlaybackHeartbeat();
      return;
    }

    stopPlaybackHeartbeat();
    let cancelled = false;
    let inFlight = false;
    const heartbeat = async () => {
      if (cancelled || inFlight || sessionRef.current !== sessionId) return;
      inFlight = true;
      try {
        await invokeDesktop<void>("playback_keepalive", { sessionId });
        if (cancelled || sessionRef.current !== sessionId) return;
        heartbeatFailuresRef.current = 0;
        setPlaybackError((current) => current === PLAYBACK_KEEPALIVE_WARNING ? null : current);
      } catch (cause) {
        if (cancelled || sessionRef.current !== sessionId) return;
        const failure = desktopError(cause);
        if (failure.code === "playback_session_expired") {
          stopPlaybackHeartbeat();
          sessionRef.current = null;
          sessionRecordingRef.current = null;
          setPlayback(null);
          setEnded(false);
          setPlaybackError(PLAYBACK_EXPIRED_MESSAGE);
          return;
        }
        heartbeatFailuresRef.current += 1;
        if (heartbeatFailuresRef.current === PLAYBACK_KEEPALIVE_FAILURE_THRESHOLD) {
          setPlaybackError(PLAYBACK_KEEPALIVE_WARNING);
        }
      } finally {
        inFlight = false;
      }
    };

    heartbeatTimerRef.current = setInterval(() => {
      void heartbeat();
    }, PLAYBACK_KEEPALIVE_INTERVAL_MS);

    return () => {
      cancelled = true;
      stopPlaybackHeartbeat();
    };
  }, [playback?.session_id, stopPlaybackHeartbeat]);

  useEffect(() => () => {
    const sessionId = sessionRef.current;
    if (sessionId && isTauri()) {
      void invokeDesktop<void>("playback_close", { sessionId }).catch(() => undefined);
    }
  }, []);

  const selected = useMemo(
    () => recordings.find((recording) => recording.recording_id === selectedId) ?? null,
    [recordings, selectedId],
  );

  const refreshVisible = useCallback(async () => {
    if (!isTauri() || !cameraId) return;
    setError(null);
    try {
      await invokeDesktop<void>("recordings_refresh");
      const available = await invokeDesktop<string[]>("recording_days", { cameraId });
      setDays(available);
      const targetDay = day && available.includes(day) ? day : (available.at(-1) ?? "");
      setDay(targetDay);
      if (targetDay) {
        await loadTimeline(cameraId, targetDay);
      } else {
        setRecordings([]);
        setSelectedId(null);
        await closePlayback();
      }
    } catch (cause) {
      setError(desktopError(cause).message);
    }
  }, [cameraId, closePlayback, day, loadTimeline]);

  const selectDay = useCallback(async (nextDay: string) => {
    if (!nextDay || nextDay === day) return;
    await closePlayback();
    setSelectedId(null);
    setDay(nextDay);
    await loadTimeline(cameraId, nextDay);
  }, [cameraId, closePlayback, day, loadTimeline]);

  const openRecording = useCallback(async (recordingId: string) => {
    if (!isTauri()) return;
    setPlaybackLoading(true);
    setError(null);
    setPlaybackError(null);
    setEnded(false);
    await closePlayback();
    try {
      const opened = await invokeDesktop<PlaybackOpenDto>("playback_open", { recordingId });
      sessionRef.current = opened.session_id;
      sessionRecordingRef.current = opened.recording.recording_id;
      setPlayback(opened);
      setSelectedId(opened.recording.recording_id);
      const openedDay = opened.recording.started_at.slice(0, 10);
      setDays((current) => current.includes(openedDay) ? current : [...current, openedDay].sort());
      setDay(openedDay);
      await loadTimeline(opened.recording.camera_id, openedDay, {
        keepSessionForRecordingId: opened.recording.recording_id,
      });
    } catch (cause) {
      const failure = desktopError(cause);
      setError(failure.message);
      if (["recording_missing", "recording_stale", "recording_not_found"].includes(failure.code)) {
        try {
          await invokeDesktop<void>("recordings_refresh");
        } catch {
          // Preserve the original typed playback failure; refresh is best effort here.
        }
        await loadTimeline(cameraId, day, { preserveError: true });
      }
    } finally {
      setPlaybackLoading(false);
    }
  }, [cameraId, closePlayback, day, loadTimeline]);

  const handlePlaybackMediaError = useCallback(() => {
    stopPlaybackHeartbeat();
    const sessionId = sessionRef.current;
    sessionRef.current = null;
    sessionRecordingRef.current = null;
    setPlayback(null);
    setEnded(false);
    setPlaybackError("Playback could not be loaded or decoded by the desktop webview. Reopen the recording to try again.");
    if (sessionId && isTauri()) {
      void invokeDesktop<void>("playback_close", { sessionId }).catch(() => undefined);
    }
  }, [stopPlaybackHeartbeat]);

  const dayIndex = days.indexOf(day);
  const previousDay = dayIndex > 0 ? days[dayIndex - 1] : null;
  const nextDay = dayIndex >= 0 && dayIndex + 1 < days.length ? days[dayIndex + 1] : null;

  if (!isTauri()) {
    return (
      <div className="empty-state">
        <h2>Recordings timeline</h2>
        <p className="muted">Playback is available in the desktop app.</p>
      </div>
    );
  }

  return (
    <section className="screen-stack timeline-screen">
      <div className="screen-toolbar">
        <div>
          <h2>Recordings / Timeline</h2>
          <p className="muted">Finalized local recordings only. Gaps are shown instead of being politely lied about.</p>
        </div>
        <button type="button" disabled={!cameraId || loading} onClick={() => void refreshVisible()}>
          Refresh
        </button>
      </div>

      {error && <p className="error-banner" role="alert">{error}</p>}
      {playbackError && <p className="error-banner" role="alert">{playbackError}</p>}

      <div className="panel timeline-filters">
        <label>
          Camera
          <select value={cameraId} onChange={(event) => setCameraId(event.target.value)}>
            {cameras.map((camera) => (
              <option key={camera.camera_id} value={camera.camera_id}>{camera.display_name}</option>
            ))}
          </select>
        </label>
        <label>
          Recording day
          <select value={day} onChange={(event) => void selectDay(event.target.value)} disabled={days.length === 0}>
            {days.map((availableDay) => <option key={availableDay} value={availableDay}>{availableDay}</option>)}
          </select>
        </label>
        <div className="button-row timeline-day-nav">
          <button type="button" disabled={!previousDay} onClick={() => previousDay && void selectDay(previousDay)}>Previous day</button>
          <button type="button" disabled={!nextDay} onClick={() => nextDay && void selectDay(nextDay)}>Next day</button>
        </div>
      </div>

      {cameras.length === 0 ? (
        <div className="empty-state"><h2>No saved cameras</h2><p className="muted">Add a camera before browsing recordings.</p></div>
      ) : days.length === 0 ? (
        <div className="empty-state"><h2>No recordings yet</h2><p className="muted">This camera has no finalized recording days in the index.</p></div>
      ) : recordings.length === 0 && !loading ? (
        <div className="empty-state"><h2>No recordings on this day</h2><p className="muted">Refresh after recording or reconciliation completes.</p></div>
      ) : (
        <div className="panel">
          <div className="panel-heading">
            <div>
              <h3>{day}</h3>
              <p className="muted">{recordings.length} finalized recording{recordings.length === 1 ? "" : "s"}</p>
            </div>
            {loading && <span className="muted">Loading…</span>}
          </div>
          <div className="recording-timeline" aria-label="Recording timeline">
            {recordings.map((recording, index) => {
              const previous = index > 0 ? recordings[index - 1] : undefined;
              const gap = previous ? knownGap(previous, recording) : null;
              const width = recording.media_duration_ms === null
                ? undefined
                : Math.max(1, Math.min(12, recording.media_duration_ms / 10_000));
              return (
                <div className="timeline-entry" key={recording.recording_id}>
                  {gap !== null && <div className="timeline-gap" aria-label={gapLabel(gap)}>{gapLabel(gap)}</div>}
                  <button
                    type="button"
                    className={`timeline-recording${selectedId === recording.recording_id ? " selected" : ""}${recording.media_duration_ms === null ? " unknown" : ""}`}
                    style={width ? { flexGrow: width } : undefined}
                    onClick={() => setSelectedId(recording.recording_id)}
                  >
                    <strong>{clock(recording.started_at)}</strong>
                    <span>{durationLabel(recording.media_duration_ms)}</span>
                    {recording.kind === "recovered" && <small>Recovered</small>}
                  </button>
                </div>
              );
            })}
          </div>
        </div>
      )}

      <div className="panel playback-panel">
        <div className="panel-heading">
          <div>
            <h3>Playback</h3>
            <p className="muted">
              {selected ? `${clock(selected.started_at)} · ${durationLabel(selected.media_duration_ms)}` : "Select a finalized recording."}
            </p>
          </div>
          <button
            className="primary-button"
            type="button"
            disabled={!selected || playbackLoading}
            onClick={() => selected && void openRecording(selected.recording_id)}
          >
            {playbackLoading ? "Preparing…" : playbackError || (playback !== null && playback.recording.recording_id === selected?.recording_id) ? "Reopen" : "Open recording"}
          </button>
        </div>

        {playback ? (
          <>
            <video
              key={playback.session_id}
              className="playback-video"
              src={playback.url}
              controls
              preload="metadata"
              onEnded={() => setEnded(true)}
              onError={handlePlaybackMediaError}
            />
            <div className="playback-meta">
              <span>{playback.inspect.video_codec.toUpperCase()}</span>
              <span>{playback.inspect.width && playback.inspect.height ? `${playback.inspect.width}×${playback.inspect.height}` : "Resolution unknown"}</span>
              <span>{playback.inspect.audio_available ? "Audio available" : "Video only"}</span>
              <span>{durationLabel(playback.inspect.duration_ms)}</span>
            </div>
            {ended && <p className="warning-message">Recording ended. Use Previous/Next so a real timeline gap is not silently skipped.</p>}
            <div className="button-row playback-navigation">
              <button
                type="button"
                disabled={!playback.adjacent.previous || playbackLoading}
                onClick={() => playback.adjacent.previous && void openRecording(playback.adjacent.previous.recording_id)}
              >Previous recording</button>
              <button
                type="button"
                disabled={!playback.adjacent.next || playbackLoading}
                onClick={() => playback.adjacent.next && void openRecording(playback.adjacent.next.recording_id)}
              >Next recording</button>
            </div>
          </>
        ) : (
          <div className="camera-placeholder">No playback session open</div>
        )}
      </div>
    </section>
  );
}
