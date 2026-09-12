import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { invoke } from "@tauri-apps/api/core";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { EventReviewScreen } from "./EventReviewScreen";
import type { EventReviewPage, EventReviewRow } from "../lib/tauri";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const eventA: EventReviewRow = {
  event_id: 1,
  camera_id: "cam-a",
  camera_display_name: "Front Door",
  kind: "motion_started",
  device_time_utc: null,
  received_time_utc: "2026-09-05T12:00:00Z",
  recording_available: true,
};

const eventB: EventReviewRow = {
  event_id: 2,
  camera_id: "cam-b",
  camera_display_name: "Back Door",
  kind: "motion_started",
  device_time_utc: null,
  received_time_utc: "2026-09-05T12:01:00Z",
  recording_available: false,
};

function installDesktop() {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((next) => {
    resolve = next;
  });
  return { promise, resolve };
}

beforeEach(() => {
  vi.mocked(invoke).mockReset();
  installDesktop();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
  Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
});

describe("EventReviewScreen", () => {
  it("loads a bounded last-24-hours query by default and applies camera/event filtering", async () => {
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") {
        return [
          { camera_id: "cam-a", display_name: "Front Door", host: "10.0.0.1", port: 554, path: "/a", audio_policy: "exclude" },
        ];
      }
      if (command === "event_query") return { rows: [eventA], next_cursor: null } satisfies EventReviewPage;
      throw new Error(`unexpected command ${command}`);
    });

    render(<EventReviewScreen />);
    expect(await screen.findByText("Motion detected")).toBeTruthy();

    const firstQuery = vi.mocked(invoke).mock.calls.find(([command]) => command === "event_query");
    expect(firstQuery).toBeTruthy();
    const input = (firstQuery?.[1] as { input: { camera_ids: string[]; kind: string | null; from_utc: string; to_utc: string; limit: number } }).input;
    expect(input.camera_ids).toEqual([]);
    expect(input.kind).toBeNull();
    expect(input.limit).toBe(50);
    const rangeMs = new Date(input.to_utc).getTime() - new Date(input.from_utc).getTime();
    expect(Math.abs(rangeMs - 24 * 60 * 60_000)).toBeLessThan(2_000);

    fireEvent.change(screen.getByLabelText("Camera"), { target: { value: "cam-a" } });
    await waitFor(() => {
      expect(vi.mocked(invoke).mock.calls.some(([command, args]) =>
        command === "event_query"
        && JSON.stringify((args as { input?: { camera_ids?: string[] } })?.input?.camera_ids) === JSON.stringify(["cam-a"]),
      )).toBe(true);
    });

    fireEvent.change(screen.getByLabelText("Event type"), { target: { value: "motion_ended" } });
    await waitFor(() => {
      expect(vi.mocked(invoke).mock.calls.some(([command, args]) =>
        command === "event_query"
        && (args as { input?: { kind?: string | null } })?.input?.kind === "motion_ended",
      )).toBe(true);
    });
  });

  it("periodic root refresh invalidates an older in-flight pagination response", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-05T12:00:00Z"));
    const oldPage = deferred<EventReviewPage>();
    let rootQueryCount = 0;
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [];
      if (command === "event_query") {
        const input = (args as { input: { cursor: string | null } }).input;
        if (input.cursor === "cursor-old") return oldPage.promise;
        if (input.cursor === "cursor-new") return { rows: [], next_cursor: null } satisfies EventReviewPage;
        rootQueryCount += 1;
        return rootQueryCount === 1
          ? { rows: [eventA], next_cursor: "cursor-old" } satisfies EventReviewPage
          : { rows: [eventB], next_cursor: "cursor-new" } satisfies EventReviewPage;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<EventReviewScreen />);
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(screen.getByRole("button", { name: /Front Door.*Motion detected/ })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });

    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    expect(screen.getByRole("button", { name: /Back Door.*Motion detected/ })).toBeTruthy();

    const staleRow = { ...eventA, event_id: 30, camera_display_name: "Stale Poll Page" };
    oldPage.resolve({ rows: [staleRow], next_cursor: "cursor-stale" });
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(screen.queryByText("Stale Poll Page")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(vi.mocked(invoke).mock.calls.some(([command, args]) =>
      command === "event_query"
      && (args as { input?: { cursor?: string | null } })?.input?.cursor === "cursor-new",
    )).toBe(true);
  });

  it("manual root refresh invalidates an older in-flight pagination response", async () => {
    const oldPage = deferred<EventReviewPage>();
    let rootQueryCount = 0;
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [];
      if (command === "event_query") {
        const input = (args as { input: { cursor: string | null } }).input;
        if (input.cursor === "cursor-old") return oldPage.promise;
        if (input.cursor === "cursor-new") return { rows: [], next_cursor: null } satisfies EventReviewPage;
        rootQueryCount += 1;
        return rootQueryCount === 1
          ? { rows: [eventA], next_cursor: "cursor-old" } satisfies EventReviewPage
          : { rows: [eventB], next_cursor: "cursor-new" } satisfies EventReviewPage;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<EventReviewScreen />);
    await screen.findByRole("button", { name: /Front Door.*Motion detected/ });
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    expect(await screen.findByRole("button", { name: /Back Door.*Motion detected/ })).toBeTruthy();

    const staleRow = { ...eventA, event_id: 31, camera_display_name: "Stale Manual Page" };
    oldPage.resolve({ rows: [staleRow], next_cursor: "cursor-stale" });
    await act(async () => { await Promise.resolve(); });
    await waitFor(() => expect(screen.queryByText("Stale Manual Page")).toBeNull());

    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    await waitFor(() => expect(vi.mocked(invoke).mock.calls.some(([command, args]) =>
      command === "event_query"
      && (args as { input?: { cursor?: string | null } })?.input?.cursor === "cursor-new",
    )).toBe(true));
  });

  it("freezes relative query bounds for every page in one dataset generation", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-05T12:00:00Z"));
    const queries: Array<{ cursor: string | null; from_utc: string; to_utc: string }> = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [];
      if (command === "event_query") {
        const input = (args as { input: { cursor: string | null; from_utc: string; to_utc: string } }).input;
        queries.push({ cursor: input.cursor, from_utc: input.from_utc, to_utc: input.to_utc });
        return input.cursor
          ? { rows: [], next_cursor: null } satisfies EventReviewPage
          : { rows: [eventA], next_cursor: "cursor-a" } satisfies EventReviewPage;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<EventReviewScreen />);
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(queries).toHaveLength(1);
    const root = queries[0]!;

    vi.setSystemTime(new Date("2026-09-05T13:00:00Z"));
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(queries).toHaveLength(2);
    expect(queries[1]).toEqual({
      cursor: "cursor-a",
      from_utc: root.from_utc,
      to_utc: root.to_utc,
    });
  });

  it("keeps root reloads single-flight and coalesces to the newest queued dataset", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-05T12:00:00Z"));
    const firstRoot = deferred<EventReviewPage>();
    const roots: Array<{ to_utc: string; cursor: string | null }> = [];
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [];
      if (command === "event_query") {
        const input = (args as { input: { cursor: string | null; to_utc: string } }).input;
        roots.push({ cursor: input.cursor, to_utc: input.to_utc });
        if (roots.length === 1) return firstRoot.promise;
        return { rows: [eventB], next_cursor: null } satisfies EventReviewPage;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<EventReviewScreen />);
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(roots).toHaveLength(1);

    await act(async () => { await vi.advanceTimersByTimeAsync(20_000); });
    expect(roots).toHaveLength(1);
    firstRoot.resolve({ rows: [eventA], next_cursor: null });
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });

    expect(roots).toHaveLength(2);
    expect(roots[1]!.to_utc).toBe("2026-09-05T12:00:20.000Z");
    expect(screen.getByRole("button", { name: /Back Door.*Motion detected/ })).toBeTruthy();
  });

  it("ignores stale selected-event recording lookup completion", async () => {
    const firstLookup = deferred<{ available: boolean; camera_id: string; seek_offset_ms: number | null }>();
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") return [];
      if (command === "event_query") return { rows: [eventA, eventB], next_cursor: null } satisfies EventReviewPage;
      if (command === "event_recording_context") {
        const eventId = (args as { eventId: number }).eventId;
        if (eventId === 1) return firstLookup.promise;
        return { available: false, camera_id: "cam-b", seek_offset_ms: null };
      }
      if (command === "playback_close") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<EventReviewScreen />);
    await screen.findByText("Back Door");
    const rows = screen.getAllByRole("button", { name: /Motion detected/ });
    fireEvent.click(rows[0]!);
    fireEvent.click(rows[1]!);

    expect((await screen.findAllByText("No recording available yet. Motion-triggered clips appear here after the segment is finalized.")).length).toBeGreaterThan(0);
    firstLookup.resolve({ available: true, camera_id: "cam-a", seek_offset_ms: 5_000 });
    await Promise.resolve();
    expect(screen.queryByRole("button", { name: "Open recording" })).toBeNull();
    expect(screen.getAllByText("No recording available yet. Motion-triggered clips appear here after the segment is finalized.").length).toBeGreaterThan(0);
  });


  it("refreshes selected recording context when a motion clip finishes indexing", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-09-05T12:00:00Z"));
    let queryCount = 0;
    let contextCount = 0;
    const pendingEvent = { ...eventA, recording_available: false };
    const readyEvent = { ...eventA, recording_available: true };
    vi.mocked(invoke).mockImplementation(async (command) => {
      if (command === "camera_list") return [];
      if (command === "event_query") {
        queryCount += 1;
        return {
          rows: [queryCount === 1 ? pendingEvent : readyEvent],
          next_cursor: null,
        } satisfies EventReviewPage;
      }
      if (command === "event_recording_context") {
        contextCount += 1;
        return contextCount === 1
          ? { available: false, camera_id: "cam-a", seek_offset_ms: null }
          : { available: true, camera_id: "cam-a", seek_offset_ms: 0 };
      }
      if (command === "playback_close") return undefined;
      throw new Error(`unexpected command ${command}`);
    });

    render(<EventReviewScreen />);
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    fireEvent.click(screen.getByRole("button", { name: /Front Door.*Motion detected/ }));
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(screen.getByText(/No recording available yet/)).toBeTruthy();

    await act(async () => { await vi.advanceTimersByTimeAsync(10_000); });
    await act(async () => { await vi.advanceTimersByTimeAsync(0); });
    expect(screen.getByRole("button", { name: "Open recording" })).toBeTruthy();
    expect(contextCount).toBe(2);
  });

  it("does not append an old page after filters change", async () => {
    const oldPage = deferred<EventReviewPage>();
    vi.mocked(invoke).mockImplementation(async (command, args) => {
      if (command === "camera_list") {
        return [
          { camera_id: "cam-a", display_name: "Front Door", host: "10.0.0.1", port: 554, path: "/a", audio_policy: "exclude" },
          { camera_id: "cam-b", display_name: "Back Door", host: "10.0.0.2", port: 554, path: "/b", audio_policy: "exclude" },
        ];
      }
      if (command === "event_query") {
        const input = (args as { input: { cursor: string | null; camera_ids: string[] } }).input;
        if (input.cursor === "cursor-a") return oldPage.promise;
        if (input.camera_ids[0] === "cam-b") return { rows: [eventB], next_cursor: null } satisfies EventReviewPage;
        return { rows: [eventA], next_cursor: "cursor-a" } satisfies EventReviewPage;
      }
      throw new Error(`unexpected command ${command}`);
    });

    render(<EventReviewScreen />);
    await screen.findByRole("button", { name: /Front Door.*Motion detected/ });
    fireEvent.click(screen.getByRole("button", { name: "Load more" }));
    fireEvent.change(screen.getByLabelText("Camera"), { target: { value: "cam-b" } });
    expect(await screen.findByRole("button", { name: /Back Door.*Motion detected/ })).toBeTruthy();

    const staleRow = { ...eventA, event_id: 3, camera_display_name: "Old Page Camera" };
    oldPage.resolve({ rows: [staleRow], next_cursor: null });
    await Promise.resolve();
    await waitFor(() => expect(screen.queryByText("Old Page Camera")).toBeNull());
  });
});
