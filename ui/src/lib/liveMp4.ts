type Mp4Box = {
  type: string;
  start: number;
  end: number;
};

export type LiveMseParts = {
  initialization: Uint8Array;
  media: Uint8Array;
};

function parseTopLevelBoxes(bytes: Uint8Array): Mp4Box[] | null {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const boxes: Mp4Box[] = [];
  let offset = 0;

  while (offset < bytes.byteLength) {
    if (bytes.byteLength - offset < 8) return null;

    const size32 = view.getUint32(offset, false);
    const type = String.fromCharCode(
      bytes[offset + 4]!,
      bytes[offset + 5]!,
      bytes[offset + 6]!,
      bytes[offset + 7]!,
    );
    let headerSize = 8;
    let size = size32;

    if (size32 === 1) {
      if (bytes.byteLength - offset < 16) return null;
      const high = view.getUint32(offset + 8, false);
      const low = view.getUint32(offset + 12, false);
      if (high !== 0) return null;
      size = low;
      headerSize = 16;
    } else if (size32 === 0) {
      size = bytes.byteLength - offset;
    }

    if (size < headerSize || offset + size > bytes.byteLength) return null;
    boxes.push({ type, start: offset, end: offset + size });
    offset += size;
  }

  return boxes;
}

function joinBoxes(bytes: Uint8Array, boxes: Mp4Box[]): Uint8Array {
  const total = boxes.reduce((sum, box) => sum + box.end - box.start, 0);
  const joined = new Uint8Array(total);
  let offset = 0;
  for (const box of boxes) {
    const slice = bytes.subarray(box.start, box.end);
    joined.set(slice, offset);
    offset += slice.byteLength;
  }
  return joined;
}

/**
 * Converts one independently playable fragmented MP4 file into the ISO-BMFF
 * byte-stream pieces expected by Media Source Extensions. The worker emits a
 * complete file per live fragment (`ftyp/moov/.../moof/mdat/.../mfra`). MSE
 * needs the initialization segment once and then only media segments; feeding
 * a fresh `moov`/index/footer on every append eventually destabilizes WebView2.
 */
export function splitLiveMp4ForMse(bytes: Uint8Array): LiveMseParts | null {
  const boxes = parseTopLevelBoxes(bytes);
  if (!boxes) return null;

  const initializationBoxes = boxes.filter((box) => box.type === "ftyp" || box.type === "moov");
  const mediaBoxes = boxes.filter((box) =>
    box.type === "styp" ||
    box.type === "moof" ||
    box.type === "mdat" ||
    box.type === "emsg" ||
    box.type === "prft"
  );
  if (!initializationBoxes.some((box) => box.type === "moov")) return null;
  if (!mediaBoxes.some((box) => box.type === "moof") || !mediaBoxes.some((box) => box.type === "mdat")) {
    return null;
  }

  return {
    initialization: joinBoxes(bytes, initializationBoxes),
    media: joinBoxes(bytes, mediaBoxes),
  };
}
