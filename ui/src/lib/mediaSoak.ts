import type { FrontendMediaDiagnostics } from "./mediaTelemetry";
import type { CameraMediaDiagnostics, PerformanceSnapshot } from "./tauri";

export const MEDIA_SOAK_SAMPLE_INTERVAL_MS = 5_000;
export const MEDIA_SOAK_MAX_SAMPLES = 1_440; // 2 hours at five-second cadence.
export const MEDIA_SOAK_SNAPSHOT_MAX_AGE_MS = 2_500;
export const MEDIA_SOAK_CHECKPOINT_MAX_LATENESS_MS = 10_000;

export interface MediaSoakSample {
  capturedAtMs: number;
  appMemoryBytes: number;
  mediaWorkerMemoryBytes: number;
  appCpuPercent: number;
  systemNetworkRxBps: number;
  mountedVideoElements: number;
  activeMseSessions: number;
  bufferedSeconds: number;
  packetBufferBytes: number | null;
  generationStarts: number | null;
}

export interface MediaSoakSummary {
  idleRecovery: MediaSoakIdleRecovery | null;
  sampleCount: number;
  durationSeconds: number;
  coldBaselineAppMemoryBytes: number;
  warmBaselineAppMemoryBytes: number | null;
  baselineAppMemoryBytes: number;
  currentAppMemoryBytes: number;
  peakAppMemoryBytes: number;
  peakMediaWorkerMemoryBytes: number;
  peakAppCpuPercent: number;
  peakPacketBufferBytes: number | null;
  peakSystemNetworkRxBps: number;
  peakMountedVideoElements: number;
  peakActiveMseSessions: number;
  peakBufferedSeconds: number;
  generationStartsDelta: number | null;
  mediaUnavailableSamples: number;
  maxSampleGapSeconds: number;
  memoryTrendBytesPerHour: number;
  recentMemorySpanBytes: number;
  recentWindowSeconds: number;
}

export interface MediaSoakIdleRecovery {
  markedAtMs: number;
  elapsedSinceMarkSeconds: number;
  memoryAtMarkBytes: number;
  memoryAfter30sBytes: number | null;
  memoryAfter120sBytes: number | null;
  deltaFromBaselineAfter30sBytes: number | null;
  deltaFromBaselineAfter120sBytes: number | null;
}

export interface MediaSoakEvidence {
  schemaVersion: 1;
  exportedAtMs: number;
  sampleIntervalMs: number;
  maxSamples: number;
  idleMarkedAtMs: number | null;
  warmBaselineMarkedAtMs: number | null;
  missedPerformanceSamples: number;
  summary: MediaSoakSummary | null;
  samples: MediaSoakSample[];
}

const RECENT_MEMORY_WINDOW_MS = 5 * 60 * 1_000;

export function isFreshMediaSoakSnapshot(
  snapshot: PerformanceSnapshot | null,
  updatedAtMs: number | null,
  nowMs: number,
): boolean {
  if (!snapshot?.sample_ready || updatedAtMs === null) return false;
  const age = nowMs - updatedAtMs;
  return age >= 0 && age <= MEDIA_SOAK_SNAPSHOT_MAX_AGE_MS;
}

function linearRatePerHour(samples: MediaSoakSample[], value: (sample: MediaSoakSample) => number): number {
  if (samples.length < 2) return 0;
  const origin = samples[0]!.capturedAtMs;
  const points = samples.map((sample) => ({
    x: (sample.capturedAtMs - origin) / 3_600_000,
    y: value(sample),
  }));
  const meanX = points.reduce((total, point) => total + point.x, 0) / points.length;
  const meanY = points.reduce((total, point) => total + point.y, 0) / points.length;
  let numerator = 0;
  let denominator = 0;
  for (const point of points) {
    const dx = point.x - meanX;
    numerator += dx * (point.y - meanY);
    denominator += dx * dx;
  }
  if (denominator <= Number.EPSILON) return 0;
  return Math.round(numerator / denominator);
}

export function createMediaSoakSample(
  snapshot: PerformanceSnapshot,
  frontend: FrontendMediaDiagnostics,
  media: CameraMediaDiagnostics[] | null,
  capturedAtMs = Date.now(),
): MediaSoakSample {
  const mediaWorkerMemoryBytes = snapshot.processes.reduce((total, process) => {
    return process.role === "Media worker" ? total + process.memory_bytes : total;
  }, 0);
  const packetBufferBytes = media?.reduce((total, camera) => {
    return total + camera.queued_bytes + camera.pre_roll_bytes;
  }, 0) ?? null;
  const generationStarts = media?.reduce((total, camera) => total + camera.generation_starts, 0) ?? null;

  return {
    capturedAtMs,
    appMemoryBytes: snapshot.app_memory_bytes,
    mediaWorkerMemoryBytes,
    appCpuPercent: snapshot.app_cpu_percent,
    systemNetworkRxBps: snapshot.system_network_rx_bps,
    mountedVideoElements: frontend.mountedVideoElements,
    activeMseSessions: frontend.activeMseSessions,
    bufferedSeconds: frontend.bufferedSeconds,
    packetBufferBytes,
    generationStarts,
  };
}

export function appendMediaSoakSample(
  samples: MediaSoakSample[],
  sample: MediaSoakSample,
): MediaSoakSample[] {
  if (samples.length >= MEDIA_SOAK_MAX_SAMPLES) return samples;
  return [...samples, sample];
}

function sampleAtMarker(samples: MediaSoakSample[], markedAtMs: number | null): MediaSoakSample | null {
  if (markedAtMs === null) return null;
  for (let index = samples.length - 1; index >= 0; index -= 1) {
    const sample = samples[index]!;
    if (sample.capturedAtMs <= markedAtMs) return sample;
  }
  return null; // Never silently substitute a different sample for a lost marker.
}

function idleRecoverySummary(
  samples: MediaSoakSample[],
  baselineAppMemoryBytes: number,
  idleMarkedAtMs: number | null,
): MediaSoakIdleRecovery | null {
  if (idleMarkedAtMs === null) return null;
  const last = samples.at(-1);
  if (!last) return null;
  const atMark = sampleAtMarker(samples, idleMarkedAtMs);
  if (!atMark) return null;

  // An hours-late sample is not evidence of recovery at +30s or +120s.
  const after30s = samples.find((sample) => sample.capturedAtMs >= idleMarkedAtMs + 30_000
    && sample.capturedAtMs <= idleMarkedAtMs + 30_000 + MEDIA_SOAK_CHECKPOINT_MAX_LATENESS_MS) ?? null;
  const after120s = samples.find((sample) => sample.capturedAtMs >= idleMarkedAtMs + 120_000
    && sample.capturedAtMs <= idleMarkedAtMs + 120_000 + MEDIA_SOAK_CHECKPOINT_MAX_LATENESS_MS) ?? null;
  return {
    markedAtMs: idleMarkedAtMs,
    elapsedSinceMarkSeconds: Math.max(0, (last.capturedAtMs - idleMarkedAtMs) / 1_000),
    memoryAtMarkBytes: atMark.appMemoryBytes,
    memoryAfter30sBytes: after30s?.appMemoryBytes ?? null,
    memoryAfter120sBytes: after120s?.appMemoryBytes ?? null,
    deltaFromBaselineAfter30sBytes: after30s
      ? after30s.appMemoryBytes - baselineAppMemoryBytes
      : null,
    deltaFromBaselineAfter120sBytes: after120s
      ? after120s.appMemoryBytes - baselineAppMemoryBytes
      : null,
  };
}

export function summarizeMediaSoak(
  samples: MediaSoakSample[],
  idleMarkedAtMs: number | null = null,
  warmBaselineMarkedAtMs: number | null = null,
): MediaSoakSummary | null {
  const first = samples[0];
  const last = samples[samples.length - 1];
  if (!first || !last) return null;

  const warmBaseline = sampleAtMarker(samples, warmBaselineMarkedAtMs);
  const baselineAppMemoryBytes = warmBaseline?.appMemoryBytes ?? first.appMemoryBytes;
  const recentStart = Math.max(first.capturedAtMs, last.capturedAtMs - RECENT_MEMORY_WINDOW_MS);
  const recent = samples.filter((sample) => sample.capturedAtMs >= recentStart);
  const recentMemoryValues = recent.map((sample) => sample.appMemoryBytes);
  const validMedia = samples.filter((sample) => sample.packetBufferBytes !== null);
  const generations = samples.map((sample) => sample.generationStarts);
  const generationsContinuous = generations.every((count, index) => count !== null
    && (index === 0 || (generations[index - 1] !== null && count >= generations[index - 1]!)));
  let maxSampleGapMs = 0;
  for (let index = 1; index < samples.length; index += 1) {
    maxSampleGapMs = Math.max(maxSampleGapMs, samples[index]!.capturedAtMs - samples[index - 1]!.capturedAtMs);
  }

  return {
    idleRecovery: idleRecoverySummary(samples, baselineAppMemoryBytes, idleMarkedAtMs),
    sampleCount: samples.length,
    durationSeconds: Math.max(0, (last.capturedAtMs - first.capturedAtMs) / 1_000),
    coldBaselineAppMemoryBytes: first.appMemoryBytes,
    warmBaselineAppMemoryBytes: warmBaseline?.appMemoryBytes ?? null,
    baselineAppMemoryBytes,
    currentAppMemoryBytes: last.appMemoryBytes,
    peakAppMemoryBytes: Math.max(...samples.map((sample) => sample.appMemoryBytes)),
    peakMediaWorkerMemoryBytes: Math.max(...samples.map((sample) => sample.mediaWorkerMemoryBytes)),
    peakAppCpuPercent: Math.max(...samples.map((sample) => sample.appCpuPercent)),
    peakPacketBufferBytes: validMedia.length > 0
      ? Math.max(...validMedia.map((sample) => sample.packetBufferBytes!)) : null,
    peakSystemNetworkRxBps: Math.max(...samples.map((sample) => sample.systemNetworkRxBps)),
    peakMountedVideoElements: Math.max(...samples.map((sample) => sample.mountedVideoElements)),
    peakActiveMseSessions: Math.max(...samples.map((sample) => sample.activeMseSessions)),
    peakBufferedSeconds: Math.max(...samples.map((sample) => sample.bufferedSeconds)),
    generationStartsDelta: generationsContinuous
      ? last.generationStarts! - first.generationStarts! : null,
    mediaUnavailableSamples: samples.length - validMedia.length,
    maxSampleGapSeconds: Math.max(0, maxSampleGapMs / 1_000),
    memoryTrendBytesPerHour: linearRatePerHour(samples, (sample) => sample.appMemoryBytes),
    recentMemorySpanBytes: Math.max(...recentMemoryValues) - Math.min(...recentMemoryValues),
    recentWindowSeconds: Math.max(0, (last.capturedAtMs - recentStart) / 1_000),
  };
}

export function createMediaSoakEvidence(
  samples: MediaSoakSample[],
  exportedAtMs = Date.now(),
  idleMarkedAtMs: number | null = null,
  warmBaselineMarkedAtMs: number | null = null,
  missedPerformanceSamples = 0,
): MediaSoakEvidence {
  return {
    schemaVersion: 1,
    exportedAtMs,
    sampleIntervalMs: MEDIA_SOAK_SAMPLE_INTERVAL_MS,
    maxSamples: MEDIA_SOAK_MAX_SAMPLES,
    idleMarkedAtMs,
    warmBaselineMarkedAtMs,
    missedPerformanceSamples,
    summary: summarizeMediaSoak(samples, idleMarkedAtMs, warmBaselineMarkedAtMs),
    samples: [...samples],
  };
}

export function serializeMediaSoakEvidence(
  samples: MediaSoakSample[],
  exportedAtMs = Date.now(),
  idleMarkedAtMs: number | null = null,
  warmBaselineMarkedAtMs: number | null = null,
  missedPerformanceSamples = 0,
): string {
  return `${JSON.stringify(createMediaSoakEvidence(samples, exportedAtMs, idleMarkedAtMs, warmBaselineMarkedAtMs, missedPerformanceSamples), null, 2)}\n`;
}
