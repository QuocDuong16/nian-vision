import assert from "node:assert/strict";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const smoke = readNormalizedText(new URL("./smoke-appimage.sh", import.meta.url));
const stage = readNormalizedText(new URL("./stage-linux.sh", import.meta.url));

test("AppImage smoke enforces exact installation-local FFmpeg layout and RUNPATH", () => {
  for (const name of ["libavformat.so.62", "libavcodec.so.62", "libavutil.so.60"]) {
    assert.ok(smoke.includes(`$appdir/usr/lib/nian-vision/${name}`));
    assert.ok(smoke.includes(`$appdir/usr/lib/$name`));
  }
  assert.match(smoke, /worker_runpath=.*readelf/);
  assert.ok(smoke.includes(`[[ "$worker_runpath" != '$ORIGIN/../lib/nian-vision' ]]`));
  assert.doesNotMatch(smoke, /grep -Fq '\$ORIGIN\/\.\.\/lib'/);
  assert.match(stage, /worker_runpath=.*readelf/);
  assert.ok(stage.includes(`[[ "$worker_runpath" != '$ORIGIN/../lib/nian-vision' ]]`));
});

test("AppImage smoke proves worker FFmpeg closure without build-time library environment", () => {
  assert.match(smoke, /env -u LD_LIBRARY_PATH -u NIAN_FFMPEG_LIB_DIR ldd/);
  assert.match(smoke, /expected="\$appdir\/usr\/lib\/nian-vision\/\$name"/);
  assert.match(smoke, /resolved=.*awk -v name=/);
  assert.match(smoke, /env -u LD_LIBRARY_PATH -u NIAN_FFMPEG_LIB_DIR \\\n+  node .*stage-runtime-smoke\.mjs/);
});
