import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { VideoPlayer } from "./VideoPlayer";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("VideoPlayer", () => {
  it("uses application controls instead of native browser controls and exposes metadata", () => {
    render(
      <VideoPlayer
        src="http://127.0.0.1:43100/playback/test"
        metadata={[
          { label: "Camera", value: "Front door" },
          { label: "Video", value: "H264" },
        ]}
      />,
    );

    const video = document.querySelector("video") as HTMLVideoElement;
    expect(video).toBeTruthy();
    expect(video.hasAttribute("controls")).toBe(false);
    expect(screen.getAllByRole("button", { name: "Play video" })).toHaveLength(2);
    expect(screen.getByRole("slider", { name: "Video position" })).toBeTruthy();
    expect(screen.getByRole("slider", { name: "Video volume" })).toBeTruthy();
    expect(screen.getByRole("button", { name: "Enter fullscreen" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Video metadata" }));
    expect(screen.getAllByRole("definition").some((entry) => entry.textContent === "Front door")).toBe(true);
    expect(screen.getByText("H264")).toBeTruthy();
  });

  it("seeks to event pre-roll on loaded metadata and supports keyboard seeking", () => {
    render(<VideoPlayer src="http://127.0.0.1:43100/playback/event" initialTimeSeconds={5} />);
    const player = screen.getByLabelText("Video playback");
    const video = document.querySelector("video") as HTMLVideoElement;
    Object.defineProperty(video, "duration", { configurable: true, value: 30 });
    video.currentTime = 0;

    fireEvent.loadedMetadata(video);
    expect(video.currentTime).toBe(5);

    fireEvent.keyDown(player, { key: "ArrowRight" });
    expect(video.currentTime).toBe(10);
    fireEvent.keyDown(player, { key: "ArrowLeft" });
    expect(video.currentTime).toBe(5);
  });

  it("requests fullscreen from the player surface", async () => {
    render(<VideoPlayer src="http://127.0.0.1:43100/playback/fullscreen" />);
    const player = screen.getByLabelText("Video playback") as HTMLDivElement & { requestFullscreen: () => Promise<void> };
    const requestFullscreen = vi.fn().mockResolvedValue(undefined);
    player.requestFullscreen = requestFullscreen;
    fireEvent.click(screen.getByRole("button", { name: "Enter fullscreen" }));
    await Promise.resolve();
    expect(requestFullscreen).toHaveBeenCalledTimes(1);
  });
});
