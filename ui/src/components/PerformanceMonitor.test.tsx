import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PerformanceMonitor } from "./PerformanceMonitor";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const mediaDiagnostics = [{
  camera_id: "front-door",
  worker_available: true,
  generation_starts: 4,
  source_count: 1,
  main_sources: 1,
  sub_sources: 0,
  connecting_sources: 0,
  ready_sources: 1,
  failed_sources: 0,
  subscribers: 2,
  reliable_subscribers: 1,
  realtime_subscribers: 1,
  consumers: { recording: 1, live: 1, motion: 0, event: 0, other: 0 },
  queued_packets: 12,
  queued_bytes: 256 * 1024,
  dropped_packets: 3,
  pre_roll_packets: 48,
  pre_roll_bytes: 2 * 1024 * 1024,
  pre_roll_dropped_packets: 7,
  sources: [{
    profile: "main",
    lifecycle: "ready",
    retainers: 1,
    subscribers: 2,
    reliable_subscribers: 1,
    realtime_subscribers: 1,
    consumers: { recording: 1, live: 1, motion: 0, event: 0, other: 0 },
    queued_packets: 12,
    queued_bytes: 256 * 1024,
    dropped_packets: 3,
    pre_roll_packets: 48,
    pre_roll_bytes: 2 * 1024 * 1024,
    pre_roll_dropped_packets: 7,
    video_frame_rate: { num: 25, den: 1 },
    video_codec: "h264",
    video_width: 1920,
    video_height: 1080,
  }],
}];

const snapshot = {
  sample_ready: true,
  process_count: 3,
  app_cpu_percent: 6.25,
  root_process_cpu_percent: 1.5,
  system_cpu_percent: 31.5,
  app_memory_bytes: 512 * 1024 * 1024,
  root_process_memory_bytes: 128 * 1024 * 1024,
  child_process_memory_bytes: 384 * 1024 * 1024,
  system_memory_used_bytes: 8 * 1024 ** 3,
  system_memory_total_bytes: 16 * 1024 ** 3,
  system_network_rx_bps: 4 * 1024 * 1024,
  system_network_tx_bps: 512 * 1024,
  app_network_rx_bps: null,
  app_network_tx_bps: null,
  app_gpu_percent: null,
  system_gpu_percent: null,
  processes: [
    { pid: 100, name: "nian-vision.exe", role: "Desktop host", cpu_percent: 1.5, memory_bytes: 128 * 1024 * 1024, is_root: true },
    { pid: 101, name: "msedgewebview2.exe", role: "WebView2 renderer", cpu_percent: 3.25, memory_bytes: 256 * 1024 * 1024, is_root: false },
    { pid: 102, name: "nian-media-worker.exe", role: "Media worker", cpu_percent: 1.5, memory_bytes: 128 * 1024 * 1024, is_root: false },
  ],
};

beforeEach(() => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(invoke).mockReset();
  vi.mocked(invoke).mockImplementation(async (command) => {
    if (command === "performance_snapshot") return snapshot;
    if (command === "media_diagnostics") return mediaDiagnostics;
    throw new Error(`unexpected command ${command}`);
  });
});

afterEach(() => {
  cleanup();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("PerformanceMonitor", () => {
  it("opens an accessible modal instead of a sidebar popover", async () => {
    render(<PerformanceMonitor />);
    const trigger = screen.getByRole("button", { name: "Nian Vision performance" });
    fireEvent.click(trigger);
    const dialog = screen.getByRole("dialog", { name: "Performance" });
    expect(dialog.getAttribute("aria-modal")).toBe("true");
    expect(document.body.contains(dialog)).toBe(true);
    expect(document.body.style.overflow).toBe("hidden");
    const close = screen.getByRole("button", { name: "Close performance monitor" });
    await waitFor(() => expect(document.activeElement).toBe(close));
    fireEvent.keyDown(document, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(Array.from(dialog.querySelectorAll("button:not([disabled])")).at(-1));
    fireEvent.click(close);
    expect(screen.queryByRole("dialog", { name: "Performance" })).toBeNull();
    expect(document.body.style.overflow).toBe("");
    expect(document.activeElement).toBe(trigger);
    fireEvent.click(trigger);
    const reopened = screen.getByRole("dialog", { name: "Performance" });
    const backdrop = reopened.parentElement!;
    fireEvent.pointerDown(backdrop);
    expect(screen.queryByRole("dialog", { name: "Performance" })).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("does not display a media-diagnostics RPC failure as zero active workers", async () => {
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "performance_snapshot") return snapshot;
      if (command === "media_diagnostics") throw new Error("offline");
      throw new Error(`unexpected command ${command}`);
    });
    render(<PerformanceMonitor />);
    await waitFor(() => expect(screen.getByText(/CPU 1.5% · Host 128 MB/)).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Nian Vision performance" }));
    await waitFor(() => expect(screen.getByText(/Media diagnostics unavailable; queue and ring metrics are not zero/)).toBeTruthy());
    expect(screen.queryByText("No active camera media workers.")).toBeNull();
  });

  it("rejects a warm-baseline marker when its process snapshot has expired", async () => {
    render(<PerformanceMonitor />);
    await waitFor(() => expect(screen.getByText(/CPU 1.5% · Host 128 MB/)).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Nian Vision performance" }));
    fireEvent.click(screen.getByRole("button", { name: "Start soak" }));
    await waitFor(() => expect(screen.getByText(/1 samples · 5s cadence/)).toBeTruthy());

    const staleNow = Date.now() + 5_000;
    const clock = vi.spyOn(Date, "now").mockReturnValue(staleNow);
    try {
      fireEvent.click(screen.getByRole("button", { name: "Mark warm baseline" }));
      expect(screen.getByRole("status").textContent).toMatch(/Performance data is stale or unavailable/);
      expect(screen.getByText(/B0 512 MB · Bw not marked/)).toBeTruthy();
      expect(screen.getByText(/1 samples · 5s cadence/)).toBeTruthy();
    } finally {
      clock.mockRestore();
    }
  });

  it("shows lightweight process telemetry and honest unsupported counters", async () => {
    render(<PerformanceMonitor />);

    await waitFor(() => expect(screen.getByText(/CPU 1.5% · Host 128 MB/)).toBeTruthy());
    const trigger = screen.getByRole("button", { name: "Nian Vision performance" });
    fireEvent.click(trigger);

    const popover = screen.getByRole("dialog", { name: "Performance" });
    expect(popover).toBeTruthy();
    expect(document.body.contains(popover)).toBe(true);
    expect(trigger.parentElement?.contains(popover)).toBe(false);
    expect(screen.getByText("3 processes")).toBeTruthy();
    expect(screen.getByText(/Desktop 128 MB · child processes 384 MB/)).toBeTruthy();
    expect(screen.getByText("WebView2 renderer")).toBeTruthy();
    expect(screen.getByText("Media worker")).toBeTruthy();
    expect(screen.getByText(/msedgewebview2.exe · PID 101/)).toBeTruthy();
    expect(screen.getByText(/3.3% CPU · 50% memory/)).toBeTruthy();
    expect(screen.getByText(/system 32%/)).toBeTruthy();
    expect(screen.getByText(/Per-process network accounting/)).toBeTruthy();
    expect(screen.getByText("GPU accounting unavailable")).toBeTruthy();
    expect(screen.getByText("0 video elements")).toBeTruthy();
    expect(screen.getByText(/0 active MSE · 0.0s buffered/)).toBeTruthy();
    await waitFor(() => expect(screen.getByText("front-door")).toBeTruthy());
    expect(screen.getByText(/1 shared RTSP source · 4 generations · main 1 · sub 0 · 2 subscribers/)).toBeTruthy();
    expect(screen.getByText(/1 reliable · 1 realtime · 12 queued · 3 dropped/)).toBeTruthy();
    expect(screen.getByText(/Recording 1 · Live 1 · Motion 0 · Event 0/)).toBeTruthy();
    expect(screen.getByText(/Pre-roll 2\.00 MB · 48 packets · 7 evicted/)).toBeTruthy();
    expect(screen.getByText(/MAIN · ready · h264 1920×1080 @ 25 fps · Event pre-roll leases 1 · Rec 1 \/ Live 1 \/ Motion 0 \/ Event 0 · R1\/RT1 · Q 256 KB\/12 · ring 2\.00 MB\/48/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Start soak" }));
    expect(screen.getByRole("button", { name: "Stop soak" })).toBeTruthy();
    await waitFor(() => expect(screen.getByText(/1 samples · 5s cadence/)).toBeTruthy());
    expect(screen.getByText(/B0 512 MB · Bw not marked/)).toBeTruthy();
    expect(screen.getByText(/RAM now 512 MB \(0 B vs B0\) · peak 512 MB/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Mark media closed" }).hasAttribute("disabled")).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Mark warm baseline" }));
    expect(screen.getByText(/B0 512 MB · Bw 512 MB/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Mark media closed" }).hasAttribute("disabled")).toBe(false);
    expect(screen.getByText(/RAM now 512 MB \(0 B vs Bw\) · peak 512 MB/)).toBeTruthy();
    expect(screen.getByText(/RAM trend 0 B\/h · recent 0s span 0 B/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Mark media closed" }));
    expect(screen.getByRole("button", { name: "Re-mark media closed" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Re-mark warm baseline" }).hasAttribute("disabled")).toBe(true);
    expect(screen.getByText(/Idle mark 512 MB · \+30s pending 0s\/30s · \+120s pending 0s\/2m 0s/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Stop soak" }));
    expect(screen.getByRole("button", { name: "Start soak" })).toBeTruthy();

    const createObjectURL = vi.fn(() => "blob:nian-soak-evidence");
    const revokeObjectURL = vi.fn();
    Object.defineProperty(URL, "createObjectURL", { configurable: true, value: createObjectURL });
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: revokeObjectURL });
    const anchorClick = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => undefined);
    fireEvent.click(screen.getByRole("button", { name: "Export JSON" }));
    expect(createObjectURL).toHaveBeenCalledOnce();
    expect(anchorClick).toHaveBeenCalledOnce();
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:nian-soak-evidence");
    anchorClick.mockRestore();

    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("dialog", { name: "Performance" })).toBeNull();
    expect(document.activeElement).toBe(trigger);

    fireEvent.click(trigger);
    fireEvent.pointerDown(screen.getByRole("dialog", { name: "Performance" }).parentElement!);
    expect(screen.queryByRole("dialog", { name: "Performance" })).toBeNull();
  });
});
