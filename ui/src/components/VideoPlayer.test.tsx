import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { VideoPlayer } from "./VideoPlayer";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("VideoPlayer", () => {
  it("releases the media resource explicitly when unmounted", async () => {
    const pause = vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => undefined);
    const load = vi.spyOn(HTMLMediaElement.prototype, "load").mockImplementation(() => undefined);
    const view = render(<VideoPlayer src="http://127.0.0.1:43100/playback/release" />);

    const video = await waitFor(() => {
      const current = view.container.querySelector("video");
      expect(current?.getAttribute("src")).toBe("http://127.0.0.1:43100/playback/release");
      return current as HTMLVideoElement;
    });
    view.unmount();

    expect(pause).toHaveBeenCalled();
    expect(load).toHaveBeenCalled();
    expect(video?.hasAttribute("src")).toBe(false);
  });

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
    expect(screen.getByText("Starts at 0:05")).toBeTruthy();
    expect(screen.getByText("0:30 clip")).toBeTruthy();
  });

  it("highlights the real detection anchor with three seconds of context and seeks on demand", async () => {
    render(<VideoPlayer src="http://127.0.0.1:43100/playback/event" initialTimeSeconds={3} markerTimeSeconds={6} />);
    await waitFor(() => expect(document.querySelector("video")).toBeTruthy());
    const video = document.querySelector("video") as HTMLVideoElement;
    Object.defineProperty(video, "duration", { configurable: true, value: 17 });
    fireEvent.loadedMetadata(video);

    expect(video.currentTime).toBe(3);
    expect(screen.getByRole("img", { name: "Detection at 0:06, highlighted from 0:03 to 0:09" })).toBeTruthy();
    expect(screen.getByText("Detection ~0:06 · ±3s context")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Jump to detection" }));
    expect(video.currentTime).toBe(6);
  });

  it("does not invent a detection marker when the clip time is unknown", async () => {
    render(<VideoPlayer src="http://127.0.0.1:43100/playback/legacy" markerTimeSeconds={null} />);
    await waitFor(() => expect(document.querySelector("video")).toBeTruthy());
    const video = document.querySelector("video") as HTMLVideoElement;
    Object.defineProperty(video, "duration", { configurable: true, value: 17 });
    fireEvent.loadedMetadata(video);
    expect(screen.queryByRole("img", { name: /Detection at/ })).toBeNull();
    expect(screen.queryByRole("button", { name: "Jump to detection" })).toBeNull();
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

  it("synchronizes G.711 PCM sidecar playback, seeking, volume and teardown without restarting on metadata updates", async () => {
    const play = vi.spyOn(HTMLMediaElement.prototype, "play").mockResolvedValue(undefined);
    const pause = vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => undefined);
    const load = vi.spyOn(HTMLMediaElement.prototype, "load").mockImplementation(() => undefined);
    const audioSrc = "http://127.0.0.1:43100/playback/test/audio";
    const view = render(<VideoPlayer src="http://127.0.0.1:43100/playback/test" audioSrc={audioSrc} />);
    await waitFor(() => expect(view.container.querySelector("audio")?.getAttribute("src")).toBe(audioSrc));
    const video = view.container.querySelector("video") as HTMLVideoElement;
    const audio = view.container.querySelector("audio") as HTMLAudioElement;
    video.currentTime = 4;
    video.volume = 0.4;
    video.muted = true;
    video.playbackRate = 1.5;
    fireEvent.play(video);
    expect(play).not.toHaveBeenCalled(); // video is not actually rendering yet
    fireEvent.playing(video);
    expect(play).toHaveBeenCalledTimes(1);
    expect(audio.currentTime).toBe(4);
    expect(audio.volume).toBe(0.4);
    expect(audio.muted).toBe(true);
    expect(audio.playbackRate).toBe(1.5);

    Object.defineProperty(video, "duration", { configurable: true, value: 30 });
    fireEvent.durationChange(video); // React re-render must not detach audio.
    expect(audio.getAttribute("src")).toBe(audioSrc);
    video.currentTime = 10;
    fireEvent.seeked(video);
    expect(audio.currentTime).toBe(10);
    video.volume = 0.7;
    video.muted = false;
    fireEvent.volumeChange(video);
    expect(audio.volume).toBe(0.7);
    expect(audio.muted).toBe(false);
    fireEvent.waiting(video);
    expect(pause).toHaveBeenCalledTimes(1);
    fireEvent.playing(video);
    expect(play).toHaveBeenCalledTimes(2);
    fireEvent.seeking(video);
    expect(pause).toHaveBeenCalledTimes(2);
    fireEvent.playing(video);
    expect(play).toHaveBeenCalledTimes(3);
    fireEvent.stalled(video);
    expect(pause).toHaveBeenCalledTimes(3);
    fireEvent.pause(video);
    expect(pause).toHaveBeenCalledTimes(4);
    view.unmount();
    expect(audio.hasAttribute("src")).toBe(false);
    expect(load).toHaveBeenCalled();
  });

  it("reports G.711 sidecar decode failures without closing the video", async () => {
    vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(() => undefined);
    vi.spyOn(HTMLMediaElement.prototype, "load").mockImplementation(() => undefined);
    const onVideoError = vi.fn();
    const view = render(<VideoPlayer src="http://127.0.0.1:43100/playback/test" audioSrc="http://127.0.0.1:43100/playback/test/audio" onError={onVideoError} />);
    await waitFor(() => expect(view.container.querySelector("audio")).toBeTruthy());
    fireEvent.error(view.container.querySelector("audio") as HTMLAudioElement);
    expect(screen.getByRole("status").textContent).toContain("G.711 audio could not be played");
    expect(view.container.querySelector("video")?.getAttribute("src")).toContain("/playback/test");
    expect(onVideoError).not.toHaveBeenCalled();
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
