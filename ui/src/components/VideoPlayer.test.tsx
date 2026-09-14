import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { VideoPlayer } from "./VideoPlayer";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("VideoPlayer", () => {
  it("mounts the Sutro Media Chrome theme instead of native browser controls and exposes NVR metadata", async () => {
    render(
      <VideoPlayer
        src="http://127.0.0.1:43100/playback/test"
        metadata={[
          { label: "Camera", value: "Front door" },
          { label: "Video", value: "H264" },
        ]}
      />,
    );

    await waitFor(() => expect(document.querySelector("video")).toBeTruthy());
    const video = document.querySelector("video") as HTMLVideoElement;
    const theme = document.querySelector("media-theme-sutro");
    expect(theme).toBeTruthy();
    expect(video.closest("media-theme-sutro")).toBe(theme);
    expect(video.hasAttribute("controls")).toBe(false);
    expect(video.getAttribute("slot")).toBe("media");

    fireEvent.click(screen.getByRole("button", { name: "Metadata" }));
    expect(screen.getAllByRole("definition").some((entry) => entry.textContent === "Front door")).toBe(true);
    expect(screen.getByText("H264")).toBeTruthy();
    expect(screen.getByRole("button", { name: "Close video metadata" })).toBeTruthy();
  });

  it("seeks to the event start after dedicated pre-roll metadata loads", async () => {
    render(<VideoPlayer src="http://127.0.0.1:43100/playback/event" initialTimeSeconds={5} />);
    await waitFor(() => expect(document.querySelector("video")).toBeTruthy());
    const video = document.querySelector("video") as HTMLVideoElement;
    Object.defineProperty(video, "duration", { configurable: true, value: 30 });
    video.currentTime = 0;

    fireEvent.loadedMetadata(video);
    expect(video.currentTime).toBe(5);
    expect(screen.getByText("Motion begins at 0:05")).toBeTruthy();
    expect(screen.getByText("0:30 clip")).toBeTruthy();
  });

  it("forwards ended and error events from the media element", async () => {
    const onEnded = vi.fn();
    const onError = vi.fn();
    render(
      <VideoPlayer
        src="http://127.0.0.1:43100/playback/events"
        onEnded={onEnded}
        onError={onError}
      />,
    );
    await waitFor(() => expect(document.querySelector("video")).toBeTruthy());
    const video = document.querySelector("video") as HTMLVideoElement;

    fireEvent.ended(video);
    fireEvent.error(video);
    expect(onEnded).toHaveBeenCalledTimes(1);
    expect(onError).toHaveBeenCalledTimes(1);
  });

  it("resets clip-specific metadata UI when the source changes", async () => {
    const view = render(
      <VideoPlayer
        src="http://127.0.0.1:43100/playback/first"
        metadata={[{ label: "Camera", value: "Front door" }]}
      />,
    );
    await waitFor(() => expect(document.querySelector("video")).toBeTruthy());
    fireEvent.click(screen.getByRole("button", { name: "Metadata" }));
    expect(screen.getByRole("region", { name: "Video metadata" })).toBeTruthy();

    view.rerender(
      <VideoPlayer
        src="http://127.0.0.1:43100/playback/second"
        metadata={[{ label: "Camera", value: "Garage" }]}
      />,
    );
    expect(screen.queryByRole("region", { name: "Video metadata" })).toBeNull();
    await waitFor(() => expect((document.querySelector("video") as HTMLVideoElement | null)?.src).toContain("/playback/second"));
  });
});
