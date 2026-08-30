import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { TimelineScreen } from "./TimelineScreen";
import type { CameraSummary, PlaybackOpenDto, RecordingDto } from "../lib/tauri";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const camera: CameraSummary = {
  camera_id: "front-door",
  display_name: "Front door",
  host: "192.168.1.50",
  port: 554,
  path: "/stream1",
  audio_policy: "copy_all",
};

const first: RecordingDto = {
  recording_id: "front-door/2026/08/29/08-00-00.mkv",
  camera_id: camera.camera_id,
  kind: "normal",
  started_at: "2026-08-29T08:00:00.000",
  sequence: 1,
  size_bytes: 1_000,
  media_duration_ms: 60_000,
  end_at: "2026-08-29T08:01:00.000",
};

const recovered: RecordingDto = {
  recording_id: "front-door/2026/08/29/08-05-00-2.recovered.mkv",
  camera_id: camera.camera_id,
  kind: "recovered",
  started_at: "2026-08-29T08:05:00.000",
  sequence: 2,
  size_bytes: 2_000,
  media_duration_ms: null,
  end_at: null,
};

function installDesktop(timeline: RecordingDto[] = [first, recovered]) {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(invoke).mockImplementation(async (command) => {
    if (command === "camera_list") return [camera];
    if (command === "recording_days") return ["2026-08-29"];
    if (command === "recording_timeline") return timeline;
    if (command === "playback_close") return undefined;
    throw new Error(`unexpected command ${command}`);
  });
}

beforeEach(() => {
  vi.mocked(invoke).mockReset();
});

afterEach(() => {
  cleanup();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("TimelineScreen", () => {
  it("renders an honest empty recordings state", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_days") return [];
      throw new Error(`unexpected command ${command}`);
    });
    render(<TimelineScreen />);
    expect(await screen.findByText("No recordings yet")).toBeTruthy();
  });

  it("renders stable ordering, an explicit known gap, recovered footage and unknown duration", async () => {
    installDesktop();
    render(<TimelineScreen />);

    expect(await screen.findByText("2 finalized recordings")).toBeTruthy();
    const timeline = screen.getByLabelText("Recording timeline");
    expect(timeline.textContent).toContain("08:00:00");
    expect(timeline.textContent).toContain("08:05:00");
    expect(timeline.textContent?.indexOf("08:00:00")).toBeLessThan(
      timeline.textContent?.indexOf("08:05:00") ?? 0,
    );
    expect(screen.getByText("4m gap")).toBeTruthy();
    expect(screen.getByText("Recovered")).toBeTruthy();
    expect(screen.getByText("Duration unknown")).toBeTruthy();
  });

  it("opens a recording and exposes adjacent navigation without hiding EOF gaps", async () => {
    installDesktop();
    const opened: PlaybackOpenDto = {
      session_id: "11111111-1111-4111-8111-111111111111",
      url: "http://127.0.0.1:43210/playback/11111111-1111-4111-8111-111111111111",
      recording: first,
      inspect: {
        duration_ms: 60_000,
        video_codec: "h264",
        width: 1920,
        height: 1080,
        audio_available: true,
        container_compatibility: "fragmented_mp4",
        seekable: true,
      },
      adjacent: { previous: null, next: recovered },
    };
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_days") return ["2026-08-29"];
      if (command === "recording_timeline") return [first, recovered];
      if (command === "playback_open") return opened;
      if (command === "playback_close") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<TimelineScreen />);
    await screen.findByText("2 finalized recordings");
    fireEvent.click(screen.getByRole("button", { name: /08:00:00/ }));
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    await waitFor(() => expect(document.querySelector("video")?.getAttribute("src")).toBe(opened.url));
    expect((screen.getByRole("button", { name: "Previous recording" }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole("button", { name: "Next recording" }) as HTMLButtonElement).disabled).toBe(false);
    fireEvent.ended(document.querySelector("video") as HTMLVideoElement);
    expect(screen.getByText(/Recording ended/)).toBeTruthy();
  });

  it("refreshes after stale playback errors and removes recordings deleted by retention", async () => {
    installDesktop([first]);
    let timelineCalls = 0;
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [camera];
      if (command === "recording_days") return ["2026-08-29"];
      if (command === "recording_timeline") {
        timelineCalls += 1;
        return timelineCalls >= 3 ? [] : [first];
      }
      if (command === "playback_open") {
        throw { code: "recording_stale", message: "Recording changed on disk." };
      }
      if (command === "playback_close") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<TimelineScreen />);
    await screen.findByText("1 finalized recording");
    fireEvent.click(screen.getByRole("button", { name: /08:00:00/ }));
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("Recording changed on disk.");
    await waitFor(() => expect(timelineCalls).toBeGreaterThanOrEqual(2));

    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(await screen.findByText("No recordings on this day")).toBeTruthy();
  });
});
