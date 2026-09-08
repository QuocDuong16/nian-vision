import assert from "node:assert/strict";
import { chmodSync, copyFileSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

const scriptPath = fileURLToPath(new URL("./normalize-appimage-ffmpeg.sh", import.meta.url));
const script = readFileSync(scriptPath, "utf8");
const sonames = ["libavformat.so.62", "libavcodec.so.62", "libavutil.so.60"];

function fixture() {
  const root = mkdtempSync(join(tmpdir(), "nian-appimage-normalize-"));
  const appdir = join(root, "AppDir");
  const privateDir = join(appdir, "usr/lib/nian-vision");
  const legacyDir = join(appdir, "usr/lib");
  mkdirSync(privateDir, { recursive: true });
  for (const [index, name] of sonames.entries()) {
    const contents = `ffmpeg-${index}\n`;
    writeFileSync(join(privateDir, name), contents);
    writeFileSync(join(legacyDir, name), contents);
  }
  return { root, appdir, privateDir, legacyDir };
}

function normalize(appdir) {
  return spawnSync(
    "bash",
    ["-c", 'source "$1"; normalize_legacy_ffmpeg_layout "$2"', "bash", scriptPath, appdir],
    { encoding: "utf8" },
  );
}

function normalizeWorkerRunpath(appdir) {
  return spawnSync(
    "bash",
    ["-c", 'source "$1"; normalize_worker_runpath "$2"', "bash", scriptPath, appdir],
    { encoding: "utf8" },
  );
}

test("AppImage normalizer removes only byte-identical legacy FFmpeg copies", () => {
  const { root, appdir, privateDir, legacyDir } = fixture();
  try {
    const result = normalize(appdir);
    assert.equal(result.status, 0, result.stderr);
    for (const name of sonames) {
      assert.equal(readFileSync(join(privateDir, name), "utf8").startsWith("ffmpeg-"), true);
      assert.throws(() => readFileSync(join(legacyDir, name)), /ENOENT/);
    }
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("AppImage normalizer refuses to delete a non-identical legacy FFmpeg entry", () => {
  const { root, appdir, legacyDir } = fixture();
  try {
    writeFileSync(join(legacyDir, "libavformat.so.62"), "unexpected-runtime\n");
    const result = normalize(appdir);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /Refusing to delete non-identical legacy FFmpeg runtime entry/);
    assert.equal(readFileSync(join(legacyDir, "libavformat.so.62"), "utf8"), "unexpected-runtime\n");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("AppImage normalizer fails closed when the private FFmpeg runtime is incomplete", () => {
  const { root, appdir, privateDir } = fixture();
  try {
    rmSync(join(privateDir, "libavcodec.so.62"));
    const result = normalize(appdir);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /private FFmpeg runtime entry is missing, non-regular, or symlinked/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("AppImage normalizer restores the private worker RUNPATH after linuxdeploy normalization", {
  skip: process.platform !== "linux" ? "ELF RUNPATH fixture requires Linux" : false,
}, () => {
  const { root, appdir } = fixture();
  try {
    const worker = join(appdir, "usr/bin/nian-media-worker");
    mkdirSync(join(appdir, "usr/bin"), { recursive: true });
    copyFileSync("/bin/true", worker);
    chmodSync(worker, 0o755);
    const setResult = spawnSync("patchelf", ["--set-rpath", "$ORIGIN/../lib", worker], { encoding: "utf8" });
    assert.equal(setResult.status, 0, setResult.stderr);

    const result = normalizeWorkerRunpath(appdir);
    assert.equal(result.status, 0, result.stderr);

    const printResult = spawnSync("patchelf", ["--print-rpath", worker], { encoding: "utf8" });
    assert.equal(printResult.status, 0, printResult.stderr);
    assert.equal(printResult.stdout.trim(), "$ORIGIN/../lib/nian-vision");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("AppImage normalizer refuses an unexpected worker RUNPATH", {
  skip: process.platform !== "linux" ? "ELF RUNPATH fixture requires Linux" : false,
}, () => {
  const { root, appdir } = fixture();
  try {
    const worker = join(appdir, "usr/bin/nian-media-worker");
    mkdirSync(join(appdir, "usr/bin"), { recursive: true });
    copyFileSync("/bin/true", worker);
    chmodSync(worker, 0o755);
    const setResult = spawnSync("patchelf", ["--set-rpath", "$ORIGIN/unexpected", worker], { encoding: "utf8" });
    assert.equal(setResult.status, 0, setResult.stderr);

    const result = normalizeWorkerRunpath(appdir);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /Refusing to rewrite unexpected AppImage worker RUNPATH/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("AppImage repack preserves the existing runtime and reuses Tauri's cached output plugin", () => {
  assert.match(script, /--appimage-offset/);
  assert.match(script, /dd if="\$appimage" of="\$normalize_work_dir\/runtime"/);
  assert.match(script, /linuxdeploy-plugin-appimage\*\.AppImage/);
  assert.match(script, /--runtime-file "\$normalize_work_dir\/runtime"/);
  assert.equal(/\bcurl\b|\bwget\b/.test(script), false);
});

test("AppImage repack keeps the no-legacy-layout contract explicit", () => {
  for (const name of sonames) assert.ok(script.includes(name));
  assert.match(script, /cmp --silent "\$private_real" "\$legacy_real"/);
  assert.match(script, /rm -f -- "\$legacy_object"/);
  assert.match(script, /Failed to remove legacy FFmpeg runtime entry/);
});


test("AppImage repack restores the private worker RUNPATH before smoke validation", () => {
  assert.match(script, /normalize_worker_runpath "\$appdir"/);
  assert.match(script, /patchelf --set-rpath "\$expected" "\$worker"/);
  assert.match(script, /require_exact_runpath "\$worker" "\$expected"/);
  assert.ok(script.includes("$ORIGIN/../lib/nian-vision"));
  assert.ok(script.includes("$ORIGIN/../lib"));
});
