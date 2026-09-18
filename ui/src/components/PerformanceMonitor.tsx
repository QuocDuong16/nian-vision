import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { readFrontendMediaDiagnostics } from "../lib/mediaTelemetry";
import type { FrontendMediaDiagnostics } from "../lib/mediaTelemetry";
import {
  MEDIA_SOAK_CHECKPOINT_MAX_LATENESS_MS,
  MEDIA_SOAK_MAX_SAMPLES,
  MEDIA_SOAK_SAMPLE_INTERVAL_MS,
  appendMediaSoakSample,
  createMediaSoakSample,
  isFreshMediaSoakSnapshot,
  serializeMediaSoakEvidence,
  summarizeMediaSoak,
} from "../lib/mediaSoak";
import type { MediaSoakSample } from "../lib/mediaSoak";
import { invokeDesktop, isTauri } from "../lib/tauri";
import type { CameraMediaDiagnostics, CameraMediaSourceDiagnostics, PerformanceSnapshot } from "../lib/tauri";

const SAMPLE_INTERVAL_MS = 1_000;

function formatPercent(value: number): string {
  if (!Number.isFinite(value)) return "—";
  return `${value < 10 ? value.toFixed(1) : value.toFixed(0)}%`;
}

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = units[0]!;
  for (let index = 1; index < units.length && value >= 1024; index += 1) {
    value /= 1024;
    unit = units[index]!;
  }
  return `${value >= 100 ? value.toFixed(0) : value >= 10 ? value.toFixed(1) : value.toFixed(2)} ${unit}`;
}

function formatSignedBytes(bytes: number): string {
  const sign = bytes > 0 ? "+" : bytes < 0 ? "−" : "";
  return `${sign}${formatBytes(Math.abs(bytes))}`;
}

function formatSignedRate(bytesPerHour: number): string {
  const sign = bytesPerHour > 0 ? "+" : bytesPerHour < 0 ? "−" : "";
  return `${sign}${formatBytes(Math.abs(bytesPerHour))}/h`;
}

function formatDuration(seconds: number): string {
  const whole = Math.max(0, Math.round(seconds));
  const hours = Math.floor(whole / 3_600);
  const minutes = Math.floor((whole % 3_600) / 60);
  const remainder = whole % 60;
  if (hours > 0) return `${hours}h ${minutes}m`;
  if (minutes > 0) return `${minutes}m ${remainder}s`;
  return `${remainder}s`;
}

function formatIdleCheckpoint(
  label: string,
  memoryBytes: number | null,
  deltaBytes: number | null,
  elapsedSeconds: number,
  targetSeconds: number,
): string {
  if (memoryBytes === null || deltaBytes === null) {
    if (elapsedSeconds > targetSeconds + MEDIA_SOAK_CHECKPOINT_MAX_LATENESS_MS / 1_000) {
      return `${label} unavailable (sample gap)`;
    }
    return `${label} pending ${formatDuration(Math.min(elapsedSeconds, targetSeconds))}/${formatDuration(targetSeconds)}`;
  }
  return `${label} ${formatBytes(memoryBytes)} (${formatSignedBytes(deltaBytes)} vs Bw)`;
}

function formatRate(bytesPerSecond: number): string {
  return `${formatBytes(bytesPerSecond)}/s`;
}

function formatMediaSourceVideo(source: CameraMediaSourceDiagnostics): string {
  const codec = source.video_codec ?? "video pending";
  const resolution = source.video_width !== null && source.video_height !== null
    ? ` ${source.video_width}×${source.video_height}`
    : "";
  const fpsValue = source.video_frame_rate !== null && source.video_frame_rate.den !== 0
    ? source.video_frame_rate.num / source.video_frame_rate.den
    : null;
  const fps = fpsValue !== null && Number.isFinite(fpsValue) && fpsValue > 0
    ? ` @ ${Number.isInteger(fpsValue) ? fpsValue.toFixed(0) : fpsValue.toFixed(2)} fps`
    : "";
  return `${codec}${resolution}${fps}`;
}

function exportMediaSoakEvidence(
  samples: MediaSoakSample[],
  idleMarkedAtMs: number | null,
  warmBaselineMarkedAtMs: number | null,
  missedPerformanceSamples: number,
): void {
  if (samples.length === 0) return;
  const exportedAt = Date.now();
  const blob = new Blob([serializeMediaSoakEvidence(samples, exportedAt, idleMarkedAtMs, warmBaselineMarkedAtMs, missedPerformanceSamples)], { type: "application/json" });
  const objectUrl = URL.createObjectURL(blob);
  try {
    const anchor = document.createElement("a");
    anchor.href = objectUrl;
    anchor.download = `nian-vision-media-soak-${new Date(exportedAt).toISOString().replaceAll(":", "-")}.json`;
    anchor.style.display = "none";
    document.body.append(anchor);
    anchor.click();
    anchor.remove();
  } finally {
    URL.revokeObjectURL(objectUrl);
  }
}

export function PerformanceMonitor() {
  const [snapshot, setSnapshot] = useState<PerformanceSnapshot | null>(null);
  const [mediaDiagnostics, setMediaDiagnostics] = useState<CameraMediaDiagnostics[]>([]);
  const [mediaDiagnosticsUnavailable, setMediaDiagnosticsUnavailable] = useState(false);
  const [frontendMedia, setFrontendMedia] = useState<FrontendMediaDiagnostics>(() => readFrontendMediaDiagnostics());
  const [soakActive, setSoakActive] = useState(false);
  const [soakSamples, setSoakSamples] = useState<MediaSoakSample[]>([]);
  const [soakMissedPerformanceSamples, setSoakMissedPerformanceSamples] = useState(0);
  const [soakWarning, setSoakWarning] = useState<string | null>(null);
  const [idleMarkedAtMs, setIdleMarkedAtMs] = useState<number | null>(null);
  const [warmBaselineMarkedAtMs, setWarmBaselineMarkedAtMs] = useState<number | null>(null);
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const snapshotRef = useRef<PerformanceSnapshot | null>(null);
  const snapshotUpdatedAtMsRef = useRef<number | null>(null);
  const mediaDiagnosticsRef = useRef<CameraMediaDiagnostics[] | null>(null);
  const mediaUpdatedAtMsRef = useRef<number | null>(null);
  const soakStartedAtMsRef = useRef<number | null>(null);
  const frontendMediaRef = useRef<FrontendMediaDiagnostics>(frontendMedia);
  const [available, setAvailable] = useState(isTauri());
  const soakSummary = useMemo(
    () => summarizeMediaSoak(soakSamples, idleMarkedAtMs, warmBaselineMarkedAtMs),
    [soakSamples, idleMarkedAtMs, warmBaselineMarkedAtMs],
  );

  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let timer: number | null = null;

    const poll = async () => {
      try {
        const next = await invokeDesktop<PerformanceSnapshot>("performance_snapshot");
        if (!disposed) {
          snapshotRef.current = next;
          snapshotUpdatedAtMsRef.current = Date.now();
          setSnapshot(next);
          setAvailable(true);
        }
      } catch {
        if (!disposed) {
          snapshotRef.current = null;
          snapshotUpdatedAtMsRef.current = null;
          setAvailable(false);
        }
      } finally {
        if (!disposed) timer = window.setTimeout(() => void poll(), SAMPLE_INTERVAL_MS);
      }
    };

    void poll();
    return () => {
      disposed = true;
      if (timer !== null) window.clearTimeout(timer);
    };
  }, []);

  useEffect(() => {
    if (!open && !soakActive) return;
    let disposed = false;
    let timer: number | null = null;
    const sample = () => {
      if (disposed) return;
      const next = readFrontendMediaDiagnostics();
      frontendMediaRef.current = next;
      setFrontendMedia(next);
      timer = window.setTimeout(sample, SAMPLE_INTERVAL_MS);
    };
    sample();
    return () => {
      disposed = true;
      if (timer !== null) window.clearTimeout(timer);
    };
  }, [open, soakActive]);

  useEffect(() => {
    if ((!open && !soakActive) || !isTauri()) return;
    let disposed = false;
    let timer: number | null = null;
    const poll = async () => {
      try {
        const rows = await invokeDesktop<CameraMediaDiagnostics[]>("media_diagnostics");
        if (!disposed) {
          mediaDiagnosticsRef.current = rows;
          mediaUpdatedAtMsRef.current = rows.every((row) => row.worker_available) ? Date.now() : null;
          setMediaDiagnostics(rows);
          setMediaDiagnosticsUnavailable(false);
        }
      } catch {
        if (!disposed) {
          mediaDiagnosticsRef.current = null;
          mediaUpdatedAtMsRef.current = null;
          setMediaDiagnostics([]);
          setMediaDiagnosticsUnavailable(true);
        }
      } finally {
        if (!disposed) timer = window.setTimeout(() => void poll(), 2_000);
      }
    };
    void poll();
    return () => {
      disposed = true;
      if (timer !== null) window.clearTimeout(timer);
    };
  }, [open, soakActive]);

  useEffect(() => {
    if (!soakActive) return;
    let disposed = false;
    let timer: number | null = null;
    const sample = () => {
      if (disposed) return;
      const now = Date.now();
      if (soakStartedAtMsRef.current !== null && now - soakStartedAtMsRef.current >= 2 * 60 * 60 * 1_000) {
        setSoakWarning("Two-hour capture window reached. Export this run before starting a new one.");
        setSoakActive(false);
        return;
      }
      const current = snapshotRef.current;
      if (isFreshMediaSoakSnapshot(current, snapshotUpdatedAtMsRef.current, now)) {
        const mediaAge = mediaUpdatedAtMsRef.current === null ? Infinity : now - mediaUpdatedAtMsRef.current;
        const media = mediaAge >= 0 && mediaAge <= 5_000 ? mediaDiagnosticsRef.current : null;
        const next = createMediaSoakSample(current!, frontendMediaRef.current, media, now);
        setSoakSamples((samples) => appendMediaSoakSample(samples, next));
      } else {
        setSoakMissedPerformanceSamples((count) => count + 1);
      }
      timer = window.setTimeout(sample, MEDIA_SOAK_SAMPLE_INTERVAL_MS);
    };
    sample();
    return () => {
      disposed = true;
      if (timer !== null) window.clearTimeout(timer);
    };
  }, [soakActive]);

  useEffect(() => {
    if (soakActive && soakSamples.length >= MEDIA_SOAK_MAX_SAMPLES) setSoakActive(false);
  }, [soakActive, soakSamples.length]);

  useEffect(() => {
    if (!open) return;

    const onPointerDown = (event: PointerEvent) => {
      const target = event.target as Node | null;
      if (target && (triggerRef.current?.contains(target) || popoverRef.current?.contains(target))) return;
      setOpen(false);
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      setOpen(false);
      triggerRef.current?.focus();
    };

    document.addEventListener("pointerdown", onPointerDown);
    document.addEventListener("keydown", onKeyDown);
    return () => {
      document.removeEventListener("pointerdown", onPointerDown);
      document.removeEventListener("keydown", onKeyDown);
    };
  }, [open]);

  const markSoakCheckpoint = (kind: "warm" | "idle") => {
    const current = snapshotRef.current;
    if (!soakActive || soakSamples.length === 0 || soakSamples.length >= MEDIA_SOAK_MAX_SAMPLES || !current?.sample_ready) return;
    if (kind === "idle" && warmBaselineMarkedAtMs === null) return;
    const markedAtMs = Date.now();
    if (!isFreshMediaSoakSnapshot(current, snapshotUpdatedAtMsRef.current, markedAtMs)) {
      setSoakWarning("Performance data is stale or unavailable. Check the process telemetry before marking a baseline.");
      return;
    }
    setSoakWarning(null);
    const frontend = readFrontendMediaDiagnostics();
    frontendMediaRef.current = frontend;
    setFrontendMedia(frontend);
    const mediaAge = mediaUpdatedAtMsRef.current === null ? Infinity : markedAtMs - mediaUpdatedAtMsRef.current;
    const media = mediaAge >= 0 && mediaAge <= 5_000 ? mediaDiagnosticsRef.current : null;
    setSoakSamples((samples) => appendMediaSoakSample(
      samples,
      createMediaSoakSample(current, frontend, media, markedAtMs),
    ));
    if (kind === "warm") setWarmBaselineMarkedAtMs(markedAtMs);
    else setIdleMarkedAtMs(markedAtMs);
  };

  const memoryPercent = useMemo(() => {
    if (!snapshot || snapshot.system_memory_total_bytes <= 0) return 0;
    return Math.min(100, (snapshot.app_memory_bytes / snapshot.system_memory_total_bytes) * 100);
  }, [snapshot]);

  const systemMemoryPercent = useMemo(() => {
    if (!snapshot || snapshot.system_memory_total_bytes <= 0) return 0;
    return Math.min(100, (snapshot.system_memory_used_bytes / snapshot.system_memory_total_bytes) * 100);
  }, [snapshot]);

  const footerLabel = snapshot?.sample_ready && available
    ? `CPU ${formatPercent(snapshot.root_process_cpu_percent)} · Host ${formatBytes(snapshot.root_process_memory_bytes)}`
    : available ? "Performance warming up…" : "Performance unavailable";
  const childProcessCount = Math.max(0, (snapshot?.process_count ?? 1) - 1);

  return (
    <div className="performance-monitor">
      <button
        ref={triggerRef}
        type="button"
        className="performance-monitor-trigger"
        aria-label="Nian Vision performance"
        aria-expanded={open}
        aria-controls="performance-details"
        title={snapshot?.sample_ready
          ? `Nian Vision process tree: desktop host + ${childProcessCount} child process${childProcessCount === 1 ? "" : "es"}`
          : undefined}
        onClick={() => setOpen((current) => !current)}
      >
        <span className={`sidebar-health-dot${available ? "" : " is-muted"}`} aria-hidden="true" />
        <span className="performance-monitor-summary">{footerLabel}</span>
      </button>

      {open && typeof document !== "undefined" && createPortal(
        <div
          id="performance-details"
          ref={popoverRef}
          className="performance-popover"
          role="region"
          aria-label="Performance details"
        >
          {!available && <small role="status">Process telemetry unavailable. Displayed process values are last known and excluded from soak capture.</small>}
          <div className="performance-popover-head">
            <div><strong>Performance</strong><span>Desktop host + WebView/media child processes</span></div>
            <span className="performance-process-count">{snapshot?.process_count ?? 0} processes</span>
          </div>
          {snapshot ? (
            <div className="performance-grid">
              <div className="performance-card">
                <span>CPU</span>
                <strong>{formatPercent(snapshot.app_cpu_percent)}</strong>
                <small>Total Nian Vision · desktop {formatPercent(snapshot.root_process_cpu_percent)} · system {formatPercent(snapshot.system_cpu_percent)}</small>
                <div className="performance-bar"><span style={{ width: `${Math.min(100, snapshot.app_cpu_percent)}%` }} /></div>
              </div>
              <div className="performance-card">
                <span>Memory</span>
                <strong>{formatBytes(snapshot.app_memory_bytes)}</strong>
                <small>Total Nian Vision {formatPercent(memoryPercent)} of {formatBytes(snapshot.system_memory_total_bytes)} · system {formatPercent(systemMemoryPercent)}</small>
                <em>Desktop {formatBytes(snapshot.root_process_memory_bytes)} · child processes {formatBytes(snapshot.child_process_memory_bytes)}</em>
                <div className="performance-bar"><span style={{ width: `${memoryPercent}%` }} /></div>
              </div>
              <div className="performance-card performance-soak-card">
                <span>Media soak</span>
                <strong>{soakActive ? "Capturing" : soakSummary ? "Stopped" : "Ready"}</strong>
                {soakSummary ? (
                  <>
                    <small>{formatDuration(soakSummary.durationSeconds)} · {soakSummary.sampleCount} samples · 5s cadence{soakSamples.length >= MEDIA_SOAK_MAX_SAMPLES ? " · capture limit reached" : ""}</small>
                    <em>
                      B0 {formatBytes(soakSummary.coldBaselineAppMemoryBytes)} · Bw {soakSummary.warmBaselineAppMemoryBytes === null ? "not marked" : formatBytes(soakSummary.warmBaselineAppMemoryBytes)}
                    </em>
                    <em>
                      RAM now {formatBytes(soakSummary.currentAppMemoryBytes)} ({formatSignedBytes(soakSummary.currentAppMemoryBytes - soakSummary.baselineAppMemoryBytes)} vs {soakSummary.warmBaselineAppMemoryBytes === null ? "B0" : "Bw"}) · peak {formatBytes(soakSummary.peakAppMemoryBytes)}
                    </em>
                    <em>
                      Worker peak {formatBytes(soakSummary.peakMediaWorkerMemoryBytes)} · packet buffers peak {soakSummary.peakPacketBufferBytes === null ? "unavailable" : formatBytes(soakSummary.peakPacketBufferBytes)} · CPU peak {formatPercent(soakSummary.peakAppCpuPercent)} · system RX peak {formatRate(soakSummary.peakSystemNetworkRxBps)}
                    </em>
                    <em>
                      RAM trend {formatSignedRate(soakSummary.memoryTrendBytesPerHour)} · recent {formatDuration(soakSummary.recentWindowSeconds)} span {formatBytes(soakSummary.recentMemorySpanBytes)}
                    </em>
                    <em>
                      Video peak {soakSummary.peakMountedVideoElements} · MSE peak {soakSummary.peakActiveMseSessions} · browser buffer peak {soakSummary.peakBufferedSeconds.toFixed(1)}s · ingest generations {soakSummary.generationStartsDelta === null ? "unavailable (missing/reset telemetry)" : `+${soakSummary.generationStartsDelta}`}
                    </em>
                    <em>Evidence quality: {soakMissedPerformanceSamples} missed process samples · {soakSummary.mediaUnavailableSamples} media-unavailable samples · max sample gap {formatDuration(soakSummary.maxSampleGapSeconds)}</em>
                    {soakSummary.idleRecovery && (
                      <em>
                        Idle mark {formatBytes(soakSummary.idleRecovery.memoryAtMarkBytes)} · {formatIdleCheckpoint(
                          "+30s", soakSummary.idleRecovery.memoryAfter30sBytes, soakSummary.idleRecovery.deltaFromBaselineAfter30sBytes,
                          soakSummary.idleRecovery.elapsedSinceMarkSeconds, 30,
                        )} · {formatIdleCheckpoint(
                          "+120s", soakSummary.idleRecovery.memoryAfter120sBytes, soakSummary.idleRecovery.deltaFromBaselineAfter120sBytes,
                          soakSummary.idleRecovery.elapsedSinceMarkSeconds, 120,
                        )}
                      </em>
                    )}
                  </>
                ) : (
                  <small>{soakActive ? "Waiting for a warmed performance sample…" : "Capture bounded five-second samples for up to two hours."}</small>
                )}
                <small>Start before first media use for B0. Open and close once, mark Bw, then mark media closed after the test cycles. Missing diagnostics are not treated as zero.</small>
                {soakWarning && <small role="status">{soakWarning}</small>}
                <div className="performance-soak-actions">
                  <button
                    type="button"
                    aria-pressed={soakActive}
                    onClick={() => {
                      if (soakActive) setSoakActive(false);
                      else {
                        setSoakSamples([]);
                        setIdleMarkedAtMs(null);
                        setWarmBaselineMarkedAtMs(null);
                        setSoakMissedPerformanceSamples(0);
                        setSoakWarning(null);
                        soakStartedAtMsRef.current = Date.now();
                        setSoakActive(true);
                      }
                    }}
                  >{soakActive ? "Stop soak" : "Start soak"}</button>
                  <button
                    type="button"
                    disabled={!soakActive || soakSamples.length === 0 || soakSamples.length >= MEDIA_SOAK_MAX_SAMPLES || idleMarkedAtMs !== null}
                    onClick={() => markSoakCheckpoint("warm")}
                  >{warmBaselineMarkedAtMs === null ? "Mark warm baseline" : "Re-mark warm baseline"}</button>
                  <button
                    type="button"
                    disabled={!soakActive || soakSamples.length === 0 || soakSamples.length >= MEDIA_SOAK_MAX_SAMPLES || warmBaselineMarkedAtMs === null}
                    onClick={() => markSoakCheckpoint("idle")}
                  >{idleMarkedAtMs === null ? "Mark media closed" : "Re-mark media closed"}</button>
                  <button type="button" disabled={soakSamples.length === 0} onClick={() => exportMediaSoakEvidence(soakSamples, idleMarkedAtMs, warmBaselineMarkedAtMs, soakMissedPerformanceSamples)}>Export JSON</button>
                  <button
                    type="button"
                    disabled={!soakActive && soakSamples.length === 0}
                    onClick={() => { setSoakActive(false); setSoakSamples([]); setIdleMarkedAtMs(null); setWarmBaselineMarkedAtMs(null); setSoakMissedPerformanceSamples(0); setSoakWarning(null); soakStartedAtMsRef.current = null; }}
                  >Reset</button>
                </div>
              </div>
              <div className="performance-card performance-process-card">
                <span>Process memory</span>
                <div className="performance-process-list">
                  {snapshot.processes.map((process) => (
                    <div className="performance-process-row" key={process.pid}>
                      <div><strong>{process.role}</strong><small>{process.name} · PID {process.pid}</small></div>
                      <div className="performance-process-metrics">
                        <strong>{formatBytes(process.memory_bytes)}</strong>
                        <small>{formatPercent(process.cpu_percent)} CPU · {formatPercent(snapshot.app_memory_bytes > 0 ? (process.memory_bytes / snapshot.app_memory_bytes) * 100 : 0)} memory</small>
                      </div>
                    </div>
                  ))}
                </div>
                <small>Windows prefers private working set per process; inaccessible counters fall back to the portable working-set estimate.</small>
              </div>
              <div className="performance-card">
                <span>WebView media</span>
                <strong>{frontendMedia.mountedVideoElements} video element{frontendMedia.mountedVideoElements === 1 ? "" : "s"}</strong>
                <small>{frontendMedia.activeMseSessions} active MSE · {frontendMedia.bufferedSeconds.toFixed(1)}s buffered</small>
                <em>DOM/MSE counters distinguish browser media retention from worker-side compressed queues.</em>
              </div>
              <div className="performance-card performance-process-card">
                <span>Media ingest</span>
                {mediaDiagnostics.length > 0 ? (
                  <div className="performance-process-list">
                    {mediaDiagnostics.map((media) => (
                      <div className="performance-process-row" key={media.camera_id}>
                        <div>
                          <strong>{media.camera_id}</strong>
                          <small>{media.worker_available ? `${media.source_count} shared RTSP source${media.source_count === 1 ? "" : "s"} · ${media.generation_starts} generations · main ${media.main_sources} · sub ${media.sub_sources} · ${media.subscribers} subscribers` : "Media worker unavailable"}</small>
                          {media.sources.length > 0 && (
                            <div className="performance-media-source-list">
                              {media.sources.map((source, index) => (
                                <small key={`${source.profile}-${index}`}>
                                  {source.profile.toUpperCase()} · {source.lifecycle} · {formatMediaSourceVideo(source)} · Event pre-roll leases {source.retainers} · Rec {source.consumers.recording} / Live {source.consumers.live} / Motion {source.consumers.motion} / Event {source.consumers.event}{source.consumers.other ? ` / Other ${source.consumers.other}` : ""} · R{source.reliable_subscribers}/RT{source.realtime_subscribers} · Q {formatBytes(source.queued_bytes)}/{source.queued_packets} · ring {formatBytes(source.pre_roll_bytes)}/{source.pre_roll_packets}
                                </small>
                              ))}
                            </div>
                          )}
                        </div>
                        <div className="performance-process-metrics">
                          <strong>{formatBytes(media.queued_bytes + media.pre_roll_bytes)}</strong>
                          <small>Recording {media.consumers.recording} · Live {media.consumers.live} · Motion {media.consumers.motion} · Event {media.consumers.event}{media.consumers.other ? ` · Other ${media.consumers.other}` : ""}</small>
                          <small>{media.reliable_subscribers} reliable · {media.realtime_subscribers} realtime · {media.queued_packets} queued · {media.dropped_packets} dropped</small>
                          <small>Pre-roll {formatBytes(media.pre_roll_bytes)} · {media.pre_roll_packets} packets · {media.pre_roll_dropped_packets} evicted</small>
                        </div>
                      </div>
                    ))}
                  </div>
                ) : (
                  <small>{mediaDiagnosticsUnavailable ? "Media diagnostics unavailable; queue and ring metrics are not zero." : "No active camera media workers."}</small>
                )}
                <small>Compressed packet queues and main-stream pre-roll are bounded; recording, live and event capture share each RTSP source inside the camera worker.</small>
              </div>
              <div className="performance-card">
                <span>Network</span>
                <strong>↓ {formatRate(snapshot.system_network_rx_bps)}</strong>
                <small>System ↑ {formatRate(snapshot.system_network_tx_bps)}</small>
                <em>Per-process network accounting is not exposed portably.</em>
              </div>
              <div className="performance-card">
                <span>Graphics</span>
                <strong>{snapshot.app_gpu_percent === null ? "—" : formatPercent(snapshot.app_gpu_percent)}</strong>
                <small>{snapshot.system_gpu_percent === null ? "GPU accounting unavailable" : `System ${formatPercent(snapshot.system_gpu_percent)}`}</small>
                <em>No vendor-specific estimate is shown as a fake universal value.</em>
              </div>
            </div>
          ) : (
            <p className="performance-unavailable">Performance telemetry is not available from the desktop host.</p>
          )}
        </div>,
        document.body,
      )}
    </div>
  );
}
