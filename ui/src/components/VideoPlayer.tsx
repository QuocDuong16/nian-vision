import { lazy, Suspense, useEffect, useRef, useState } from "react";

const MediaThemeSutro = lazy(() =>
  import("@player.style/sutro/react").then((module) => ({ default: module.default })),
);

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
  const videoRef = useRef<HTMLVideoElement>(null);
  const [duration, setDuration] = useState(0);
  const [metadataOpen, setMetadataOpen] = useState(false);

  useEffect(() => {
    setDuration(0);
    setMetadataOpen(false);
  }, [src]);

  return (
    <div className="video-player video-player-sutro" aria-label={ariaLabel}>
      <Suspense fallback={<div className="video-player-loading" role="status">Loading player…</div>}>
        <MediaThemeSutro className="nian-media-theme">
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
            setDuration(nextDuration);
            if (initialTimeSeconds > 0) {
              video.currentTime = Math.min(initialTimeSeconds, nextDuration || initialTimeSeconds);
            }
          }}
          onDurationChange={(event) => {
            setDuration(Number.isFinite(event.currentTarget.duration) ? event.currentTarget.duration : 0);
          }}
          onEnded={() => onEnded?.()}
          onError={() => onError?.()}
          />
        </MediaThemeSutro>
      </Suspense>

      {(initialTimeSeconds > 0 || metadata.length > 0) && (
        <div className="video-player-context-bar">
          <div className="video-player-context-copy">
            {initialTimeSeconds > 0 && (
              <span>Motion begins at {timeLabel(Math.min(initialTimeSeconds, duration || initialTimeSeconds))}</span>
            )}
            {duration > 0 && <span>{timeLabel(duration)} clip</span>}
          </div>
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
