import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LiveViewScreen } from "./LiveViewScreen";
import type {
  CameraSummary,
  LiveOpenDto,
  LiveStatus,
  RecordingIntent,
  RecordingStatus,
} from "../lib/tauri";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const front: CameraSummary = {
  camera_id: "front-door",
  display_name: "Front door",
  host: "192.168.1.50",
  port: 554,
  path: "/stream1",
  audio_policy: "exclude",
};

const garage: CameraSummary = {
  camera_id: "garage",
  display_name: "Garage",
  host: "192.168.1.51",
  port: 554,
  path: "/stream1",
  audio_policy: "exclude",
};

function stopped(cameraId: string): RecordingStatus {
  return {
    state: "stopped",
    camera_id: cameraId,
    failure_category: null,
    reconnect_attempt: 0,
    finalized_segments: 0,
  };
}

function installDesktop(
  stateForCamera?: (cameraId: string, sessionId: string) => LiveStatus,
  openOverride?: (cameraId: string) => Promise<LiveOpenDto>,
  liveStatusesOverride?: () => Promise<LiveStatus[]>,
) {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  let live: LiveStatus[] = [];
  let recording: RecordingStatus[] = [stopped(front.camera_id), stopped(garage.camera_id)];
  let intent: RecordingIntent = { camera_ids: [] };
  let sequence = 0;

  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "camera_list") return [front, garage];
    if (command === "live_statuses") return liveStatusesOverride ? await liveStatusesOverride() : live;
    if (command === "recording_statuses") return recording;
    if (command === "recording_intent") return intent;
    if (command === "live_keepalive") return undefined;
    if (command === "ptz_capabilities") {
      const cameraId = (args as { cameraId: string }).cameraId;
      return {
        camera_id: cameraId, configured: false, ptz_supported: false,
        pan_tilt_supported: false, zoom_supported: false, state: null, error: null,
      };
    }
    if (command === "live_open") {
      const cameraId = (args as { cameraId: string }).cameraId;
      const opened = openOverride
        ? await openOverride(cameraId)
        : {
            session_id: `00000000-0000-4000-8000-${String(++sequence).padStart(12, "0")}`,
            camera_id: cameraId,
            url: `http://127.0.0.1:43100/live/00000000-0000-4000-8000-${String(sequence).padStart(12, "0")}`,
            state: "starting" as const,
          };
      live = [
        ...live.filter((status) => status.camera_id !== cameraId),
        stateForCamera?.(cameraId, opened.session_id) ?? {
          session_id: opened.session_id,
          camera_id: cameraId,
          state: "live",
          failure_category: null,
          reconnect_attempt: 0,
        },
      ];
      return opened;
    }
    if (command === "live_close") {
      const sessionId = (args as { sessionId: string }).sessionId;
      live = live.filter((status) => status.session_id !== sessionId);
      return undefined;
    }
    if (command === "recording_start") {
      const cameraId = (args as { cameraId: string }).cameraId;
      intent = { camera_ids: [...new Set([...intent.camera_ids, cameraId])] };
      recording = recording.map((status) =>
        status.camera_id === cameraId ? { ...status, state: "recording" } : status,
      );
      return recording.find((status) => status.camera_id === cameraId);
    }
    if (command === "recording_stop") {
      const cameraId = (args as { cameraId: string }).cameraId;
      intent = { camera_ids: intent.camera_ids.filter((id) => id !== cameraId) };
      recording = recording.map((status) =>
        status.camera_id === cameraId ? { ...status, state: "stopped" } : status,
      );
      return recording.find((status) => status.camera_id === cameraId);
    }
    throw new Error(`unexpected command ${command}`);
  });

  return {
    setLiveState(cameraId: string, state: LiveStatus["state"], reconnectAttempt: number) {
      live = live.map((status) =>
        status.camera_id === cameraId
          ? {
              ...status,
              state,
              reconnect_attempt: reconnectAttempt,
              failure_category: state === "backoff" ? "source_open_failed" : null,
            }
          : status,
      );
    },
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function liveOpen(cameraId: string, sequence: number): LiveOpenDto {
  const sessionId = `00000000-0000-4000-8000-${String(sequence).padStart(12, "0")}`;
  return {
    session_id: sessionId,
    camera_id: cameraId,
    url: `http://127.0.0.1:43100/live/${sessionId}`,
    state: "starting",
  };
}

beforeEach(() => {
  vi.mocked(invoke).mockReset();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("LiveViewScreen", () => {
  it("selects configured cameras and exposes only opaque localhost media URLs", async () => {
    installDesktop();
    render(<LiveViewScreen />);

    expect(await screen.findByText("No live cameras selected")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));

    expect(await screen.findByRole("article", { name: "Front door live camera" })).toBeTruthy();
    await waitFor(() => {
      const video = document.querySelector("video") as HTMLVideoElement | null;
      expect(video).toBeTruthy();
      expect(video?.src).toContain("http://127.0.0.1:43100/live/");
      expect(video?.src).not.toContain("rtsp://");
      expect(document.body.textContent).not.toContain("192.168.1.50");
    });
  });

  it("shows one failed camera without taking down another live tile", async () => {
    installDesktop((cameraId, sessionId) => ({
      session_id: sessionId,
      camera_id: cameraId,
      state: cameraId === front.camera_id ? "failed" : "live",
      failure_category: cameraId === front.camera_id ? "source_open_failed" : null,
      reconnect_attempt: 0,
    }));
    render(<LiveViewScreen />);

    await screen.findByText("No live cameras selected");
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    await screen.findByRole("article", { name: "Front door live camera" });

    await waitFor(() => expect(screen.getByRole("button", { name: "Retry live" })).toBeTruthy());
    expect(screen.getByText("source_open_failed")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    expect(await screen.findByRole("article", { name: "Garage live camera" })).toBeTruthy();
    await waitFor(() => {
      const garageTile = screen.getByRole("article", { name: "Garage live camera" });
      expect(garageTile.querySelector("video")).toBeTruthy();
    });
    expect(screen.getByRole("article", { name: "Front door live camera" })).toBeTruthy();
  });

  it("uses existing recording commands without closing a healthy live session", async () => {
    installDesktop();
    render(<LiveViewScreen />);

    await screen.findByText("No live cameras selected");
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    const tile = await screen.findByRole("article", { name: "Front door live camera" });
    await waitFor(() => expect(tile.querySelector("video")).toBeTruthy());

    fireEvent.click(screen.getByRole("button", { name: "Start recording" }));
    await waitFor(() => {
      expect(vi.mocked(invoke).mock.calls.some(([command]) => command === "recording_start")).toBe(true);
      expect(screen.getByRole("button", { name: "Stop recording" })).toBeTruthy();
    });
    expect(vi.mocked(invoke).mock.calls.some(([command]) => command === "live_close")).toBe(false);
    expect(tile.querySelector("video")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Stop recording" }));
    await waitFor(() => {
      expect(vi.mocked(invoke).mock.calls.some(([command]) => command === "recording_stop")).toBe(true);
    });
    expect(tile.querySelector("video")).toBeTruthy();
  });

  it("remounts media after an independent reconnect cycle", async () => {
    const desktop = installDesktop();
    render(<LiveViewScreen />);

    await screen.findByText("No live cameras selected");
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    const tile = await screen.findByRole("article", { name: "Front door live camera" });
    const firstVideo = await waitFor(() => {
      const video = tile.querySelector("video");
      expect(video).toBeTruthy();
      return video as HTMLVideoElement;
    });

    desktop.setLiveState(front.camera_id, "backoff", 1);
    await waitFor(
      () => {
        expect(screen.getByText(/Reconnecting · attempt 1/)).toBeTruthy();
        expect(tile.querySelector("video")).toBeNull();
      },
      { timeout: 2_500 },
    );

    desktop.setLiveState(front.camera_id, "live", 1);
    await waitFor(
      () => {
        const reconnected = tile.querySelector("video");
        expect(reconnected).toBeTruthy();
        expect(reconnected).not.toBe(firstVideo);
      },
      { timeout: 2_500 },
    );
  });

  it("removes a pending camera immediately and closes the late live_open result without owning it", async () => {
    const pending = deferred<LiveOpenDto>();
    let keepaliveTick: (() => void) | undefined;
    const realSetInterval = window.setInterval.bind(window);
    vi.spyOn(window, "setInterval").mockImplementation((handler, timeout, ...args) => {
      if (timeout === 30_000 && typeof handler === "function") {
        keepaliveTick = () => handler(...args);
      }
      return realSetInterval(handler, timeout, ...args);
    });
    installDesktop(undefined, async () => pending.promise);
    const view = render(<LiveViewScreen />);

    await screen.findByText("No live cameras selected");
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    const tile = await screen.findByRole("article", { name: "Front door live camera" });
    expect(tile.querySelector("video")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(screen.queryByRole("article", { name: "Front door live camera" })).toBeNull());

    const late = liveOpen(front.camera_id, 41);
    pending.resolve(late);
    await waitFor(() => {
      expect(
        vi.mocked(invoke).mock.calls.some(
          ([command, args]) => command === "live_close" && (args as { sessionId?: string })?.sessionId === late.session_id,
        ),
      ).toBe(true);
    });
    expect(document.querySelector("video")).toBeNull();
    expect(keepaliveTick).toBeTruthy();
    keepaliveTick?.();
    await Promise.resolve();
    expect(
      vi.mocked(invoke).mock.calls.some(
        ([command, args]) => command === "live_keepalive" && (args as { sessionId?: string })?.sessionId === late.session_id,
      ),
    ).toBe(false);

    const closesBeforeUnmount = vi.mocked(invoke).mock.calls.filter(
      ([command, args]) => command === "live_close" && (args as { sessionId?: string })?.sessionId === late.session_id,
    ).length;
    view.unmount();
    await Promise.resolve();
    const closesAfterUnmount = vi.mocked(invoke).mock.calls.filter(
      ([command, args]) => command === "live_close" && (args as { sessionId?: string })?.sessionId === late.session_id,
    ).length;
    expect(closesAfterUnmount).toBe(closesBeforeUnmount);
  });

  it("closes every late session after unmount while two live_open calls are pending", async () => {
    const pendingFront = deferred<LiveOpenDto>();
    const pendingGarage = deferred<LiveOpenDto>();
    installDesktop(undefined, async (cameraId) =>
      cameraId === front.camera_id ? pendingFront.promise : pendingGarage.promise,
    );
    const view = render(<LiveViewScreen />);

    await screen.findByText("No live cameras selected");
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    await screen.findByRole("article", { name: "Front door live camera" });
    await waitFor(() => {
      const picker = screen.getByRole("combobox", { name: "Camera to add" }) as HTMLSelectElement;
      expect(picker.value).toBe(garage.camera_id);
    });
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    await screen.findByRole("article", { name: "Garage live camera" });
    view.unmount();

    const lateFront = liveOpen(front.camera_id, 51);
    const lateGarage = liveOpen(garage.camera_id, 52);
    pendingFront.resolve(lateFront);
    pendingGarage.resolve(lateGarage);
    await waitFor(() => {
      const closes = vi.mocked(invoke).mock.calls
        .filter(([command]) => command === "live_close")
        .map(([, args]) => (args as { sessionId?: string })?.sessionId);
      expect(closes).toContain(lateFront.session_id);
      expect(closes).toContain(lateGarage.session_id);
    });
  });

  it("queues a newer generation behind a pending open and never lets the stale result replace it", async () => {
    const first = deferred<LiveOpenDto>();
    const second = deferred<LiveOpenDto>();
    let opens = 0;
    installDesktop(undefined, async () => {
      opens += 1;
      return opens === 1 ? first.promise : second.promise;
    });
    render(<LiveViewScreen />);

    await screen.findByText("No live cameras selected");
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    await screen.findByRole("article", { name: "Front door live camera" });
    fireEvent.click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() => expect(screen.queryByRole("article", { name: "Front door live camera" })).toBeNull());
    const picker = screen.getByRole("combobox", { name: "Camera to add" }) as HTMLSelectElement;
    fireEvent.change(picker, { target: { value: front.camera_id } });
    expect(picker.value).toBe(front.camera_id);
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    await screen.findByRole("article", { name: "Front door live camera" });
    expect(opens).toBe(1);

    const stale = liveOpen(front.camera_id, 61);
    first.resolve(stale);
    await waitFor(() => expect(opens).toBe(2));
    await waitFor(() => {
      expect(
        vi.mocked(invoke).mock.calls.some(
          ([command, args]) => command === "live_close" && (args as { sessionId?: string })?.sessionId === stale.session_id,
        ),
      ).toBe(true);
    });

    const fresh = liveOpen(front.camera_id, 62);
    second.resolve(fresh);
    const tile = await screen.findByRole("article", { name: "Front door live camera" });
    await waitFor(() => {
      const video = tile.querySelector("video") as HTMLVideoElement | null;
      expect(video?.src).toContain(fresh.session_id);
      expect(video?.src).not.toContain(stale.session_id);
    });
  });


  it("keeps aggregate status polling single-flight and recovers after resolve or reject", async () => {
    const first = deferred<LiveStatus[]>();
    const second = deferred<LiveStatus[]>();
    const third = deferred<LiveStatus[]>();
    const responses = [first, second, third];
    let calls = 0;
    let statusTick: (() => void) | undefined;
    const realSetInterval = window.setInterval.bind(window);
    vi.spyOn(window, "setInterval").mockImplementation((handler, timeout, ...args) => {
      if (timeout === 1_000 && typeof handler === "function") {
        statusTick = () => handler(...args);
      }
      return realSetInterval(handler, timeout, ...args);
    });
    installDesktop(undefined, undefined, async () => {
      const response = responses[calls];
      calls += 1;
      if (!response) return [];
      return response.promise;
    });
    const view = render(<LiveViewScreen />);

    await waitFor(() => expect(calls).toBe(1));
    expect(statusTick).toBeTruthy();
    statusTick?.();
    statusTick?.();
    statusTick?.();
    await Promise.resolve();
    expect(calls).toBe(1);

    first.resolve([]);
    await waitFor(() => expect(screen.queryByText("Loading live view…")).toBeNull());
    statusTick?.();
    await waitFor(() => expect(calls).toBe(2));

    second.reject(new Error("status failed"));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("Desktop operation failed."));
    statusTick?.();
    await waitFor(() => expect(calls).toBe(3));

    view.unmount();
    third.resolve([]);
    await Promise.resolve();
    await Promise.resolve();
    statusTick?.();
    await Promise.resolve();
    expect(calls).toBe(3);
  });

  it("closes the backend session on media failure and unmount", async () => {
    installDesktop();
    const view = render(<LiveViewScreen />);

    await screen.findByText("No live cameras selected");
    fireEvent.click(screen.getByRole("button", { name: "Add to live view" }));
    await screen.findByRole("article", { name: "Front door live camera" });
    const video = await waitFor(() => {
      const found = document.querySelector("video") as HTMLVideoElement | null;
      expect(found).toBeTruthy();
      return found as HTMLVideoElement;
    });
    fireEvent.error(video);
    await waitFor(() => {
      expect(screen.getByText(/live media element failed/i)).toBeTruthy();
      expect(vi.mocked(invoke).mock.calls.some(([command]) => command === "live_close")).toBe(true);
    });

    fireEvent.click(screen.getByRole("button", { name: "Retry live" }));
    await waitFor(() => expect(document.querySelector("video")).toBeTruthy());
    const closesBeforeUnmount = vi.mocked(invoke).mock.calls.filter(([command]) => command === "live_close").length;
    view.unmount();
    await waitFor(() => {
      const closesAfterUnmount = vi.mocked(invoke).mock.calls.filter(([command]) => command === "live_close").length;
      expect(closesAfterUnmount).toBeGreaterThan(closesBeforeUnmount);
    });
  });
});
