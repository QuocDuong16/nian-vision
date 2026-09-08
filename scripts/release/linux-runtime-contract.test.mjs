import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const runtimeContractPath = fileURLToPath(new URL("./linux-runtime-contract.sh", import.meta.url));
const requiredTools = ["bash", "gcc", "patchelf", "readelf", "ldd"];
const linuxElfToolsAvailable =
  process.platform === "linux" &&
  requiredTools.every((command) => !spawnSync(command, ["--version"], { stdio: "ignore" }).error);

test(
  "Linux ELF runtime contract resolves siblings cleanly and rejects non-exact/private paths",
  { skip: !linuxElfToolsAvailable },
  () => {
    const root = mkdtempSync(join(tmpdir(), "nian-linux-runtime-contract-"));
    try {
      const utilSource = join(root, "avutil.c");
      const codecSource = join(root, "avcodec.c");
      const util = join(root, "libavutil.so.60");
      const codec = join(root, "libavcodec.so.62");
      const wrongDir = join(root, "host-ffmpeg");

      writeFileSync(utilSource, "int nian_avutil(void) { return 60; }\n");
      writeFileSync(
        codecSource,
        "extern int nian_avutil(void); int nian_avcodec(void) { return nian_avutil(); }\n",
      );
      execFileSync("gcc", ["-shared", "-fPIC", "-Wl,-soname,libavutil.so.60", "-o", util, utilSource]);
      execFileSync("gcc", [
        "-shared",
        "-fPIC",
        "-Wl,-soname,libavcodec.so.62",
        "-o",
        codec,
        codecSource,
        `-L${root}`,
        "-Wl,-l:libavutil.so.60",
      ]);
      execFileSync("patchelf", ["--set-rpath", "$ORIGIN", util]);
      execFileSync("patchelf", ["--set-rpath", "$ORIGIN", codec]);

      mkdirSync(wrongDir);
      cpSync(util, join(wrongDir, "libavutil.so.60"));

      const hostileEnv = {
        ...process.env,
        LD_LIBRARY_PATH: wrongDir,
        NIAN_FFMPEG_LIB_DIR: wrongDir,
      };
      execFileSync(
        "bash",
        [
          "-c",
          'source "$1"; require_exact_runpath "$2" "$3"; require_private_ffmpeg_closure "$2" "$4"',
          "_",
          runtimeContractPath,
          codec,
          "$ORIGIN",
          root,
        ],
        { env: hostileEnv, stdio: "pipe" },
      );

      assert.throws(() =>
        execFileSync(
          "bash",
          ["-c", 'source "$1"; require_private_ffmpeg_closure "$2" "$3"', "_", runtimeContractPath, codec, wrongDir],
          { stdio: "pipe" },
        ),
      );

      execFileSync("patchelf", ["--set-rpath", "$ORIGIN:/usr/lib", codec]);
      assert.throws(() =>
        execFileSync(
          "bash",
          ["-c", 'source "$1"; require_exact_runpath "$2" "$3"', "_", runtimeContractPath, codec, "$ORIGIN"],
          { stdio: "pipe" },
        ),
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  },
);
