import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PtzControls } from "./PtzControls";
import type { PtzCapabilities, PtzMovement } from "../lib/tauri";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const supported: PtzCapabilities = {
  camera_id: "front-door",
  configured: true,
  ptz_supported: true,
  pan_tilt_supported: true,
  zoom_supported: true,
  state: "ready",
  error: null,
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function movement(generation: number): PtzMovement {
  return {
    camera_id: "front-door",
    generation,
    lease_ms: 1_000,
    renew_after_ms: 400,
  };
}

beforeEach(() => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(invoke).mockReset();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("PtzControls", () => {
  it("shows not-configured and unsupported states without sending commands", () => {
    const { rerender } = render(
      <PtzControls cameraId="front-door" capabilities={{ ...supported, configured: false, ptz_supported: false, pan_tilt_supported: false }} error={null} onError={() => undefined} />,
    );
    expect(screen.getByText("PTZ: not configured")).toBeTruthy();
    rerender(
      <PtzControls cameraId="front-door" capabilities={{ ...supported, ptz_supported: false, pan_tilt_supported: false }} error={null} onError={() => undefined} />,
    );
    expect(screen.getByText("PTZ: unsupported")).toBeTruthy();
    expect(invoke).not.toHaveBeenCalled();
  });

  it("hold direction starts movement and release stops the returned generation", async () => {
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "ptz_move") return movement(7);
      if (command === "ptz_stop") return undefined;
      if (command === "ptz_renew") return undefined;
      throw new Error(`unexpected ${command}`);
    });
    render(<PtzControls cameraId="front-door" capabilities={supported} error={null} onError={() => undefined} />);
    const left = screen.getByRole("button", { name: "PTZ Left" });
    fireEvent.pointerDown(left, { pointerId: 1 });
    await waitFor(() => expect(vi.mocked(invoke).mock.calls.some(([command]) => command === "ptz_move")).toBe(true));
    fireEvent.pointerUp(left, { pointerId: 1 });
    await waitFor(() => expect(vi.mocked(invoke).mock.calls.some(([command, args]) =>
      command === "ptz_stop" && (args as { input: { generation: number } }).input.generation === 7,
    )).toBe(true));
  });

  it("release while ptz_move is pending immediately stops the late movement", async () => {
    const pending = deferred<PtzMovement>();
    vi.mocked(invoke).mockImplementation((command) => {
      if (command === "ptz_move") return pending.promise;
      if (command === "ptz_stop") return Promise.resolve(undefined);
      return Promise.resolve(undefined);
    });
    render(<PtzControls cameraId="front-door" capabilities={supported} error={null} onError={() => undefined} />);
    const left = screen.getByRole("button", { name: "PTZ Left" });
    fireEvent.pointerDown(left, { pointerId: 1 });
    fireEvent.pointerUp(left, { pointerId: 1 });
    pending.resolve(movement(11));
    await waitFor(() => expect(vi.mocked(invoke).mock.calls.some(([command, args]) =>
      command === "ptz_stop" && (args as { input: { generation: number } }).input.generation === 11,
    )).toBe(true));
  });

  it("Left then Right cannot let a stale Left response replace current ownership", async () => {
    const leftPending = deferred<PtzMovement>();
    const rightPending = deferred<PtzMovement>();
    let moveCount = 0;
    vi.mocked(invoke).mockImplementation((command) => {
      if (command === "ptz_move") {
        moveCount += 1;
        return moveCount === 1 ? leftPending.promise : rightPending.promise;
      }
      return Promise.resolve(undefined);
    });
    render(<PtzControls cameraId="front-door" capabilities={supported} error={null} onError={() => undefined} />);
    fireEvent.pointerDown(screen.getByRole("button", { name: "PTZ Left" }), { pointerId: 1 });
    fireEvent.pointerDown(screen.getByRole("button", { name: "PTZ Right" }), { pointerId: 2 });
    rightPending.resolve(movement(22));
    await waitFor(() => expect(moveCount).toBe(2));
    leftPending.resolve(movement(21));
    await waitFor(() => expect(vi.mocked(invoke).mock.calls.some(([command, args]) =>
      command === "ptz_stop" && (args as { input: { generation: number } }).input.generation === 21,
    )).toBe(true));
    expect(vi.mocked(invoke).mock.calls.some(([command, args]) =>
      command === "ptz_stop" && (args as { input: { generation: number } }).input.generation === 22,
    )).toBe(false);
  });

  it("unmount stops current movement and late pending movement", async () => {
    const pending = deferred<PtzMovement>();
    vi.mocked(invoke).mockImplementation((command) => {
      if (command === "ptz_move") return pending.promise;
      return Promise.resolve(undefined);
    });
    const view = render(<PtzControls cameraId="front-door" capabilities={supported} error={null} onError={() => undefined} />);
    fireEvent.pointerDown(screen.getByRole("button", { name: "PTZ Up" }), { pointerId: 1 });
    view.unmount();
    pending.resolve(movement(31));
    await waitFor(() => expect(vi.mocked(invoke).mock.calls.some(([command, args]) =>
      command === "ptz_stop" && (args as { input: { generation: number } }).input.generation === 31,
    )).toBe(true));
  });

  it("surfaces a PTZ failure only through the per-camera error callback", async () => {
    const onError = vi.fn();
    vi.mocked(invoke).mockRejectedValue({ code: "device_unreachable", message: "ONVIF PTZ device is unreachable" });
    render(<PtzControls cameraId="front-door" capabilities={supported} error={null} onError={onError} />);
    fireEvent.pointerDown(screen.getByRole("button", { name: "PTZ Down" }), { pointerId: 1 });
    await waitFor(() => expect(onError).toHaveBeenCalledWith({
      code: "device_unreachable",
      message: "ONVIF PTZ device is unreachable",
    }));
  });
});
