import { describe, expect, it } from "vitest";
import { splitLiveMp4ForMse } from "./liveMp4";

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
  });

  it("rejects malformed, non-fragmented, or structurally incomplete MP4 input", () => {
    expect(splitLiveMp4ForMse(new Uint8Array([0, 0, 0, 4]))).toBeNull();
    expect(splitLiveMp4ForMse(concat(box("ftyp", []), box("moov", []), box("mdat", [])))).toBeNull();
    expect(
      splitLiveMp4ForMse(concat(box("ftyp", []), box("moov", []), box("moof", []), box("mdat", []))),
    ).toBeNull();
  });
});
