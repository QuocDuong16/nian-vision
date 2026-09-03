import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LiveViewScreen } from "./LiveViewScreen";
import type {
  CameraSummary,
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

function installDesktop(stateForCamera?: (cameraId: string, sessionId: string) => LiveStatus) {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  let live: LiveStatus[] = [];
  let recording: RecordingStatus[] = [stopped(front.camera_id), stopped(garage.camera_id)];
  let intent: RecordingIntent = { camera_ids: [] };
  let sequence = 0;

  vi.mocked(invoke).mockImplementation(async (command, args) => {
    if (command === "camera_list") return [front, garage];
    if (command === "live_statuses") return live;
    if (command === "recording_statuses") return recording;
    if (command === "recording_intent") return intent;
    if (command === "live_keepalive") return undefined;
    if (command === "live_open") {
      const cameraId = (args as { cameraId: string }).cameraId;
      const sessionId = `00000000-0000-4000-8000-${String(++sequence).padStart(12, "0")}`;
      live = [
        ...live.filter((status) => status.camera_id !== cameraId),
        stateForCamera?.(cameraId, sessionId) ?? {
          session_id: sessionId,
          camera_id: cameraId,
          state: "live",
          failure_category: null,
          reconnect_attempt: 0,
        },
      ];
      return {
        session_id: sessionId,
        camera_id: cameraId,
        url: `http://127.0.0.1:43100/live/${sessionId}`,
        state: "starting",
      };
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

beforeEach(() => {
  vi.mocked(invoke).mockReset();
});

afterEach(() => {
  cleanup();
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
