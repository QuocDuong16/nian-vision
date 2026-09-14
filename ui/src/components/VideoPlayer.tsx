import { useCallback, useEffect, useMemo, useRef, useState } from "react";

export type VideoMetadataEntry = {
  label: string;
  value: string;
};

type VideoPlayerProps = {
  src: string;
  ariaLabel?: string;
  initialTimeSeconds?: number;
  metadata?: VideoMetadataEntry[];
  onEnded?: () => void;
  onError?: () => void;
};

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
  ariaLabel = "Video playback",
  initialTimeSeconds = 0,
  metadata = [],
  onEnded,
  onError,
}: VideoPlayerProps) {
  const frameRef = useRef<HTMLDivElement>(null);
  const videoRef = useRef<HTMLVideoElement>(null);
  const [playing, setPlaying] = useState(false);
  const [currentTime, setCurrentTime] = useState(0);
  const [duration, setDuration] = useState(0);
  const [muted, setMuted] = useState(false);
  const [volume, setVolume] = useState(1);
  const [fullscreen, setFullscreen] = useState(false);
  const [metadataOpen, setMetadataOpen] = useState(false);

  useEffect(() => {
    setPlaying(false);
    setCurrentTime(0);
    setDuration(0);
    setMetadataOpen(false);
  }, [src]);

  useEffect(() => {
    const update = () => setFullscreen(document.fullscreenElement === frameRef.current);
    document.addEventListener("fullscreenchange", update);
    return () => document.removeEventListener("fullscreenchange", update);
  }, []);

  const togglePlay = useCallback(() => {
    const video = videoRef.current;
    if (!video) return;
    if (video.paused) {
      void video.play().catch(() => undefined);
    } else {
      video.pause();
    }
  }, []);

  const toggleMute = useCallback(() => {
    const video = videoRef.current;
    if (!video) return;
    video.muted = !video.muted;
    setMuted(video.muted);
  }, []);

  const seekRelative = useCallback((delta: number) => {
    const video = videoRef.current;
    if (!video) return;
    const upper = Number.isFinite(video.duration) ? video.duration : Number.POSITIVE_INFINITY;
    video.currentTime = Math.min(upper, Math.max(0, video.currentTime + delta));
    setCurrentTime(video.currentTime);
  }, []);

  const toggleFullscreen = useCallback(async () => {
    const frame = frameRef.current;
    if (!frame) return;
    try {
      if (document.fullscreenElement === frame) {
        await document.exitFullscreen();
      } else if (frame.requestFullscreen) {
        await frame.requestFullscreen();
      }
    } catch {
      // Fullscreen permission/policy failures leave playback otherwise usable.
    }
  }, []);

  const handleKeyboard = useCallback((event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.target instanceof HTMLInputElement) return;
    if (event.key === " " || event.key.toLowerCase() === "k") {
      event.preventDefault();
      togglePlay();
    } else if (event.key === "ArrowLeft") {
      event.preventDefault();
      seekRelative(-5);
    } else if (event.key === "ArrowRight") {
      event.preventDefault();
      seekRelative(5);
    } else if (event.key.toLowerCase() === "m") {
      event.preventDefault();
      toggleMute();
    } else if (event.key.toLowerCase() === "f") {
      event.preventDefault();
      void toggleFullscreen();
    }
  }, [seekRelative, toggleFullscreen, toggleMute, togglePlay]);

  const volumeIcon = useMemo(() => muted || volume === 0 ? "Muted" : "Volume", [muted, volume]);

  return (
    <div
      ref={frameRef}
      className={`video-player${playing ? " is-playing" : ""}${fullscreen ? " is-fullscreen" : ""}`}
      aria-label={ariaLabel}
      tabIndex={0}
      onKeyDown={handleKeyboard}
    >
      <div className="video-player-stage" onDoubleClick={() => void toggleFullscreen()}>
        <video
          ref={videoRef}
          className="playback-video video-player-media"
          src={src}
          preload="metadata"
          playsInline
          onLoadedMetadata={(event) => {
            const video = event.currentTarget;
            const nextDuration = Number.isFinite(video.duration) ? video.duration : 0;
            setDuration(nextDuration);
            if (initialTimeSeconds > 0) {
              video.currentTime = Math.min(initialTimeSeconds, nextDuration || initialTimeSeconds);
              setCurrentTime(video.currentTime);
            }
            video.muted = muted;
            video.volume = volume;
          }}
          onTimeUpdate={(event) => setCurrentTime(event.currentTarget.currentTime)}
          onDurationChange={(event) => setDuration(Number.isFinite(event.currentTarget.duration) ? event.currentTarget.duration : 0)}
          onPlay={() => setPlaying(true)}
          onPause={() => setPlaying(false)}
          onEnded={() => {
            setPlaying(false);
            onEnded?.();
          }}
          onVolumeChange={(event) => {
            setMuted(event.currentTarget.muted);
            setVolume(event.currentTarget.volume);
          }}
          onError={() => onError?.()}
          onClick={togglePlay}
        />
        <button className="video-player-center-action" type="button" aria-label={playing ? "Pause video" : "Play video"} onClick={togglePlay}>
          <span aria-hidden="true">{playing ? "Ⅱ" : "▶"}</span>
        </button>
        {metadataOpen && metadata.length > 0 && (
          <dl className="video-player-metadata" aria-label="Video metadata">
            {metadata.map((entry) => (
              <div key={`${entry.label}:${entry.value}`}><dt>{entry.label}</dt><dd>{entry.value}</dd></div>
            ))}
          </dl>
        )}
      </div>

      <div className="video-player-controls">
        <button type="button" aria-label={playing ? "Pause video" : "Play video"} title={playing ? "Pause (Space)" : "Play (Space)"} onClick={togglePlay}>
          <span aria-hidden="true">{playing ? "Ⅱ" : "▶"}</span>
        </button>
        <span className="video-player-time">{timeLabel(currentTime)}</span>
        <input
          className="video-player-seek"
          type="range"
          aria-label="Video position"
          min={0}
          max={Math.max(duration, 0)}
          step="0.1"
          value={Math.min(Math.max(currentTime, 0), Math.max(duration, 0))}
          onChange={(event) => {
            const next = Number(event.target.value);
            const video = videoRef.current;
            if (video && Number.isFinite(next)) video.currentTime = next;
            setCurrentTime(next);
          }}
        />
        <span className="video-player-time">{timeLabel(duration)}</span>
        <button type="button" aria-label={muted ? "Unmute video" : "Mute video"} title="Mute (M)" onClick={toggleMute}>
          <span aria-hidden="true">{muted ? "×" : "◖"}</span>
        </button>
        <input
          className="video-player-volume"
          type="range"
          aria-label="Video volume"
          min={0}
          max={1}
          step="0.05"
          value={muted ? 0 : volume}
          title={volumeIcon}
          onChange={(event) => {
            const next = Number(event.target.value);
            const video = videoRef.current;
            if (!video || !Number.isFinite(next)) return;
            video.volume = next;
            video.muted = next === 0;
            setVolume(next);
            setMuted(next === 0);
          }}
        />
        {metadata.length > 0 && (
          <button type="button" aria-pressed={metadataOpen} aria-label="Video metadata" title="Metadata" onClick={() => setMetadataOpen((open) => !open)}>
            i
          </button>
        )}
        <button type="button" aria-label={fullscreen ? "Exit fullscreen" : "Enter fullscreen"} title="Fullscreen (F)" onClick={() => void toggleFullscreen()}>
          <span aria-hidden="true">{fullscreen ? "↙" : "⛶"}</span>
        </button>
      </div>
    </div>
  );
}
