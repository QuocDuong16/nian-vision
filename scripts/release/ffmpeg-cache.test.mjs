import assert from "node:assert/strict";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";

import {
  buildContractDigest,
  buildWindowsFfmpegContract,
  expectedWindowsFfmpegMetadata,
  loadWindowsFfmpegContractInputs,
  validateWindowsFfmpegMetadata,
} from "./ffmpeg-cache-key.mjs";
import {
  requiredComponentMacros,
  validateWindowsFfmpegOutput,
} from "./validate-ffmpeg-windows.mjs";

const inputs = loadWindowsFfmpegContractInputs();

function cloneInputs() {
  return structuredClone(inputs);
}

function createValidOutput(t) {
  const root = mkdtempSync(join(tmpdir(), "nian-ffmpeg-cache-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  mkdirSync(join(root, "bin"), { recursive: true });
  mkdirSync(join(root, "lib"), { recursive: true });
  mkdirSync(join(root, "include"), { recursive: true });

  const { releaseConfig, windowsContract } = inputs;
  const configLines = [
    "#define CONFIG_GPL 0",
    "#define CONFIG_NONFREE 0",
    "#define CONFIG_SHARED 1",
    "#define CONFIG_NETWORK 1",
    "#define CONFIG_CBS_APV_LAVF 1",
    "#define CONFIG_CBS_AV1_LAVF 1",
  ];
  const componentLines = [
    ...requiredComponentMacros(windowsContract.configureFlags).map((macro) => `#define ${macro} 1`),
    "#define CONFIG_HTTP_PROTOCOL 1",
    "#define CONFIG_ASF_DEMUXER 1",
    "#define CONFIG_RM_DEMUXER 1",
    "#define CONFIG_MPEGTS_DEMUXER 1",
  ];
  writeFileSync(join(root, "FFMPEG_CONFIG.h"), `${configLines.join("\n")}\n`, "utf8");
  writeFileSync(join(root, "FFMPEG_CONFIG_COMPONENTS.h"), `${componentLines.join("\n")}\n`, "utf8");
  writeFileSync(join(root, "FFMPEG_BUILD_FLAGS.txt"), windowsContract.configureFlags.join("\n"), "utf8");
  writeFileSync(join(root, "FFMPEG-LGPL-2.1.txt"), "LGPL fixture\n", "utf8");
  writeFileSync(
    join(root, "FFMPEG_BUILD_METADATA.json"),
    `${JSON.stringify(expectedWindowsFfmpegMetadata(releaseConfig, windowsContract), null, 2)}\n`,
    "utf8",
  );

  const contract = buildWindowsFfmpegContract(releaseConfig, windowsContract);
  for (const name of contract.required_outputs.dlls) writeFileSync(join(root, "bin", name), "dll fixture");
  for (const name of contract.required_outputs.import_libs) writeFileSync(join(root, "lib", name), "lib fixture");
  for (const name of contract.required_outputs.headers) {
    const path = join(root, "include", ...name.split("/"));
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, "header fixture");
  }
  return root;
}

test("Windows FFmpeg cache key is deterministic and independent of unrelated application state", () => {
  const first = buildContractDigest(inputs.releaseConfig, inputs.windowsContract);
  const second = buildContractDigest(inputs.releaseConfig, inputs.windowsContract);
  assert.match(first, /^[0-9a-f]{64}$/);
  assert.equal(second, first);

  const unrelated = cloneInputs();
  unrelated.releaseConfig.applicationVersion = "9.9.9-unrelated";
  unrelated.releaseConfig.applicationSourceCommit = "deadbeef";
  assert.equal(buildContractDigest(unrelated.releaseConfig, unrelated.windowsContract), first);
});

test("Windows FFmpeg cache key changes for every authoritative build-contract dimension", () => {
  const base = buildContractDigest(inputs.releaseConfig, inputs.windowsContract);
  const variants = [];

  const source = cloneInputs();
  source.releaseConfig.ffmpegSourceSha256 = "0".repeat(64);
  variants.push(source);

  const flags = cloneInputs();
  flags.windowsContract.configureFlags = [...flags.windowsContract.configureFlags, "--disable-debug"];
  variants.push(flags);

  const target = cloneInputs();
  target.releaseConfig.windowsTarget = "x86_64-pc-windows-gnu";
  variants.push(target);

  const toolchain = cloneInputs();
  toolchain.windowsContract.toolchain = "clang-cl";
  variants.push(toolchain);

  const msysMake = cloneInputs();
  msysMake.windowsContract.msysBuildTools.make = "/mingw64/bin/mingw32-make";
  variants.push(msysMake);

  const msysUniq = cloneInputs();
  msysUniq.windowsContract.msysBuildTools.uniq = "/mingw64/bin/uniq";
  variants.push(msysUniq);

  const makePackageVersion = cloneInputs();
  makePackageVersion.windowsContract.provisionedMsysPackages.make.version = "4.4.1-2";
  variants.push(makePackageVersion);

  const makePackageSha = cloneInputs();
  makePackageSha.windowsContract.provisionedMsysPackages.make.sha256 = "f".repeat(64);
  variants.push(makePackageSha);

  const makePackageUrl = cloneInputs();
  makePackageUrl.windowsContract.provisionedMsysPackages.make.url = "https://repo.msys2.org/msys/x86_64/make-old.pkg.tar.zst";
  variants.push(makePackageUrl);

  const makePackageExecutable = cloneInputs();
  makePackageExecutable.windowsContract.provisionedMsysPackages.make.msysExecutable = "/mingw64/bin/make";
  variants.push(makePackageExecutable);

  const diffutilsPackageVersion = cloneInputs();
  diffutilsPackageVersion.windowsContract.provisionedMsysPackages.diffutils.version = "3.11-1";
  variants.push(diffutilsPackageVersion);

  const diffutilsPackageSha = cloneInputs();
  diffutilsPackageSha.windowsContract.provisionedMsysPackages.diffutils.sha256 = "e".repeat(64);
  variants.push(diffutilsPackageSha);

  const diffutilsPackageUrl = cloneInputs();
  diffutilsPackageUrl.windowsContract.provisionedMsysPackages.diffutils.url = "https://mirror.msys2.org/msys/x86_64/diffutils-old.pkg.tar.zst";
  variants.push(diffutilsPackageUrl);

  const diffutilsExecutable = cloneInputs();
  diffutilsExecutable.windowsContract.provisionedMsysPackages.diffutils.msysExecutable = "/mingw64/bin/cmp";
  variants.push(diffutilsExecutable);

  const buildRevision = cloneInputs();
  buildRevision.windowsContract.buildContractVersion += 1;
  variants.push(buildRevision);

  const outputRevision = cloneInputs();
  outputRevision.windowsContract.outputRuntimeContractVersion += 1;
  variants.push(outputRevision);

  for (const variant of variants) {
    assert.notEqual(buildContractDigest(variant.releaseConfig, variant.windowsContract), base);
  }
});

test("Windows FFmpeg portable cache contract excludes machine-specific native tool paths", () => {
  const serialized = JSON.stringify(buildWindowsFfmpegContract(inputs.releaseConfig, inputs.windowsContract));
  for (const forbidden of ["Visual Studio", "Windows Kits", "dumpbin.exe", "rc.exe", "HostX64"]) {
    assert.equal(serialized.includes(forbidden), false, `portable FFmpeg cache contract leaked ${forbidden}`);
  }
});

test("RC10 contract without deterministic uniq cannot share the current cache key", () => {
  const withoutUniq = cloneInputs();
  delete withoutUniq.windowsContract.msysBuildTools.uniq;
  assert.notEqual(
    buildContractDigest(withoutUniq.releaseConfig, withoutUniq.windowsContract),
    buildContractDigest(inputs.releaseConfig, inputs.windowsContract),
  );
});

test("RC10 make-only package contract cannot share the make+diffutils cache key", () => {
  const makeOnly = cloneInputs();
  delete makeOnly.windowsContract.provisionedMsysPackages.diffutils;
  assert.notEqual(
    buildContractDigest(makeOnly.releaseConfig, makeOnly.windowsContract),
    buildContractDigest(inputs.releaseConfig, inputs.windowsContract),
  );
});

test("pre-RC10 Windows FFmpeg build contract cannot share the RC10 cache key", () => {
  const old = cloneInputs();
  old.windowsContract.buildContractVersion = 2;
  delete old.windowsContract.provisionedMsysPackages;
  assert.notEqual(buildContractDigest(old.releaseConfig, old.windowsContract), buildContractDigest(inputs.releaseConfig, inputs.windowsContract));
});

test("Windows FFmpeg metadata validates only for the current source and build contract", () => {
  const metadata = expectedWindowsFfmpegMetadata(inputs.releaseConfig, inputs.windowsContract);
  assert.equal(validateWindowsFfmpegMetadata(metadata, inputs.releaseConfig, inputs.windowsContract), true);

  const wrongSource = { ...metadata, source_sha256: "0".repeat(64) };
  assert.throws(
    () => validateWindowsFfmpegMetadata(wrongSource, inputs.releaseConfig, inputs.windowsContract),
    /does not match/,
  );

  const wrongDigest = { ...metadata, build_contract_sha256: "f".repeat(64) };
  assert.throws(
    () => validateWindowsFfmpegMetadata(wrongDigest, inputs.releaseConfig, inputs.windowsContract),
    /does not match/,
  );
});

test("Windows FFmpeg component ownership excludes global network and preserves required families", () => {
  const macros = new Set(requiredComponentMacros(inputs.windowsContract.configureFlags));
  assert.equal(macros.has("CONFIG_NETWORK"), false);
  assert.equal(macros.has("CONFIG_RTSP_PROTOCOL"), false);
  assert.equal(macros.has("CONFIG_RTSP_DEMUXER"), true);
  for (const macro of [
    "CONFIG_FILE_PROTOCOL",
    "CONFIG_TCP_PROTOCOL",
    "CONFIG_RTP_PROTOCOL",
    "CONFIG_UDP_PROTOCOL",
    "CONFIG_MATROSKA_DEMUXER",
    "CONFIG_MOV_DEMUXER",
    "CONFIG_RTSP_DEMUXER",
    "CONFIG_MATROSKA_MUXER",
    "CONFIG_MOV_MUXER",
    "CONFIG_MP4_MUXER",
    "CONFIG_H264_PARSER",
    "CONFIG_MPEG4VIDEO_PARSER",
    "CONFIG_MPEGAUDIO_PARSER",
    "CONFIG_AAC_PARSER",
    "CONFIG_MPEG4_DECODER",
    "CONFIG_AAC_DECODER",
  ]) {
    assert.equal(macros.has(macro), true, `missing required component macro ${macro}`);
  }
});

test("realistic Windows FFmpeg headers validate with CONFIG_NETWORK only in config.h", (t) => {
  const root = createValidOutput(t);
  const config = readFileSync(join(root, "FFMPEG_CONFIG.h"), "utf8");
  const components = readFileSync(join(root, "FFMPEG_CONFIG_COMPONENTS.h"), "utf8");
  assert.match(config, /^#define CONFIG_NETWORK 1$/m);
  assert.doesNotMatch(components, /CONFIG_NETWORK/);
  assert.doesNotMatch(components, /CONFIG_RTSP_PROTOCOL/);
  assert.match(components, /^#define CONFIG_RTSP_DEMUXER 1$/m);
  assert.match(components, /^#define CONFIG_HTTP_PROTOCOL 1$/m);
  assert.equal(validateWindowsFfmpegOutput(root), true);
});

test("Windows FFmpeg cached runtime rejects missing or disabled CONFIG_NETWORK in config.h", (t) => {
  for (const mutate of [
    (config) => config.replace("#define CONFIG_NETWORK 1\n", ""),
    (config) => config.replace("CONFIG_NETWORK 1", "CONFIG_NETWORK 0"),
  ]) {
    const root = createValidOutput(t);
    const configPath = join(root, "FFMPEG_CONFIG.h");
    writeFileSync(configPath, mutate(readFileSync(configPath, "utf8")), "utf8");
    assert.throws(() => validateWindowsFfmpegOutput(root), /CONFIG_NETWORK/);
  }
});

test("Windows FFmpeg cached runtime rejects wrong source metadata", (t) => {
  const root = createValidOutput(t);
  const metadataPath = join(root, "FFMPEG_BUILD_METADATA.json");
  const metadata = JSON.parse(readFileSync(metadataPath, "utf8"));
  metadata.source_sha256 = "0".repeat(64);
  writeFileSync(metadataPath, JSON.stringify(metadata));
  assert.throws(() => validateWindowsFfmpegOutput(root), /metadata does not match/);
});

test("Windows FFmpeg cached runtime rejects wrong build-contract digest", (t) => {
  const root = createValidOutput(t);
  const metadataPath = join(root, "FFMPEG_BUILD_METADATA.json");
  const metadata = JSON.parse(readFileSync(metadataPath, "utf8"));
  metadata.build_contract_sha256 = "f".repeat(64);
  writeFileSync(metadataPath, JSON.stringify(metadata));
  assert.throws(() => validateWindowsFfmpegOutput(root), /metadata does not match/);
});

test("Windows FFmpeg cached runtime rejects missing DLL", (t) => {
  const root = createValidOutput(t);
  unlinkSync(join(root, "bin", "avformat-62.dll"));
  assert.throws(() => validateWindowsFfmpegOutput(root), /runtime DLL/);
});

test("Windows FFmpeg cached runtime rejects missing import library", (t) => {
  const root = createValidOutput(t);
  unlinkSync(join(root, "lib", "avcodec.lib"));
  assert.throws(() => validateWindowsFfmpegOutput(root), /import library/);
});

test("Windows FFmpeg cached runtime rejects altered configure flags", (t) => {
  const root = createValidOutput(t);
  const flagsPath = join(root, "FFMPEG_BUILD_FLAGS.txt");
  writeFileSync(flagsPath, `${readFileSync(flagsPath, "utf8")}\n--enable-gpl`, "utf8");
  assert.throws(() => validateWindowsFfmpegOutput(root), /configure flags do not exactly match/);
});

test("Windows FFmpeg cached runtime still enforces every required component family", (t) => {
  for (const macro of [
    "CONFIG_TCP_PROTOCOL",
    "CONFIG_MATROSKA_DEMUXER",
    "CONFIG_MP4_MUXER",
    "CONFIG_H264_PARSER",
    "CONFIG_AAC_DECODER",
  ]) {
    const root = createValidOutput(t);
    const configPath = join(root, "FFMPEG_CONFIG_COMPONENTS.h");
    const config = readFileSync(configPath, "utf8").replace(`${macro} 1`, `${macro} 0`);
    writeFileSync(configPath, config, "utf8");
    assert.throws(() => validateWindowsFfmpegOutput(root), /requested FFmpeg component/, macro);
  }
});

test("Windows FFmpeg cached runtime rejects forbidden CLI programs", (t) => {
  const root = createValidOutput(t);
  writeFileSync(join(root, "bin", "ffmpeg.exe"), "forbidden");
  assert.throws(() => validateWindowsFfmpegOutput(root), /CLI program unexpectedly present/);
});


test("Windows FFmpeg cached runtime rejects missing mandatory license evidence", (t) => {
  const root = createValidOutput(t);
  unlinkSync(join(root, "FFMPEG-LGPL-2.1.txt"));
  assert.throws(() => validateWindowsFfmpegOutput(root));
});

test("Windows FFmpeg cached runtime rejects missing component configuration evidence", (t) => {
  const root = createValidOutput(t);
  unlinkSync(join(root, "FFMPEG_CONFIG_COMPONENTS.h"));
  assert.throws(() => validateWindowsFfmpegOutput(root));
});

test("Windows FFmpeg cached runtime rejects duplicated required DLL", (t) => {
  const root = createValidOutput(t);
  writeFileSync(join(root, "lib", "avutil-60.dll"), "duplicate dll fixture");
  assert.throws(() => validateWindowsFfmpegOutput(root), /runtime DLL missing, duplicated, or misplaced/);
});

test("RC12 pristine FFmpeg contract cannot share the current patched cache key", () => {
  const rc12 = cloneInputs();
  rc12.windowsContract.buildContractVersion = 3;
  rc12.windowsContract.configureFlags = rc12.windowsContract.configureFlags.map((flag) =>
    flag === "--enable-protocol=file,tcp,rtp,udp" ? "--enable-protocol=file,tcp,rtsp,rtp,udp" : flag,
  );
  delete rc12.releaseConfig.ffmpegUpstreamPatch;
  const rc12Digest = buildContractDigest(rc12.releaseConfig, rc12.windowsContract);
  const currentDigest = buildContractDigest(inputs.releaseConfig, inputs.windowsContract);
  assert.equal(rc12Digest, "0d15adba7b6429e6b48a49d70d1635a6ae5d659080d55e38c7bd9ae4bd4ec438");
  assert.notEqual(currentDigest, rc12Digest);
});

test("RC15 corrected protocol text cannot share the RC14 Windows FFmpeg cache digest", () => {
  const rc14 = cloneInputs();
  rc14.windowsContract.configureFlags = rc14.windowsContract.configureFlags.map((flag) =>
    flag === "--enable-protocol=file,tcp,rtp,udp" ? "--enable-protocol=file,tcp,rtsp,rtp,udp" : flag,
  );
  assert.equal(rc14.windowsContract.buildContractVersion, 4);
  assert.equal(inputs.windowsContract.buildContractVersion, 4);
  assert.notEqual(
    buildContractDigest(rc14.releaseConfig, rc14.windowsContract),
    buildContractDigest(inputs.releaseConfig, inputs.windowsContract),
  );
});

test("RC13 Windows FFmpeg cache digest changes with upstream patch commit", () => {
  const changed = cloneInputs();
  changed.releaseConfig.ffmpegUpstreamPatch.commit = "7".repeat(40);
  assert.notEqual(
    buildContractDigest(changed.releaseConfig, changed.windowsContract),
    buildContractDigest(inputs.releaseConfig, inputs.windowsContract),
  );
});

test("RC13 Windows FFmpeg build contract fails closed without patch metadata", () => {
  const missing = cloneInputs();
  delete missing.releaseConfig.ffmpegUpstreamPatch;
  assert.throws(
    () => buildContractDigest(missing.releaseConfig, missing.windowsContract),
    /requires complete upstream patch provenance/,
  );
});

test("RC13 Windows FFmpeg metadata records exact upstream patch identity", () => {
  const metadata = expectedWindowsFfmpegMetadata(inputs.releaseConfig, inputs.windowsContract);
  assert.equal(metadata.build_contract_version, 4);
  assert.equal(metadata.upstream_patch_contract_version, 1);
  assert.equal(metadata.upstream_patch_repository, "FFmpeg/FFmpeg");
  assert.equal(metadata.upstream_patch_commit, "6a59c847b50c6bc30630df7fca56ccd6cd8a5a8c");
  assert.equal(metadata.upstream_patch_subject, "configure: Redo enabling cbs in lavf");
  assert.deepEqual(metadata.upstream_patch_files, ["configure", "libavformat/Makefile", "libavformat/cbs.h"]);
});

test("RC13 cached runtime rejects missing wrong or pristine RC12 patch metadata", (t) => {
  for (const mutate of [
    (metadata) => { delete metadata.upstream_patch_commit; },
    (metadata) => { metadata.upstream_patch_commit = "7".repeat(40); },
    (metadata) => {
      metadata.build_contract_version = 3;
      delete metadata.upstream_patch_contract_version;
      delete metadata.upstream_patch_repository;
      delete metadata.upstream_patch_commit;
      delete metadata.upstream_patch_subject;
      delete metadata.upstream_patch_files;
    },
  ]) {
    const root = createValidOutput(t);
    const metadataPath = join(root, "FFMPEG_BUILD_METADATA.json");
    const metadata = JSON.parse(readFileSync(metadataPath, "utf8"));
    mutate(metadata);
    writeFileSync(metadataPath, JSON.stringify(metadata));
    assert.throws(() => validateWindowsFfmpegOutput(root), /metadata does not match/);
  }
});
