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
  if (boxes.length === 0) return new Uint8Array();
  const contiguous = boxes.every((box, index) => index === 0 || boxes[index - 1]!.end === box.start);
  if (contiguous) return bytes.subarray(boxes[0]!.start, boxes[boxes.length - 1]!.end);

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

  const rewritten = media;
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

/** Read the codec from a VIDEO sample entry, not an arbitrary `avcC` byte pattern. */
export type LiveVideoConfiguration = { mime: string; signature: string };

/** Identify the decoder configuration independently of changing movie timing metadata. */
export function detectLiveVideoConfiguration(initialization: Uint8Array): LiveVideoConfiguration | null {
  const topLevel = parseBoxes(initialization);
  const moov = topLevel?.find((box) => box.type === "moov");
  if (!moov) return null;
  const movieBoxes = parseBoxes(initialization, moov.start + moov.headerSize, moov.end);
  if (!movieBoxes) return null;
  for (const track of movieBoxes.filter((box) => box.type === "trak")) {
    const tracks = parseBoxes(initialization, track.start + track.headerSize, track.end);
    const mdia = tracks?.find((box) => box.type === "mdia");
    if (!mdia) continue;
    const media = parseBoxes(initialization, mdia.start + mdia.headerSize, mdia.end);
    const minf = media?.find((box) => box.type === "minf");
    if (!minf) continue;
    const mediaInfo = parseBoxes(initialization, minf.start + minf.headerSize, minf.end);
    const stbl = mediaInfo?.find((box) => box.type === "stbl");
    if (!stbl) continue;
    const sampleTable = parseBoxes(initialization, stbl.start + stbl.headerSize, stbl.end);
    const stsd = sampleTable?.find((box) => box.type === "stsd");
    if (!stsd || stsd.end - stsd.start < stsd.headerSize + 8) continue;
    const data = new DataView(initialization.buffer, initialization.byteOffset, initialization.byteLength);
    const count = data.getUint32(stsd.start + stsd.headerSize + 4, false);
    if (count === 0 || count > 16) continue;
    const entries = parseBoxes(initialization, stsd.start + stsd.headerSize + 8, stsd.end);
    if (!entries || entries.length !== count) continue;
    for (const entry of entries) {
      if (!["avc1", "avc3", "hvc1", "hev1"].includes(entry.type) || entry.end - entry.start < entry.headerSize + 78) continue;
      // Compare the sample entry (including dimensions and SPS/PPS/VPS), not
      // the entire moov, whose timing metadata may legitimately differ.
      const sampleEntry = initialization.subarray(entry.start, entry.end);
      if (sampleEntry.byteLength > 16 * 1024) return null;
      const signature = Array.from(sampleEntry, (value) => value.toString(16).padStart(2, "0")).join("");
      const children = parseBoxes(initialization, entry.start + entry.headerSize + 78, entry.end);
      if (!children) continue;
      if (entry.type === "avc1" || entry.type === "avc3") {
        const box = children.find((child) => child.type === "avcC");
        if (!box || box.end - box.start < box.headerSize + 4) continue;
        const offset = box.start + box.headerSize;
        if (initialization[offset] !== 1) continue;
        const hex = [initialization[offset + 1], initialization[offset + 2], initialization[offset + 3]]
          .map((value) => value!.toString(16).padStart(2, "0")).join("");
        return { mime: `video/mp4; codecs="${entry.type}.${hex}"`, signature };
      }
      const box = children.find((child) => child.type === "hvcC");
      if (!box || box.end - box.start < box.headerSize + 13) continue;
      const offset = box.start + box.headerSize;
      if (initialization[offset] !== 1) continue;
      const profileByte = initialization[offset + 1]!;
      const profileIdc = profileByte & 31;
      const level = initialization[offset + 12]!;
      if (profileIdc === 0 || level === 0) continue;
      let compatibility = data.getUint32(offset + 2, false);
      let reversed = 0;
      for (let bit = 0; bit < 32; bit += 1) {
        reversed = (reversed << 1) | (compatibility & 1);
        compatibility >>>= 1;
      }
      const space = ["", "A", "B", "C"][profileByte >> 6]!;
      const tier = (profileByte & 0x20) !== 0 ? "H" : "L";
      const constraints = Array.from(initialization.subarray(offset + 6, offset + 12));
      while (constraints.length > 0 && constraints[constraints.length - 1] === 0) constraints.pop();
      const constraintSuffix = constraints.length
        ? `.${constraints.map((value) => value.toString(16).padStart(2, "0").toUpperCase()).join(".")}`
        : "";
      return { mime: `video/mp4; codecs="${entry.type}.${space}${profileIdc}.${(reversed >>> 0).toString(16).toUpperCase()}.${tier}${level}${constraintSuffix}"`, signature };
    }
  }
  return null;
}

export function detectLiveVideoMime(initialization: Uint8Array): string | null {
  return detectLiveVideoConfiguration(initialization)?.mime ?? null;
}

/**
 * Converts one independently muxed live MP4 file into the ISO-BMFF pieces
 * expected by a long-lived Media Source Extensions buffer.
 * Contiguous box ranges are returned as views over the response buffer and
 * movie-fragment sequence numbers are rewritten in place so the realtime path
 * does not allocate a second full media fragment just before MSE appends it.
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
