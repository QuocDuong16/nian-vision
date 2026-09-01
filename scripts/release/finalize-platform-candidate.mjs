import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { copyFileSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { collectVersions, validateVersions } from "./version-check.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const config = JSON.parse(readFileSync(resolve(root, "scripts/release/release-config.json"), "utf8"));

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function oneFile(dir, suffix) {
  const matches = readdirSync(dir).filter((name) => name.endsWith(suffix));
  if (matches.length !== 1) throw new Error(`expected exactly one ${suffix} in ${dir}, found ${matches.length}`);
  return resolve(dir, matches[0]);
}

function copy(source, output, name) {
  const destination = resolve(output, name);
  copyFileSync(source, destination);
  return destination;
}

function ffmpegHashes(dir, names) {
  return Object.fromEntries(names.map((name) => {
    const path = resolve(dir, name);
    return [name, { sha256: sha256(path), bytes: statSync(path).size }];
  }));
}

export function finalizeLinuxCandidate() {
  const version = validateVersions(collectVersions());
  const bundle = resolve(root, "target/release/bundle/appimage");
  const stage = resolve(root, "dist/linux-x86_64");
  const output = resolve(root, "dist/candidate-linux");
  const source = oneFile(bundle, ".AppImage");
  const sourceSig = `${source}.sig`;
  const artifactName = `Nian-Vision_${version}_linux-x86_64.AppImage`;
  rmSync(output, { recursive: true, force: true });
  mkdirSync(output, { recursive: true });
  const artifact = copy(source, output, artifactName);
  const signature = copy(sourceSig, output, `${artifactName}.sig`);
  copy(resolve(root, "THIRD_PARTY_NOTICES.txt"), output, "THIRD_PARTY_NOTICES.txt");
  copy(resolve(root, "RELEASE_NOTES.md"), output, "RELEASE_NOTES.md");
  copy(resolve(stage, "FFMPEG-LGPL-2.1.txt"), output, "FFMPEG-LGPL-2.1.txt");
  copy(resolve(stage, "FFMPEG_BUILD_FLAGS.txt"), output, "FFMPEG_BUILD_FLAGS_linux-x86_64.txt");
  copy(resolve(stage, "FFMPEG_CONFIG.h"), output, "FFMPEG_CONFIG_linux-x86_64.h");
  copy(resolve(stage, "BUILD_METADATA.json"), output, "BUILD_METADATA_linux-x86_64.json");
  const worker = resolve(stage, "bin/nian-media-worker");
  const manifest = {
    version,
    commit: execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim(),
    platform: config.platform,
    target: config.target,
    artifact: { filename: artifactName, sha256: sha256(artifact), bytes: statSync(artifact).size },
    worker: { sha256: sha256(worker), bytes: statSync(worker).size },
    updater: {
      signature_file: `${artifactName}.sig`,
      signature_sha256: sha256(signature),
      signature: readFileSync(signature, "utf8").trim(),
    },
    ffmpeg: {
      version: config.ffmpegVersion,
      source_sha256: config.ffmpegSourceSha256,
      libraries: ffmpegHashes(resolve(stage, "lib/nian-vision"), ["libavformat.so.62", "libavcodec.so.62", "libavutil.so.60"]),
    },
  };
  writeFileSync(resolve(output, "platform-manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  return output;
}

export function finalizeWindowsCandidate({ authenticodeSigned = false } = {}) {
  const version = validateVersions(collectVersions());
  const bundle = resolve(root, `target/${config.windowsTarget}/release/bundle/nsis`);
  const stage = resolve(root, "dist/windows-x86_64");
  const output = resolve(root, "dist/candidate-windows");
  const source = oneFile(bundle, "-setup.exe");
  const sourceSig = `${source}.sig`;
  const artifactName = `Nian-Vision_${version}_windows-x86_64-setup.exe`;
  rmSync(output, { recursive: true, force: true });
  mkdirSync(output, { recursive: true });
  const artifact = copy(source, output, artifactName);
  const signature = copy(sourceSig, output, `${artifactName}.sig`);
  copy(resolve(stage, "FFMPEG_BUILD_FLAGS.txt"), output, "FFMPEG_BUILD_FLAGS_windows-x86_64.txt");
  copy(resolve(stage, "FFMPEG_CONFIG.h"), output, "FFMPEG_CONFIG_windows-x86_64.h");
  copy(resolve(stage, "BUILD_METADATA.json"), output, "BUILD_METADATA_windows-x86_64.json");
  const runtime = resolve(stage, "runtime");
  const worker = resolve(runtime, "nian-media-worker.exe");
  const manifest = {
    version,
    commit: execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim(),
    platform: config.windowsPlatform,
    target: config.windowsTarget,
    artifact: { filename: artifactName, sha256: sha256(artifact), bytes: statSync(artifact).size },
    worker: { sha256: sha256(worker), bytes: statSync(worker).size },
    updater: {
      signature_file: `${artifactName}.sig`,
      signature_sha256: sha256(signature),
      signature: readFileSync(signature, "utf8").trim(),
    },
    ffmpeg: {
      version: config.ffmpegVersion,
      source_sha256: config.ffmpegSourceSha256,
      libraries: ffmpegHashes(runtime, ["avformat-62.dll", "avcodec-62.dll", "avutil-60.dll"]),
    },
    authenticode_signed: Boolean(authenticodeSigned),
  };
  writeFileSync(resolve(output, "platform-manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  return output;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const platform = process.argv[2];
    const output = platform === "linux"
      ? finalizeLinuxCandidate()
      : platform === "windows"
        ? finalizeWindowsCandidate({ authenticodeSigned: process.env.NIAN_WINDOWS_AUTHENTICODE_SIGNED === "true" })
        : (() => { throw new Error("usage: finalize-platform-candidate.mjs <linux|windows>"); })();
    process.stdout.write(`${platform} release candidate finalized at ${output}\n`);
  } catch (error) {
    console.error(`platform candidate finalization failed: ${error.message}`);
    process.exitCode = 1;
  }
}
