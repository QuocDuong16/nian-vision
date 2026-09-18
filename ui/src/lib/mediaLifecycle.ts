export interface LiveMseResources {
  mediaSource: MediaSource;
  sourceBuffer: SourceBuffer | null;
  video: HTMLVideoElement;
  objectUrl: string;
}

/**
 * Best-effort teardown for one live MSE presentation.
 *
 * Transport cancellation happens outside this helper. Every browser-owned
 * resource is released independently so one partially-transitioned MSE object
 * cannot prevent the remaining cleanup from running.
 */
export function teardownLiveMse({ mediaSource, sourceBuffer, video, objectUrl }: LiveMseResources): void {
  try {
    video.pause();
  } catch {
    // Detaching the media element below still releases its resource graph.
  }

  if (sourceBuffer && mediaSource.readyState === "open") {
    if (sourceBuffer.updating) {
      try {
        sourceBuffer.abort();
      } catch {
        // Continue with SourceBuffer removal even if abort races MSE state.
      }
    }
    if (mediaSource.sourceBuffers.length > 0) {
      try {
        mediaSource.removeSourceBuffer(sourceBuffer);
      } catch {
        // The element detachment remains authoritative if removal races state.
      }
    }
  }

  if (mediaSource.readyState === "open") {
    try {
      mediaSource.endOfStream();
    } catch {
      // A concurrently transitioning MediaSource can reject endOfStream().
    }
  }

  try {
    video.removeAttribute("src");
    video.load();
  } catch {
    // Revoking the object URL is still useful if element reset is unavailable.
  }
  try {
    URL.revokeObjectURL(objectUrl);
  } catch {
    // Teardown is idempotent/best-effort across browser implementations.
  }
}
