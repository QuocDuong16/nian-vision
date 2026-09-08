import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  applyFfmpegUpstreamPatches,
  expectedFfmpegUpstreamPatch,
  ffmpegUpstreamTransformations,
  verifyFfmpegUpstreamPatchSource,
} from "./apply-ffmpeg-upstream-patches.mjs";
import { readNormalizedText } from "./test-text.mjs";

const pristineConfigure = `CONFIG_EXTRA="
    cabac
    cbs
    cbs_apv
    cbs_av1
    cbs_h264
    cbs_h265
"

mov_demuxer_select="iso_media riffdec"
mov_muxer_select="iso_media iso_writer riffenc rtpenc_chain vp9_superframe_bsf aac_adtstoasc_bsf ac3_parser"
mov_muxer_suggest="iamfenc"
`;

const pristineMakefile = `OBJS-$(HAVE_LIBC_MSVCRT)                 += file_open.o

# subsystems
OBJS-$(CONFIG_ISO_MEDIA)                 += isom.o
OBJS-$(CONFIG_ISO_WRITER)                += avc.o hevc.o vvc.o

OBJS-$(CONFIG_MOV_DEMUXER)               += mov.o mov_chan.o mov_esds.o \\
                                            qtpalette.o replaygain.o dovi_isom.o \\
                                            dvdclut.o
OBJS-$(CONFIG_MOV_MUXER)                 += movenc.o \\
                                            movenchint.o mov_chan.o rtp.o \\
                                            movenccenc.o movenc_ttml.o rawutils.o \\
                                            apv.o dovi_isom.o evc.o cbs.o cbs_av1.o cbs_apv.o
OBJS-$(CONFIG_MP2_MUXER)                 += rawenc.o
`;

const pristineCbsHeader = `#ifndef AVFORMAT_CBS_H
#define AVFORMAT_CBS_H

#define CBS_PREFIX lavf_cbs
#define CBS_WRITE 0
#define CBS_TRACE 0
#define CBS_H264 0
#define CBS_H265 0
#define CBS_H266 0
#define CBS_JPEG 0
#define CBS_MPEG2 0
#define CBS_VP8 0
#define CBS_VP9 0

#include "libavcodec/cbs.h"

#endif /* AVFORMAT_CBS_H */
`;

function countOccurrences(text, needle) {
  return text.split(needle).length - 1;
}

function createFixture(t) {
  const root = mkdtempSync(join(tmpdir(), "nian-ffmpeg-upstream-patch-"));
  t?.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(join(root, "libavformat"), { recursive: true });
  writeFileSync(join(root, "configure"), pristineConfigure, "utf8");
  writeFileSync(join(root, "libavformat", "Makefile"), pristineMakefile, "utf8");
  writeFileSync(join(root, "libavformat", "cbs.h"), pristineCbsHeader, "utf8");
  writeFileSync(join(root, "unrelated.txt"), "must remain byte-for-byte unchanged\n", "utf8");
  return root;
}

function readFixture(root) {
  return new Map([
    ["configure", readFileSync(join(root, "configure"), "utf8")],
    ["libavformat/Makefile", readFileSync(join(root, "libavformat", "Makefile"), "utf8")],
    ["libavformat/cbs.h", readFileSync(join(root, "libavformat", "cbs.h"), "utf8")],
  ]);
}

function readAllFixtureFiles(root) {
  return new Map([
    ["configure", readFileSync(join(root, "configure"))],
    ["libavformat/Makefile", readFileSync(join(root, "libavformat", "Makefile"))],
    ["libavformat/cbs.h", readFileSync(join(root, "libavformat", "cbs.h"))],
    ["unrelated.txt", readFileSync(join(root, "unrelated.txt"))],
  ]);
}

test("RC13 patch provenance is the exact FFmpeg upstream commit and three-file allowlist", () => {
  assert.deepEqual(expectedFfmpegUpstreamPatch, {
    contractVersion: 1,
    repository: "FFmpeg/FFmpeg",
    commit: "6a59c847b50c6bc30630df7fca56ccd6cd8a5a8c",
    subject: "configure: Redo enabling cbs in lavf",
    files: ["configure", "libavformat/Makefile", "libavformat/cbs.h"],
  });
  const releaseConfig = JSON.parse(readFileSync(new URL("./release-config.json", import.meta.url), "utf8"));
  assert.deepEqual(releaseConfig.ffmpegUpstreamPatch, expectedFfmpegUpstreamPatch);
});

test("exact n8.0.3 fixture contains every patch preimage exactly once", (t) => {
  const files = readFixture(createFixture(t));
  for (const transformation of ffmpegUpstreamTransformations) {
    assert.equal(countOccurrences(files.get(transformation.file), transformation.preimage), 1, transformation.label);
  }
});

test("shared helper applies exact upstream semantics and only the three allowed files", (t) => {
  const root = createFixture(t);
  const before = readAllFixtureFiles(root);
  assert.deepEqual(applyFfmpegUpstreamPatches(root), expectedFfmpegUpstreamPatch.files);
  assert.equal(verifyFfmpegUpstreamPatchSource(root), true);
  const after = readAllFixtureFiles(root);
  const changedFiles = [...after.keys()].filter(
    (file) => !after.get(file).equals(before.get(file)),
  );
  assert.deepEqual(changedFiles, expectedFfmpegUpstreamPatch.files);

  const files = readFixture(root);
  for (const transformation of ffmpegUpstreamTransformations) {
    assert.equal(countOccurrences(files.get(transformation.file), transformation.preimage), 0, transformation.label);
    assert.equal(countOccurrences(files.get(transformation.file), transformation.postimage), 1, transformation.label);
  }
  const cbs = files.get("libavformat/cbs.h");
  assert.match(cbs, /#define CBS_APV CONFIG_CBS_APV_LAVF/);
  assert.match(cbs, /#define CBS_AV1 CONFIG_CBS_AV1_LAVF/);
  for (const codec of ["H264", "H265", "H266", "JPEG", "MPEG2", "VP8", "VP9"]) {
    assert.match(cbs, new RegExp(`#define CBS_${codec} 0`));
  }
});

test("second application fails closed instead of silently succeeding", (t) => {
  const root = createFixture(t);
  applyFfmpegUpstreamPatches(root);
  assert.throws(() => applyFfmpegUpstreamPatches(root), /preimage must occur exactly once/);
});

test("missing preimage fails before any source file is written", (t) => {
  const root = createFixture(t);
  const configurePath = join(root, "configure");
  writeFileSync(configurePath, pristineConfigure.replace("    cbs_apv\n    cbs_av1\n", "    cbs_apv\n"), "utf8");
  const before = readFixture(root);
  assert.throws(() => applyFfmpegUpstreamPatches(root), /preimage must occur exactly once/);
  assert.deepEqual(readFixture(root), before);
});

test("duplicate preimage fails before any source file is written", (t) => {
  const root = createFixture(t);
  const configurePath = join(root, "configure");
  writeFileSync(configurePath, `${pristineConfigure}\n${pristineConfigure}\n`, "utf8");
  const before = readFixture(root);
  assert.throws(() => applyFfmpegUpstreamPatches(root), /preimage=2/);
  assert.deepEqual(readFixture(root), before);
});

test("partially prepatched fixture fails closed", (t) => {
  const root = createFixture(t);
  const transformation = ffmpegUpstreamTransformations[0];
  const configurePath = join(root, "configure");
  writeFileSync(
    configurePath,
    pristineConfigure.replace(transformation.preimage, transformation.postimage),
    "utf8",
  );
  assert.throws(() => applyFfmpegUpstreamPatches(root), /postimage=1/);
});

test("release helper contains no runtime network patch fetch", () => {
  const helper = readNormalizedText(new URL("./apply-ffmpeg-upstream-patches.mjs", import.meta.url));
  for (const forbidden of ["fetch(", "https://", "http://", "curl ", "wget ", "git fetch", "github.com/"]) {
    assert.equal(helper.includes(forbidden), false, `patch helper must not contain runtime network fetch token: ${forbidden}`);
  }
});

test("Windows and Linux use the corrected RC15 RTSP component contract", () => {
  const windowsContract = JSON.parse(
    readFileSync(new URL("./ffmpeg-windows-contract.json", import.meta.url), "utf8"),
  );
  const linuxRelease = readNormalizedText(new URL("./build-ffmpeg-linux.sh", import.meta.url));
  const linuxCi = readNormalizedText(new URL("../ci-install-ffmpeg.sh", import.meta.url));

  assert.ok(windowsContract.configureFlags.includes("--enable-protocol=file,tcp,rtp,udp"));
  assert.ok(windowsContract.configureFlags.includes("--enable-demuxer=matroska,mov,rtsp"));
  assert.equal(
    windowsContract.configureFlags.some((flag) => flag === "--enable-protocol=file,tcp,rtsp,rtp,udp"),
    false,
  );
  for (const linux of [linuxRelease, linuxCi]) {
    assert.match(linux, /--enable-protocol=file,tcp,rtp,udp/);
    assert.match(linux, /--enable-demuxer=matroska,mov,rtsp/);
    assert.doesNotMatch(linux, /--enable-protocol=file,tcp,rtsp,rtp,udp/);
  }
});

test("Windows and Linux invoke the same patch helper after verified extraction and validate CBS before compile", () => {
  const windows = readNormalizedText(new URL("./build-ffmpeg-windows.ps1", import.meta.url));
  const linux = readNormalizedText(new URL("./build-ffmpeg-linux.sh", import.meta.url));

  const windowsSha = windows.indexOf('Invoke-FfmpegPhase "SHA-256 verification"');
  const windowsExtract = windows.indexOf('Invoke-FfmpegPhase "extraction"');
  const windowsPatch = windows.indexOf('Invoke-FfmpegPhase "apply upstream CBS lavf backport"');
  const windowsConfigure = windows.indexOf('Invoke-FfmpegPhase "configure"');
  const windowsGlobalValidate = windows.indexOf('Invoke-FfmpegPhase "post-configure global validation"');
  const windowsComponentValidate = windows.indexOf('Invoke-FfmpegPhase "post-configure component validation"');
  const windowsCbsValidate = windows.indexOf('Invoke-FfmpegPhase "post-configure CBS lavf validation"');
  const windowsCompile = windows.indexOf('Invoke-FfmpegPhase "compile"');
  assert.ok(
    windowsSha < windowsExtract &&
      windowsExtract < windowsPatch &&
      windowsPatch < windowsConfigure &&
      windowsConfigure < windowsGlobalValidate &&
      windowsGlobalValidate < windowsComponentValidate &&
      windowsComponentValidate < windowsCbsValidate &&
      windowsCbsValidate < windowsCompile,
  );
  assert.match(windows, /apply-ffmpeg-upstream-patches\.mjs/);

  const linuxSha = linux.indexOf("sha256sum --check --strict");
  const linuxExtract = linux.indexOf('tar -xJf "$tarball"');
  const linuxPatch = linux.indexOf("apply-ffmpeg-upstream-patches.mjs");
  const linuxConfigure = linux.indexOf('./configure "${configure_flags[@]}"');
  const linuxGlobalValidate = linux.indexOf("--scope global");
  const linuxComponentValidate = linux.indexOf("validate-ffmpeg-components.mjs");
  const linuxCbsValidate = linux.indexOf("--scope cbs");
  const linuxCompile = linux.indexOf('make -j"$(nproc)"');
  assert.ok(
    linuxSha < linuxExtract &&
      linuxExtract < linuxPatch &&
      linuxPatch < linuxConfigure &&
      linuxConfigure < linuxGlobalValidate &&
      linuxGlobalValidate < linuxComponentValidate &&
      linuxComponentValidate < linuxCbsValidate &&
      linuxCbsValidate < linuxCompile,
  );
});
