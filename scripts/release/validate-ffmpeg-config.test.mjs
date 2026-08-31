import test from "node:test";
import assert from "node:assert/strict";

import { validateFfmpegConfiguration } from "./validate-ffmpeg-config.mjs";

const config = `#define CONFIG_GPL 0\n#define CONFIG_NONFREE 0\n#define CONFIG_SHARED 1\n`;
const flags = [
  "--enable-shared",
  "--disable-static",
  "--disable-gpl",
  "--disable-nonfree",
].join("\n");

test("LGPL shared configuration passes", () => {
  assert.equal(validateFfmpegConfiguration(config, flags), true);
});

test("explicit GPL enable flag fails", () => {
  assert.throws(
    () => validateFfmpegConfiguration(config, `${flags}\n--enable-gpl\n`),
    /forbidden/,
  );
});

test("configured GPL mode fails even without an enable flag", () => {
  assert.throws(
    () => validateFfmpegConfiguration(config.replace("CONFIG_GPL 0", "CONFIG_GPL 1"), flags),
    /CONFIG_GPL/,
  );
});

test("static-only drift fails", () => {
  assert.throws(
    () => validateFfmpegConfiguration(config, flags.replace("--enable-shared\n", "")),
    /required FFmpeg release flag/,
  );
});
