import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PerformanceMonitor } from "./PerformanceMonitor";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const snapshot = {
  sample_ready: true,
  process_count: 4,
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

    await waitFor(() => expect(screen.getByText(/CPU 6.3% · RAM 512 MB/)).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Nian Vision performance" }));

    expect(screen.getByRole("region", { name: "Performance details" })).toBeTruthy();
    expect(screen.getByText("4 processes")).toBeTruthy();
    expect(screen.getByText(/Desktop 128 MB · children 384 MB/)).toBeTruthy();
    expect(screen.getByText(/system 32%/)).toBeTruthy();
    expect(screen.getByText(/Per-process network accounting/)).toBeTruthy();
    expect(screen.getByText("GPU accounting unavailable")).toBeTruthy();
  });
});
