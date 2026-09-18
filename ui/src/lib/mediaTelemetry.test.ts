import { afterEach, describe, expect, it } from "vitest";
import { readFrontendMediaDiagnostics, retainMseSession } from "./mediaTelemetry";

afterEach(() => {
  document.body.replaceChildren();
});

describe("media telemetry", () => {
  it("counts mounted video elements, active MSE sessions and buffered ranges", () => {
    const first = document.createElement("video");
    const second = document.createElement("video");
    Object.defineProperty(first, "buffered", {
      configurable: true,
      value: {
        length: 2,
        start: (index: number) => index === 0 ? 0 : 4,
        end: (index: number) => index === 0 ? 1.5 : 6,
      },
    });
    document.body.append(first, second);

    const releaseFirst = retainMseSession();
    const releaseSecond = retainMseSession();
    releaseFirst();
    releaseFirst();

    expect(readFrontendMediaDiagnostics()).toEqual({
      mountedVideoElements: 2,
      activeMseSessions: 1,
      bufferedSeconds: 3.5,
    });

    releaseSecond();
    expect(readFrontendMediaDiagnostics().activeMseSessions).toBe(0);
  });
});
