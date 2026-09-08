import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import test from "node:test";

const appDir = new URL("../../apps/nian-desktop/", import.meta.url);
const config = JSON.parse(readFileSync(new URL("tauri.conf.json", appDir), "utf8"));
const icoPath = fileURLToPath(new URL("icons/icon.ico", appDir));

test("desktop bundle declares the Windows ICO required by tauri-build", () => {
  assert.ok(config.bundle.icon.includes("icons/icon.ico"));
  const ico = readFileSync(icoPath);
  assert.ok(ico.length > 6, "Windows icon must not be empty");
  assert.equal(ico.readUInt16LE(0), 0, "ICO reserved field must be zero");
  assert.equal(ico.readUInt16LE(2), 1, "ICO image type must be icon");
  const count = ico.readUInt16LE(4);
  assert.ok(count >= 1, "ICO must contain at least one image");

  const sizes = new Set();
  for (let index = 0; index < count; index += 1) {
    const offset = 6 + index * 16;
    assert.ok(offset + 16 <= ico.length, "ICO directory entry exceeds file length");
    const width = ico[offset] === 0 ? 256 : ico[offset];
    const height = ico[offset + 1] === 0 ? 256 : ico[offset + 1];
    assert.equal(width, height, `ICO entry ${index} must be square`);
    sizes.add(width);
  }

  for (const required of [16, 32, 48, 128, 256]) {
    assert.ok(sizes.has(required), `ICO is missing required ${required}x${required} representation`);
  }
});
