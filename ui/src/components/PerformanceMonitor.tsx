import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invokeDesktop, isTauri } from "../lib/tauri";
import type { PerformanceSnapshot } from "../lib/tauri";

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

function formatRate(bytesPerSecond: number): string {
  return `${formatBytes(bytesPerSecond)}/s`;
}

export function PerformanceMonitor() {
  const [snapshot, setSnapshot] = useState<PerformanceSnapshot | null>(null);
  const [open, setOpen] = useState(false);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const popoverRef = useRef<HTMLDivElement | null>(null);
  const [available, setAvailable] = useState(isTauri());

  useEffect(() => {
    if (!isTauri()) return;
    let disposed = false;
    let timer: number | null = null;

    const poll = async () => {
      try {
        const next = await invokeDesktop<PerformanceSnapshot>("performance_snapshot");
        if (!disposed) {
          setSnapshot(next);
          setAvailable(true);
        }
      } catch {
        if (!disposed) setAvailable(false);
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

  const memoryPercent = useMemo(() => {
    if (!snapshot || snapshot.system_memory_total_bytes <= 0) return 0;
    return Math.min(100, (snapshot.app_memory_bytes / snapshot.system_memory_total_bytes) * 100);
  }, [snapshot]);

  const systemMemoryPercent = useMemo(() => {
    if (!snapshot || snapshot.system_memory_total_bytes <= 0) return 0;
    return Math.min(100, (snapshot.system_memory_used_bytes / snapshot.system_memory_total_bytes) * 100);
  }, [snapshot]);

  const footerLabel = snapshot?.sample_ready
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
