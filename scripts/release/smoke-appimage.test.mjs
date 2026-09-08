import assert from "node:assert/strict";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const smoke = readNormalizedText(new URL("./smoke-appimage.sh", import.meta.url));
const stage = readNormalizedText(new URL("./stage-linux.sh", import.meta.url));
const preflight = readNormalizedText(new URL("./preflight-linux.sh", import.meta.url));
const runtimeContract = readNormalizedText(new URL("./linux-runtime-contract.sh", import.meta.url));

test("Linux release preflight explicitly requires patchelf", () => {
  const required = preflight.match(/required=\(([\s\S]*?)\n\)/)?.[1];
  assert.ok(required, "preflight required-tool list is missing");
  assert.match(required, /\bpatchelf\b/);
});

test("Linux staging patches private FFmpeg copies with exact $ORIGIN RUNPATH", () => {
  assert.ok(stage.includes('lib_stage="$stage/lib/nian-vision"'));
  assert.ok(stage.includes('[[ ! -f "$object" || -L "$object" ]]'));
  assert.ok(stage.includes("staged private FFmpeg runtime entry must be a regular non-symlink file"));
  assert.ok(stage.includes("patchelf --set-rpath '$ORIGIN' \"$object\""));
  assert.ok(stage.includes("require_exact_runpath \"$object\" '$ORIGIN'"));
  assert.doesNotMatch(stage, /LD_LIBRARY_PATH\s*=/);
});

test("AppImage smoke enforces exact installation-local FFmpeg layout and RUNPATH", () => {
  for (const name of ["libavformat.so.62", "libavcodec.so.62", "libavutil.so.60"]) {
    assert.ok(smoke.includes(`$private_lib_dir/${name}`));
    assert.ok(smoke.includes(`$appdir/usr/lib/$name`));
  }
  assert.ok(smoke.includes("expected_private_ffmpeg=(libavcodec.so.62 libavformat.so.62 libavutil.so.60)"));
  assert.ok(smoke.includes('[[ ! -f "$object" || -L "$object" ]]'));
  assert.ok(smoke.includes("AppImage private FFmpeg runtime entry must be a regular non-symlink file"));
  assert.ok(smoke.includes("require_exact_runpath \"$object\" '$ORIGIN'"));
  assert.ok(smoke.includes('require_private_ffmpeg_closure "$object" "$private_lib_dir"'));
  assert.ok(smoke.includes("require_exact_runpath \"$appdir/usr/bin/nian-media-worker\" '$ORIGIN/../lib/nian-vision'"));
  assert.ok(stage.includes("require_exact_runpath \"$worker_stage\" '$ORIGIN/../lib/nian-vision'"));
  assert.doesNotMatch(smoke, /LD_LIBRARY_PATH\s*=/);
});

test("AppImage smoke proves worker FFmpeg closure without build-time library environment", () => {
  assert.ok(runtimeContract.includes('env -u LD_LIBRARY_PATH -u NIAN_FFMPEG_LIB_DIR ldd "$object"'));
  assert.doesNotMatch(runtimeContract, /LD_LIBRARY_PATH\s*=/);
  assert.ok(stage.includes('require_private_ffmpeg_closure "$object" "$lib_stage"'));
  assert.match(smoke, /expected="\$private_lib_dir\/\$name"/);
  assert.match(smoke, /resolved=.*awk -v name=/);
  assert.match(runtimeContract, /private_real=.*readlink -f/);
  assert.ok(runtimeContract.includes('"$expected_real" != "$private_real/"*'));
  assert.match(runtimeContract, /resolved_real=.*readlink -f/);
  assert.match(runtimeContract, /expected_real=.*readlink -f/);
  assert.ok(
    smoke.includes(
      [
        "env -u LD_LIBRARY_PATH -u NIAN_FFMPEG_LIB_DIR \\",
        '  node "$repo_root/scripts/release/stage-runtime-smoke.mjs" \\',
      ].join("\n"),
    ),
  );
});
