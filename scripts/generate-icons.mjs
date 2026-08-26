// Generates the Tauri window/app icons as PNG files without external
// dependencies. Output: apps/nian-desktop/icons/{icon,128x128@2x,128x128,32x32}.png
import { deflateSync } from "node:zlib";
import { writeFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const outDir = join(repoRoot, "apps", "nian-desktop", "icons");

const SIZE = 1024;
const RADIUS = 190;
const BG = [15, 23, 42]; // slate-900
const RING = [226, 232, 240]; // slate-200
const IRIS = [56, 189, 248]; // sky-400

function insideRoundedRect(x, y, size, radius) {
  const r = radius / size;
  const u = x / size;
  const v = y / size;
  const cx = Math.min(Math.max(u, r), 1 - r);
  const cy = Math.min(Math.max(v, r), 1 - r);
  if (cx === u && cy === v) return true;
  const dx = u - cx;
  const dy = v - cy;
  return dx * dx + dy * dy <= r * r;
}

function sample(fx, fy) {
  // fx, fy in [0,1)
  if (!insideRoundedRect(fx, fy, 1, RADIUS / SIZE)) {
    return [0, 0, 0, 0];
  }
  const dx = fx - 0.5;
  const dy = fy - 0.5;
  const dist = Math.sqrt(dx * dx + dy * dy);

  // Outer lens ring: 0.20..0.30 of half-size
  if (dist >= 0.2 && dist <= 0.3) return [...RING, 255];
  // Iris disc
  if (dist <= 0.13) return [...IRIS, 255];
  // Pupil
  if (dist <= 0.055) return [...BG, 255];
  return [...BG, 255];
}

function render(size) {
  const pixels = new Uint8Array(size * size * 4);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      // Average 2x2 subsamples to reduce aliasing on the ring edge.
      let r = 0,
        g = 0,
        b = 0,
        a = 0;
      for (const [ox, oy] of [
        [0.25, 0.25],
        [0.75, 0.25],
        [0.25, 0.75],
        [0.75, 0.75],
      ]) {
        const [pr, pg, pb, pa] = sample((x + ox) / size, (y + oy) / size);
        r += pr;
        g += pg;
        b += pb;
        a += pa;
      }
      const i = (y * size + x) * 4;
      pixels[i] = Math.round(r / 4);
      pixels[i + 1] = Math.round(g / 4);
      pixels[i + 2] = Math.round(b / 4);
      pixels[i + 3] = Math.round(a / 4);
    }
  }
  return pixels;
}

function boxDownscale(pixels, from, to) {
  const factor = from / to;
  const out = new Uint8Array(to * to * 4);
  for (let y = 0; y < to; y++) {
    for (let x = 0; x < to; x++) {
      let r = 0,
        g = 0,
        b = 0,
        a = 0,
        n = 0;
      const x0 = Math.floor(x * factor);
      const y0 = Math.floor(y * factor);
      const x1 = Math.floor((x + 1) * factor);
      const y1 = Math.floor((y + 1) * factor);
      for (let sy = y0; sy < y1; sy++) {
        for (let sx = x0; sx < x1; sx++) {
          const i = (sy * from + sx) * 4;
          r += pixels[i];
          g += pixels[i + 1];
          b += pixels[i + 2];
          a += pixels[i + 3];
          n++;
        }
      }
      const o = (y * to + x) * 4;
      out[o] = Math.round(r / n);
      out[o + 1] = Math.round(g / n);
      out[o + 2] = Math.round(b / n);
      out[o + 3] = Math.round(a / n);
    }
  }
  return out;
}

const CRC_TABLE = new Int32Array(256);
for (let n = 0; n < 256; n++) {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  CRC_TABLE[n] = c;
}

function crc32(buf) {
  let c = 0xffffffff;
  for (const byte of buf) c = CRC_TABLE[(c ^ byte) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}

function encodePng(pixels, size) {
  const signature = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0; // filter: none
    Buffer.from(pixels.buffer, y * size * 4, size * 4).copy(
      raw,
      y * (size * 4 + 1) + 1
    );
  }
  return Buffer.concat([
    signature,
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

mkdirSync(outDir, { recursive: true });
const master = render(SIZE);
for (const [name, size] of [
  ["icon.png", SIZE],
  ["128x128@2x.png", 256],
  ["128x128.png", 128],
  ["32x32.png", 32],
]) {
  const pixels = size === SIZE ? master : boxDownscale(master, SIZE, size);
  writeFileSync(join(outDir, name), encodePng(pixels, size));
  console.log(`wrote ${name} (${size}x${size})`);
}
