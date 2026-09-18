import { afterEach, describe, expect, it, vi } from "vitest";
import { teardownLiveMse } from "./mediaLifecycle";

const originalRevokeObjectUrl = Object.getOwnPropertyDescriptor(URL, "revokeObjectURL");

afterEach(() => {
  if (originalRevokeObjectUrl) Object.defineProperty(URL, "revokeObjectURL", originalRevokeObjectUrl);
  else Reflect.deleteProperty(URL, "revokeObjectURL");
});

describe("teardownLiveMse", () => {
  it("releases SourceBuffer, MediaSource, video and object URL resources", () => {
    const calls: string[] = [];
    const sourceBuffer = {
      updating: true,
      abort: () => calls.push("abort"),
    } as unknown as SourceBuffer;
    const mediaSource = {
      readyState: "open",
      sourceBuffers: { length: 1 },
      removeSourceBuffer: (value: SourceBuffer) => {
        expect(value).toBe(sourceBuffer);
        calls.push("remove-source-buffer");
      },
      endOfStream: () => calls.push("end-of-stream"),
    } as unknown as MediaSource;
    const video = {
      pause: () => calls.push("pause"),
      removeAttribute: (name: string) => calls.push(`remove-${name}`),
      load: () => calls.push("load"),
    } as unknown as HTMLVideoElement;
    Object.defineProperty(URL, "revokeObjectURL", {
      configurable: true,
      value: (url: string) => calls.push(`revoke-${url}`),
    });

    teardownLiveMse({ mediaSource, sourceBuffer, video, objectUrl: "blob:nian-live" });

    expect(calls).toEqual([
      "pause",
      "abort",
      "remove-source-buffer",
      "end-of-stream",
      "remove-src",
      "load",
      "revoke-blob:nian-live",
    ]);
  });

  it("continues teardown when SourceBuffer abort races browser state", () => {
    const removeSourceBuffer = vi.fn();
    const endOfStream = vi.fn();
    const sourceBuffer = {
      updating: true,
      abort: () => { throw new DOMException("stale", "InvalidStateError"); },
    } as unknown as SourceBuffer;
    const mediaSource = {
      readyState: "open",
      sourceBuffers: { length: 1 },
      removeSourceBuffer,
      endOfStream,
    } as unknown as MediaSource;
    const video = {
      pause: vi.fn(),
      removeAttribute: vi.fn(),
      load: vi.fn(),
    } as unknown as HTMLVideoElement;
    const revokeObjectURL = vi.fn();
    Object.defineProperty(URL, "revokeObjectURL", { configurable: true, value: revokeObjectURL });

    expect(() => teardownLiveMse({ mediaSource, sourceBuffer, video, objectUrl: "blob:stale" })).not.toThrow();
    expect(removeSourceBuffer).toHaveBeenCalledWith(sourceBuffer);
    expect(endOfStream).toHaveBeenCalledOnce();
    expect(revokeObjectURL).toHaveBeenCalledWith("blob:stale");
  });
});
