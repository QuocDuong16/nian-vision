export interface FrontendMediaDiagnostics {
  mountedVideoElements: number;
  activeMseSessions: number;
  bufferedSeconds: number;
}

let activeMseSessions = 0;

export function retainMseSession(): () => void {
  activeMseSessions += 1;
  let released = false;
  return () => {
    if (released) return;
    released = true;
    activeMseSessions = Math.max(0, activeMseSessions - 1);
  };
}

function bufferedDuration(video: HTMLVideoElement): number {
  try {
    let total = 0;
    for (let index = 0; index < video.buffered.length; index += 1) {
      const start = video.buffered.start(index);
      const end = video.buffered.end(index);
      if (Number.isFinite(start) && Number.isFinite(end) && end >= start) {
        total += end - start;
      }
    }
    return total;
  } catch {
    return 0;
  }
}

export function readFrontendMediaDiagnostics(root: ParentNode = document): FrontendMediaDiagnostics {
  const videos = Array.from(root.querySelectorAll("video"));
  const bufferedSeconds = videos.reduce((total, node) => {
    return total + bufferedDuration(node as HTMLVideoElement);
  }, 0);
  return {
    mountedVideoElements: videos.length,
    activeMseSessions,
    bufferedSeconds,
  };
}
