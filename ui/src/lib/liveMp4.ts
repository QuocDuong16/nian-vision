type Mp4Box = {
  type: string;
  start: number;
  end: number;
  headerSize: number;
};

export type LiveMseParts = {
  initialization: Uint8Array;
  media: Uint8Array;
  movieFragmentCount: number;
};

function parseBoxes(bytes: Uint8Array, start = 0, end = bytes.byteLength): Mp4Box[] | null {
  if (start < 0 || end < start || end > bytes.byteLength) return null;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const boxes: Mp4Box[] = [];
  let offset = start;

  while (offset < end) {
    if (end - offset < 8) return null;

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
      if (end - offset < 16) return null;
      const high = view.getUint32(offset + 8, false);
      const low = view.getUint32(offset + 12, false);
      if (high !== 0) return null;
      size = low;
      headerSize = 16;
    } else if (size32 === 0) {
      size = end - offset;
    }

    if (size < headerSize || offset + size > end) return null;
    boxes.push({ type, start: offset, end: offset + size, headerSize });
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

function rewriteMovieFragmentSequenceNumbers(
  media: Uint8Array,
  firstSequence: number,
): { media: Uint8Array; count: number } | null {
  if (!Number.isSafeInteger(firstSequence) || firstSequence < 1 || firstSequence > 0xffff_ffff) {
    return null;
  }

  const rewritten = Uint8Array.from(media);
  const topLevel = parseBoxes(rewritten);
  if (!topLevel) return null;
  const view = new DataView(rewritten.buffer, rewritten.byteOffset, rewritten.byteLength);
  let count = 0;

  for (const box of topLevel) {
    if (box.type !== "moof") continue;
    const children = parseBoxes(rewritten, box.start + box.headerSize, box.end);
    if (!children) return null;
    const mfhd = children.find((child) => child.type === "mfhd");
    if (!mfhd) return null;
    const sequenceOffset = mfhd.start + mfhd.headerSize + 4;
    if (sequenceOffset + 4 > mfhd.end) return null;
    const sequence = firstSequence + count;
    if (sequence > 0xffff_ffff) return null;
    view.setUint32(sequenceOffset, sequence, false);
    count += 1;
  }

  return count > 0 ? { media: rewritten, count } : null;
}

/**
 * Converts one independently muxed live MP4 file into the ISO-BMFF pieces
 * expected by a long-lived Media Source Extensions buffer.
 *
 * FFmpeg emits a complete file for every worker chunk, so every file repeats
 * `ftyp`/`moov`, indexing/footer boxes, and starts its `mfhd.sequence_number`
 * from the beginning. MSE needs one initialization segment followed by media
 * segments whose movie-fragment sequence numbers stay monotonic. Timestamps
 * are intentionally left zero-based here; the player places each chunk on the
 * continuous MSE timeline with `SourceBuffer.timestampOffset`.
 */
export function splitLiveMp4ForMse(bytes: Uint8Array, firstSequence = 1): LiveMseParts | null {
  const boxes = parseBoxes(bytes);
  if (!boxes) return null;

  const initializationBoxes = boxes.filter((box) => box.type === "ftyp" || box.type === "moov");
  const mediaBoxes = boxes.filter((box) =>
    box.type === "styp" ||
    box.type === "moof" ||
    box.type === "mdat" ||
    box.type === "emsg" ||
    box.type === "prft"
  );
  if (!initializationBoxes.some((box) => box.type === "ftyp")) return null;
  if (!initializationBoxes.some((box) => box.type === "moov")) return null;
  if (!mediaBoxes.some((box) => box.type === "moof") || !mediaBoxes.some((box) => box.type === "mdat")) {
    return null;
  }

  const rewritten = rewriteMovieFragmentSequenceNumbers(joinBoxes(bytes, mediaBoxes), firstSequence);
  if (!rewritten) return null;

  return {
    initialization: joinBoxes(bytes, initializationBoxes),
    media: rewritten.media,
    movieFragmentCount: rewritten.count,
  };
}
