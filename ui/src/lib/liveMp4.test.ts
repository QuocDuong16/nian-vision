import { describe, expect, it } from "vitest";
import { detectLiveVideoConfiguration, detectLiveVideoMime, splitLiveMp4ForMse } from "./liveMp4";

function box(type: string, payload: number[] | Uint8Array): Uint8Array {
  const body = payload instanceof Uint8Array ? payload : Uint8Array.from(payload);
  const result = new Uint8Array(8 + body.length);
  const view = new DataView(result.buffer);
  view.setUint32(0, result.byteLength, false);
  for (let index = 0; index < 4; index += 1) result[4 + index] = type.charCodeAt(index);
  result.set(body, 8);
  return result;
}

function mfhd(sequence: number): Uint8Array {
  const payload = new Uint8Array(8);
  const view = new DataView(payload.buffer);
  view.setUint32(4, sequence, false);
  return box("mfhd", payload);
}

function moof(sequence: number): Uint8Array {
  return box("moof", mfhd(sequence));
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const output = new Uint8Array(parts.reduce((sum, part) => sum + part.byteLength, 0));
  let offset = 0;
  for (const part of parts) {
    output.set(part, offset);
    offset += part.byteLength;
  }
  return output;
}

function types(bytes: Uint8Array): string[] {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const result: string[] = [];
  let offset = 0;
  while (offset < bytes.byteLength) {
    const size = view.getUint32(offset, false);
    result.push(String.fromCharCode(...bytes.subarray(offset + 4, offset + 8)));
    offset += size;
  }
  return result;
}

function movieFragmentSequences(bytes: Uint8Array): number[] {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const result: number[] = [];
  let offset = 0;
  while (offset < bytes.byteLength) {
    const size = view.getUint32(offset, false);
    const type = String.fromCharCode(...bytes.subarray(offset + 4, offset + 8));
    if (type === "moof") {
      const childOffset = offset + 8;
      expect(String.fromCharCode(...bytes.subarray(childOffset + 4, childOffset + 8))).toBe("mfhd");
      result.push(view.getUint32(childOffset + 12, false));
    }
    offset += size;
  }
  return result;
}

describe("splitLiveMp4ForMse", () => {
  it("keeps init once, strips indexes, and makes movie-fragment sequences continuous", () => {
    const completeFragment = concat(
      box("ftyp", [1]),
      box("moov", [2]),
      box("sidx", [3]),
      moof(1),
      box("mdat", [5]),
      moof(2),
      box("mdat", [7]),
      box("mfra", [8]),
    );

    const split = splitLiveMp4ForMse(completeFragment, 41);
    expect(split).not.toBeNull();
    expect(types(split!.initialization)).toEqual(["ftyp", "moov"]);
    expect(types(split!.media)).toEqual(["moof", "mdat", "moof", "mdat"]);
    expect(movieFragmentSequences(split!.media)).toEqual([41, 42]);
    expect(split!.movieFragmentCount).toBe(2);
    expect(split!.initialization.buffer).toBe(completeFragment.buffer);
    expect(split!.media.buffer).toBe(completeFragment.buffer);
  });

  it("rejects malformed, non-fragmented, or structurally incomplete MP4 input", () => {
    expect(splitLiveMp4ForMse(new Uint8Array([0, 0, 0, 4]))).toBeNull();
    expect(splitLiveMp4ForMse(concat(box("ftyp", []), box("moov", []), box("mdat", [])))).toBeNull();
    expect(
      splitLiveMp4ForMse(concat(box("ftyp", []), box("moov", []), box("moof", []), box("mdat", []))),
    ).toBeNull();
  });
});

function videoInitialization(entryType: string, configurationType: string, configuration: number[]): Uint8Array {
  const sampleEntry = box(entryType, concat(new Uint8Array(78), box(configurationType, configuration)));
  const sampleDescription = box("stsd", concat(new Uint8Array([0, 0, 0, 0, 0, 0, 0, 1]), sampleEntry));
  return concat(box("ftyp", []), box("moov", box("trak", box("mdia", box("minf", box("stbl", sampleDescription))))));
}

describe("detectLiveVideoMime", () => {
  it("derives an exact AVC codec string from the video sample entry", () => {
    expect(detectLiveVideoMime(videoInitialization("avc1", "avcC", [1, 0x64, 0, 0x1f])))
      .toBe('video/mp4; codecs="avc1.64001f"');
  });

  it("derives an HEVC hvc1 codec string with reversed compatibility flags and constraints", () => {
    const hvcc = [1, 1, 0x60, 0, 0, 0, 0xb0, 0, 0, 0, 0, 0, 120];
    expect(detectLiveVideoMime(videoInitialization("hvc1", "hvcC", hvcc)))
      .toBe('video/mp4; codecs="hvc1.1.6.L120.B0"');
    expect(detectLiveVideoMime(videoInitialization("hev1", "hvcC", hvcc)))
      .toBe('video/mp4; codecs="hev1.1.6.L120.B0"');
  });

  it("rejects invalid configuration rather than treating arbitrary hvcC bytes as a playable codec", () => {
    const valid = [1, 1, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 120];
    expect(detectLiveVideoMime(concat(box("ftyp", []), box("moov", box("udta", box("hvcC", valid)))))).toBeNull();
    expect(detectLiveVideoMime(videoInitialization("hvc1", "avcC", valid))).toBeNull();
    expect(detectLiveVideoMime(videoInitialization("hvc1", "hvcC", [0, ...valid.slice(1)]))).toBeNull();
    expect(detectLiveVideoMime(videoInitialization("hvc1", "hvcC", valid.slice(0, 12)))).toBeNull();
  });
  it("detects decoder-configuration changes even when the AVC MIME stays identical", () => {
    const first = videoInitialization("avc1", "avcC", [1, 0x64, 0, 0x1f, 0xaa]);
    const changed = videoInitialization("avc1", "avcC", [1, 0x64, 0, 0x1f, 0xbb]);
    const initial = detectLiveVideoConfiguration(first);
    const newer = detectLiveVideoConfiguration(changed);
    expect(initial?.mime).toBe(newer?.mime);
    expect(initial?.signature).not.toBe(newer?.signature);
    expect(detectLiveVideoConfiguration(concat(first, box("free", [1, 2])))?.signature)
      .toBe(initial?.signature);
    expect(detectLiveVideoConfiguration(videoInitialization("hvc1", "hvcC", [1, 1, 0x60, 0, 0, 0, 0, 0, 0, 0, 0, 0, 120]))?.signature)
      .not.toBe(initial?.signature);
  });
});
