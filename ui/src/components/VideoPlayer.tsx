import { lazy, Suspense, useCallback, useEffect, useRef, useState } from "react";

const MediaThemeSutro = lazy(() =>
  import("@player.style/sutro/react").then((module) => ({ default: module.default })),
);

export type VideoMetadataEntry = {
  label: string;
  value: string;
};

type VideoPlayerProps = {
  src: string;
  audioSrc?: string | null;
  ariaLabel?: string;
  initialTimeSeconds?: number;
  /** Clip-relative detection anchor; null means no trustworthy time mapping. */
  markerTimeSeconds?: number | null;
  metadata?: VideoMetadataEntry[];
  onEnded?: () => void;
  onError?: () => void;
};

type ManagedVideoProps = {
  src: string;
  audioSrc: string | null;
  initialTimeSeconds: number;
  markerTimeSeconds: number | null;
  markerJump: number;
  onDuration: (duration: number) => void;
  onEnded: (() => void) | undefined;
  onError: (() => void) | undefined;
  onAudioError: () => void;
};

function ManagedVideo({
  src,
  audioSrc,
  initialTimeSeconds,
  markerTimeSeconds,
  markerJump,
  onDuration,
  onEnded,
  onError,
  onAudioError,
}: ManagedVideoProps) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const audioRef = useRef<HTMLAudioElement>(null);

  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    // Capture the concrete media element while it is mounted. With Suspense,
    // a parent ref can be detached before passive cleanup runs, leaving the
    // Chromium decoder resource attached until its cache reclaims it.
    return () => {
      if (video.getAttribute("src") !== src) return;
      video.pause();
      video.removeAttribute("src");
      video.load();
    };
  }, [src]);

  useEffect(() => {
    if (markerJump === 0 || markerTimeSeconds === null) return;
    const video = videoRef.current;
    if (!video || !Number.isFinite(markerTimeSeconds)) return;
    video.currentTime = Math.min(Math.max(0, markerTimeSeconds), Number.isFinite(video.duration) ? video.duration : markerTimeSeconds);
  }, [markerJump, markerTimeSeconds]);

  useEffect(() => {
    const video = videoRef.current;
    const audio = audioRef.current;
    if (!video || !audio || !audioSrc) return;

    const synchronize = () => {
      audio.volume = video.volume;
      audio.muted = video.muted;
      audio.playbackRate = video.playbackRate;
      // Seeking and browser scheduling can introduce small drift between the
      // native video and PCM sidecar. Only correct meaningful drift so the
      // audio decoder does not continuously seek on every timeupdate event.
      if (Number.isFinite(video.currentTime) && Math.abs(audio.currentTime - video.currentTime) > 0.35) {
        audio.currentTime = video.currentTime;
      }
    };
    const startAudio = () => {
      // "play" fires before video decoding starts. Follow "playing" instead,
      // otherwise slow HEVC startup lets the separate WAV outrun frozen video.
      synchronize();
      void audio.play().catch(onAudioError);
    };
    const stopAudio = () => audio.pause();
    const followVideo = () => {
      synchronize();
      if (video.paused && !audio.paused) audio.pause();
    };
    video.addEventListener("playing", startAudio);
    video.addEventListener("pause", stopAudio);
    video.addEventListener("ended", stopAudio);
    video.addEventListener("waiting", stopAudio);
    video.addEventListener("stalled", stopAudio);
    video.addEventListener("seeking", stopAudio);
    video.addEventListener("seeked", followVideo);
    video.addEventListener("timeupdate", followVideo);
    video.addEventListener("volumechange", synchronize);
    video.addEventListener("ratechange", synchronize);
    synchronize();
    return () => {
      video.removeEventListener("playing", startAudio);
      video.removeEventListener("pause", stopAudio);
      video.removeEventListener("ended", stopAudio);
      video.removeEventListener("waiting", stopAudio);
      video.removeEventListener("stalled", stopAudio);
      video.removeEventListener("seeking", stopAudio);
      video.removeEventListener("seeked", followVideo);
      video.removeEventListener("timeupdate", followVideo);
      video.removeEventListener("volumechange", synchronize);
      video.removeEventListener("ratechange", synchronize);
      if (audio.getAttribute("src") === audioSrc) {
        audio.pause();
        audio.removeAttribute("src");
        audio.load();
      }
    };
  }, [src, audioSrc, onAudioError]);

  return (
    <>
    <video
      ref={videoRef}
      className="playback-video"
      slot="media"
      src={src}
      preload="metadata"
      playsInline
      onLoadedMetadata={(event) => {
        const video = event.currentTarget;
        const nextDuration = Number.isFinite(video.duration) ? video.duration : 0;
        onDuration(nextDuration);
        if (initialTimeSeconds > 0) {
          video.currentTime = Math.min(initialTimeSeconds, nextDuration || initialTimeSeconds);
        }
      }}
      onDurationChange={(event) => {
        onDuration(Number.isFinite(event.currentTarget.duration) ? event.currentTarget.duration : 0);
      }}
      onEnded={() => onEnded?.()}
      onError={() => onError?.()}
    />
    {audioSrc && (
      <audio ref={audioRef} src={audioSrc} preload="metadata" hidden aria-hidden="true" onError={onAudioError} />
    )}
    </>
  );
}

function timeLabel(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "0:00";
  const whole = Math.floor(seconds);
  const hours = Math.floor(whole / 3600);
  const minutes = Math.floor((whole % 3600) / 60);
  const rest = whole % 60;
  return hours > 0
    ? `${hours}:${String(minutes).padStart(2, "0")}:${String(rest).padStart(2, "0")}`
    : `${minutes}:${String(rest).padStart(2, "0")}`;
}

export function VideoPlayer({
  src,
  audioSrc = null,
  ariaLabel = "Video playback",
  initialTimeSeconds = 0,
  markerTimeSeconds = null,
  metadata = [],
  onEnded,
  onError,
}: VideoPlayerProps) {
  const [duration, setDuration] = useState(0);
  const [metadataOpen, setMetadataOpen] = useState(false);
  const [audioError, setAudioError] = useState(false);
  const [markerJump, setMarkerJump] = useState(0);
  const handleAudioError = useCallback(() => setAudioError(true), []);

  useEffect(() => {
    setDuration(0);
    setMetadataOpen(false);
    setAudioError(false);
  }, [src, audioSrc]);

  const markerValid = markerTimeSeconds !== null
    && Number.isFinite(markerTimeSeconds)
    && markerTimeSeconds >= 0
    && duration > 0
    && markerTimeSeconds <= duration;
  const markerStart = markerValid ? Math.max(0, markerTimeSeconds - 3) : 0;
  const markerEnd = markerValid ? Math.min(duration, markerTimeSeconds + 3) : 0;

  return (
    <div className="video-player video-player-sutro" aria-label={ariaLabel}>
      <Suspense fallback={<div className="video-player-loading" role="status">Loading player…</div>}>
        <MediaThemeSutro className="nian-media-theme">
          <ManagedVideo
            src={src}
            audioSrc={audioSrc}
            initialTimeSeconds={initialTimeSeconds}
            markerTimeSeconds={markerTimeSeconds}
            markerJump={markerJump}
            onDuration={setDuration}
            onEnded={onEnded}
            onError={onError}
            onAudioError={handleAudioError}
          />
        </MediaThemeSutro>
      </Suspense>
      {audioError && <p className="warning-message" role="status">G.711 audio could not be played. Video playback remains available.</p>}

      {markerValid && (
        <div className="video-player-motion-context" aria-label="Event detection timeline">
          <div className="video-player-motion-track" role="img" aria-label={`Detection at ${timeLabel(markerTimeSeconds)}, highlighted from ${timeLabel(markerStart)} to ${timeLabel(markerEnd)}`}>
            <span className="video-player-motion-window" style={{ left: `${markerStart / duration * 100}%`, width: `${(markerEnd - markerStart) / duration * 100}%` }} aria-hidden="true" />
            <span className="video-player-motion-marker" style={{ left: `${markerTimeSeconds / duration * 100}%` }} aria-hidden="true" />
          </div>
          <div className="video-player-motion-labels">
            <span>0:00</span>
            <span>Detection ~{timeLabel(markerTimeSeconds)} · ±3s context</span>
            <span>{timeLabel(duration)}</span>
          </div>
        </div>
      )}
      {(initialTimeSeconds > 0 || markerValid || metadata.length > 0) && (
        <div className="video-player-context-bar">
          <div className="video-player-context-copy">
            {markerValid ? (
              <span>Detection around {timeLabel(markerTimeSeconds)}</span>
            ) : initialTimeSeconds > 0 ? (
              <span>Starts at {timeLabel(Math.min(initialTimeSeconds, duration || initialTimeSeconds))}</span>
            ) : null}
            {duration > 0 && <span>{timeLabel(duration)} clip</span>}
          </div>
          {markerValid && <button type="button" onClick={() => setMarkerJump((value) => value + 1)}>Jump to detection</button>}
          {metadata.length > 0 && (
            <button
              type="button"
              aria-expanded={metadataOpen}
              onClick={() => setMetadataOpen((open) => !open)}
            >
              {metadataOpen ? "Hide metadata" : "Metadata"}
            </button>
          )}
        </div>
      )}

      {metadataOpen && metadata.length > 0 && (
        <section className="video-player-metadata-panel" aria-label="Video metadata">
          <div className="video-player-metadata-head">
            <div><strong>Recording metadata</strong><span>Technical details for this clip</span></div>
            <button type="button" aria-label="Close video metadata" onClick={() => setMetadataOpen(false)}>Close</button>
          </div>
          <dl className="video-player-metadata">
            {metadata.map((entry) => (
              <div key={`${entry.label}:${entry.value}`}><dt>{entry.label}</dt><dd>{entry.value}</dd></div>
            ))}
          </dl>
        </section>
      )}
    </div>
  );
}
