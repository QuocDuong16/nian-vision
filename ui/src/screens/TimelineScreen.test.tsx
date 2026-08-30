import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
const late: RecordingDto = {
  recording_id: "front-door/2026/08/29/23-59-00.mkv",
  camera_id: camera.camera_id,
  kind: "normal",
  started_at: "2026-08-29T23:59:00.000",
  sequence: 1,
  size_bytes: 3_000,
  media_duration_ms: 60_000,
  end_at: "2026-08-30T00:00:00.000",
};
const early: RecordingDto = {
  recording_id: "front-door/2026/08/30/00-01-00.mkv",
  camera_id: camera.camera_id,
  kind: "normal",
  started_at: "2026-08-30T00:01:00.000",
  sequence: 1,
  size_bytes: 4_000,
  media_duration_ms: 60_000,
  end_at: "2026-08-30T00:02:00.000",
};

function opened(
  recording: RecordingDto,
  sessionId: string,
  previous: RecordingDto | null,
  next: RecordingDto | null,
): PlaybackOpenDto {
  return {
    session_id: sessionId,
    url: `http://127.0.0.1:43210/playback/${sessionId}`,
    recording,
    inspect: {
      duration_ms: recording.media_duration_ms,
      video_codec: "h264",
      width: 1920,
      height: 1080,
      audio_available: true,
      container_compatibility: "fragmented_mp4",
      seekable: true,
    },
    adjacent: { previous, next },
  };
}

function installDesktop(timeline: RecordingDto[] = [first, recovered]) {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(invoke).mockImplementation(async (command) => {
    if (command === "camera_list") return [camera];
    if (command === "recordings_refresh") return undefined;
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
  vi.useRealTimers();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("TimelineScreen", () => {
  it("renders an honest empty recordings state", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [camera];
      if (command === "recordings_refresh") return undefined;
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
      if (command === "recordings_refresh") return undefined;
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
      if (command === "recordings_refresh") return undefined;
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
  it("keeps the newly opened session when Next crosses midnight", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    const lateOpen = opened(late, "22222222-2222-4222-8222-222222222222", null, early);
    const earlyOpen = opened(early, "33333333-3333-4333-8333-333333333333", late, null);
    const closed: string[] = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recordings_refresh") return undefined;
      if (command === "recording_days") return ["2026-08-29", "2026-08-30"];
      if (command === "recording_timeline") {
        return (args as { start: string }).start.startsWith("2026-08-29") ? [late] : [early];
      }
      if (command === "playback_open") {
        return (args as { recordingId: string }).recordingId === late.recording_id ? lateOpen : earlyOpen;
      }
      if (command === "playback_close") {
        closed.push((args as { sessionId: string }).sessionId);
        return undefined;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<TimelineScreen />);
    await screen.findByText("1 finalized recording");
    fireEvent.change(screen.getByRole("combobox", { name: "Recording day" }), {
      target: { value: "2026-08-29" },
    });
    await screen.findByRole("button", { name: /23:59:00/ });
    fireEvent.click(screen.getByRole("button", { name: /23:59:00/ }));
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    await waitFor(() => expect(document.querySelector("video")?.getAttribute("src")).toBe(lateOpen.url));
    fireEvent.click(screen.getByRole("button", { name: "Next recording" }));
    await waitFor(() => expect(document.querySelector("video")?.getAttribute("src")).toBe(earlyOpen.url));
    expect((screen.getByRole("combobox", { name: "Recording day" }) as HTMLSelectElement).value).toBe("2026-08-30");
    expect(screen.getByRole("button", { name: /00:01:00/ }).className).toContain("selected");
    expect(closed).toContain(lateOpen.session_id);
    expect(closed).not.toContain(earlyOpen.session_id);
  });

  it("keeps the newly opened session when Previous crosses midnight", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    const lateOpen = opened(late, "44444444-4444-4444-8444-444444444444", null, early);
    const earlyOpen = opened(early, "55555555-5555-4555-8555-555555555555", late, null);
    const closed: string[] = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recordings_refresh") return undefined;
      if (command === "recording_days") return ["2026-08-29", "2026-08-30"];
      if (command === "recording_timeline") {
        return (args as { start: string }).start.startsWith("2026-08-29") ? [late] : [early];
      }
      if (command === "playback_open") {
        return (args as { recordingId: string }).recordingId === late.recording_id ? lateOpen : earlyOpen;
      }
      if (command === "playback_close") {
        closed.push((args as { sessionId: string }).sessionId);
        return undefined;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<TimelineScreen />);
    await screen.findByRole("button", { name: /00:01:00/ });
    fireEvent.click(screen.getByRole("button", { name: /00:01:00/ }));
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    await waitFor(() => expect(document.querySelector("video")?.getAttribute("src")).toBe(earlyOpen.url));
    fireEvent.click(screen.getByRole("button", { name: "Previous recording" }));
    await waitFor(() => expect(document.querySelector("video")?.getAttribute("src")).toBe(lateOpen.url));
    expect((screen.getByRole("combobox", { name: "Recording day" }) as HTMLSelectElement).value).toBe("2026-08-29");
    expect(screen.getByRole("button", { name: /23:59:00/ }).className).toContain("selected");
    expect(closed).toContain(earlyOpen.session_id);
    expect(closed).not.toContain(lateOpen.session_id);
  });

  it("shows a recoverable error and releases the session when the media element fails", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    let openCount = 0;
    const closed: string[] = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recordings_refresh") return undefined;
      if (command === "recording_days") return ["2026-08-29"];
      if (command === "recording_timeline") return [first];
      if (command === "playback_open") {
        openCount += 1;
        return opened(first, `66666666-6666-4666-8666-66666666666${openCount}`, null, null);
      }
      if (command === "playback_close") {
        closed.push((args as { sessionId: string }).sessionId);
        return undefined;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<TimelineScreen />);
    await screen.findByRole("button", { name: /08:00:00/ });
    fireEvent.click(screen.getByRole("button", { name: /08:00:00/ }));
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    await waitFor(() => expect(document.querySelector("video")).not.toBeNull());

    const failedSrc = document.querySelector("video")?.getAttribute("src");
    fireEvent.error(document.querySelector("video") as HTMLVideoElement);
    expect(await screen.findByText(/could not be loaded or decoded/i)).toBeTruthy();
    expect(document.querySelector("video")).toBeNull();
    expect(closed).toContain(failedSrc?.split("/").at(-1));

    fireEvent.click(screen.getByRole("button", { name: "Reopen" }));
    await waitFor(() => expect(document.querySelector("video")).not.toBeNull());
    expect(openCount).toBe(2);
  });

  it("heartbeats the mounted playback session and switches heartbeat ownership to a newly opened recording", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    const firstOpen = opened(first, "77777777-7777-4777-8777-777777777777", null, recovered);
    const secondOpen = opened(recovered, "88888888-8888-4888-8888-888888888888", first, null);
    const keepalives: string[] = [];
    const closed: string[] = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recordings_refresh") return undefined;
      if (command === "recording_days") return ["2026-08-29"];
      if (command === "recording_timeline") return [first, recovered];
      if (command === "playback_open") {
        return (args as { recordingId: string }).recordingId === first.recording_id ? firstOpen : secondOpen;
      }
      if (command === "playback_keepalive") {
        keepalives.push((args as { sessionId: string }).sessionId);
        return undefined;
      }
      if (command === "playback_close") {
        closed.push((args as { sessionId: string }).sessionId);
        return undefined;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<TimelineScreen />);
    await screen.findByText("2 finalized recordings");
    fireEvent.click(screen.getByRole("button", { name: /08:00:00/ }));
    vi.useFakeTimers();
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(document.querySelector("video")?.getAttribute("src")).toBe(firstOpen.url);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(135_000);
    });
    expect(keepalives).toEqual([firstOpen.session_id, firstOpen.session_id, firstOpen.session_id]);

    fireEvent.click(screen.getByRole("button", { name: "Next recording" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(document.querySelector("video")?.getAttribute("src")).toBe(secondOpen.url);
    expect(closed).toContain(firstOpen.session_id);
    const previousHeartbeatCount = keepalives.length;

    await act(async () => {
      await vi.advanceTimersByTimeAsync(90_000);
    });
    expect(keepalives.slice(previousHeartbeatCount)).toEqual([secondOpen.session_id, secondOpen.session_id]);
    expect(closed).not.toContain(secondOpen.session_id);
  });

  it("stops heartbeats and requests close when the media element fails", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    const current = opened(first, "99999999-9999-4999-8999-999999999999", null, null);
    const keepalives: string[] = [];
    const closed: string[] = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recordings_refresh") return undefined;
      if (command === "recording_days") return ["2026-08-29"];
      if (command === "recording_timeline") return [first];
      if (command === "playback_open") return current;
      if (command === "playback_keepalive") {
        keepalives.push((args as { sessionId: string }).sessionId);
        return undefined;
      }
      if (command === "playback_close") {
        closed.push((args as { sessionId: string }).sessionId);
        return undefined;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<TimelineScreen />);
    await screen.findByText("1 finalized recording");
    fireEvent.click(screen.getByRole("button", { name: /08:00:00/ }));
    vi.useFakeTimers();
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(45_000);
    });
    expect(keepalives).toEqual([current.session_id]);

    fireEvent.error(document.querySelector("video") as HTMLVideoElement);
    expect(screen.getByText(/could not be loaded or decoded/i)).toBeTruthy();
    expect(document.querySelector("video")).toBeNull();
    expect(closed).toContain(current.session_id);

    await act(async () => {
      await vi.advanceTimersByTimeAsync(90_000);
    });
    expect(keepalives).toEqual([current.session_id]);
  });

  it("stops heartbeats and requests close when TimelineScreen unmounts", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    const current = opened(first, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", null, null);
    const keepalives: string[] = [];
    const closed: string[] = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recordings_refresh") return undefined;
      if (command === "recording_days") return ["2026-08-29"];
      if (command === "recording_timeline") return [first];
      if (command === "playback_open") return current;
      if (command === "playback_keepalive") {
        keepalives.push((args as { sessionId: string }).sessionId);
        return undefined;
      }
      if (command === "playback_close") {
        closed.push((args as { sessionId: string }).sessionId);
        return undefined;
      }
      throw new Error(`unexpected command ${command}`);
    });

    const view = render(<TimelineScreen />);
    await screen.findByText("1 finalized recording");
    fireEvent.click(screen.getByRole("button", { name: /08:00:00/ }));
    vi.useFakeTimers();
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(45_000);
    });
    expect(keepalives).toEqual([current.session_id]);

    act(() => view.unmount());
    expect(closed).toContain(current.session_id);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(90_000);
    });
    expect(keepalives).toEqual([current.session_id]);
  });

  it("clears an expired backend session and offers Reopen without reviving it", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    let openCount = 0;
    const keepalives: string[] = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [camera];
      if (command === "recordings_refresh") return undefined;
      if (command === "recording_days") return ["2026-08-29"];
      if (command === "recording_timeline") return [first];
      if (command === "playback_open") {
        openCount += 1;
        return opened(first, `bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbb${openCount}`, null, null);
      }
      if (command === "playback_keepalive") {
        keepalives.push((args as { sessionId: string }).sessionId);
        if (keepalives.length === 1) {
          throw { code: "playback_session_expired", message: "Playback session has expired." };
        }
        return undefined;
      }
      if (command === "playback_close") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<TimelineScreen />);
    await screen.findByText("1 finalized recording");
    fireEvent.click(screen.getByRole("button", { name: /08:00:00/ }));
    vi.useFakeTimers();
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(document.querySelector("video")).not.toBeNull();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(45_000);
    });
    expect(document.querySelector("video")).toBeNull();
    expect(screen.getByText("Playback session expired. Reopen the recording.")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Reopen" })).toBeTruthy();
    expect(screen.getByRole("button", { name: /08:00:00/ }).className).toContain("selected");

    fireEvent.click(screen.getByRole("button", { name: "Reopen" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(openCount).toBe(2);
    expect(document.querySelector("video")).not.toBeNull();
  });

  it("retries one-off keepalive failures and surfaces only repeated failures", async () => {
    Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
    const current = opened(first, "cccccccc-cccc-4ccc-8ccc-cccccccccccc", null, null);
    let attempts = 0;
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [camera];
      if (command === "recordings_refresh") return undefined;
      if (command === "recording_days") return ["2026-08-29"];
      if (command === "recording_timeline") return [first];
      if (command === "playback_open") return current;
      if (command === "playback_keepalive") {
        attempts += 1;
        if (attempts <= 3) {
          throw { code: "internal", message: "temporary keepalive failure" };
        }
        return undefined;
      }
      if (command === "playback_close") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<TimelineScreen />);
    await screen.findByText("1 finalized recording");
    fireEvent.click(screen.getByRole("button", { name: /08:00:00/ }));
    vi.useFakeTimers();
    fireEvent.click(screen.getByRole("button", { name: "Open recording" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    await act(async () => {
      await vi.advanceTimersByTimeAsync(45_000);
    });
    expect(screen.queryByText(/keepalive is unavailable/i)).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(45_000);
    });
    expect(screen.queryByText(/keepalive is unavailable/i)).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(45_000);
    });
    expect(screen.getByText(/keepalive is unavailable/i)).toBeTruthy();

    await act(async () => {
      await vi.advanceTimersByTimeAsync(45_000);
    });
    expect(screen.queryByText(/keepalive is unavailable/i)).toBeNull();
    expect(document.querySelector("video")).not.toBeNull();
  });

});
