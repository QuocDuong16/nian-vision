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
  ];
  const componentLines = requiredComponentMacros(windowsContract.configureFlags).map((macro) => `#define ${macro} 1`);
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

test("valid Windows FFmpeg cached runtime passes the shared validator", (t) => {
  const root = createValidOutput(t);
  assert.equal(validateWindowsFfmpegOutput(root), true);
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

test("Windows FFmpeg cached runtime rejects disabled required component", (t) => {
  const root = createValidOutput(t);
  const configPath = join(root, "FFMPEG_CONFIG_COMPONENTS.h");
  const config = readFileSync(configPath, "utf8").replace("CONFIG_RTSP_PROTOCOL 1", "CONFIG_RTSP_PROTOCOL 0");
  writeFileSync(configPath, config, "utf8");
  assert.throws(() => validateWindowsFfmpegOutput(root), /required FFmpeg component/);
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
