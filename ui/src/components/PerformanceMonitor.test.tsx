import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PerformanceMonitor } from "./PerformanceMonitor";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

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
    throw new Error(`unexpected command ${command}`);
  });
});

afterEach(() => {
  cleanup();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("PerformanceMonitor", () => {
  it("shows lightweight process telemetry and honest unsupported counters", async () => {
    render(<PerformanceMonitor />);

    await waitFor(() => expect(screen.getByText(/CPU 1.5% · Host 128 MB/)).toBeTruthy());
    const trigger = screen.getByRole("button", { name: "Nian Vision performance" });
    fireEvent.click(trigger);

    const popover = screen.getByRole("region", { name: "Performance details" });
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

    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("region", { name: "Performance details" })).toBeNull();
    expect(document.activeElement).toBe(trigger);

    fireEvent.click(trigger);
    fireEvent.pointerDown(document.body);
    expect(screen.queryByRole("region", { name: "Performance details" })).toBeNull();
  });
});
