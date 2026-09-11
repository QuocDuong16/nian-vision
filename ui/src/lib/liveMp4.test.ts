import { describe, expect, it } from "vitest";
import { splitLiveMp4ForMse } from "./liveMp4";

function box(type: string, payload: number[]): Uint8Array {
  const result = new Uint8Array(8 + payload.length);
  const view = new DataView(result.buffer);
  view.setUint32(0, result.byteLength, false);
  for (let index = 0; index < 4; index += 1) result[4 + index] = type.charCodeAt(index);
  result.set(payload, 8);
  return result;
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

describe("splitLiveMp4ForMse", () => {
  it("keeps initialization once and strips file indexes and footer from media bytes", () => {
    const completeFragment = concat(
      box("ftyp", [1]),
      box("moov", [2]),
      box("sidx", [3]),
      box("moof", [4]),
      box("mdat", [5]),
      box("moof", [6]),
      box("mdat", [7]),
      box("mfra", [8]),
    );

    const split = splitLiveMp4ForMse(completeFragment);
    expect(split).not.toBeNull();
    expect(types(split!.initialization)).toEqual(["ftyp", "moov"]);
    expect(types(split!.media)).toEqual(["moof", "mdat", "moof", "mdat"]);
  });

  it("rejects malformed or non-fragmented MP4 input", () => {
    expect(splitLiveMp4ForMse(new Uint8Array([0, 0, 0, 4]))).toBeNull();
    expect(splitLiveMp4ForMse(concat(box("ftyp", []), box("moov", []), box("mdat", [])))).toBeNull();
  });
});
