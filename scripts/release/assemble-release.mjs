import { createHash } from "node:crypto";
import { copyFileSync, mkdirSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { validateHttpsAuthority } from "./release-authority.mjs";
import { collectVersions, validateVersions } from "./version-check.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const config = JSON.parse(readFileSync(resolve(root, "scripts/release/release-config.json"), "utf8"));

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function loadCandidate(dir, expectedPlatform) {
  const manifest = JSON.parse(readFileSync(resolve(dir, "platform-manifest.json"), "utf8"));
  if (manifest.platform !== expectedPlatform) throw new Error(`candidate platform mismatch: expected ${expectedPlatform}`);
  const artifact = resolve(dir, manifest.artifact.filename);
  const signature = resolve(dir, manifest.updater.signature_file);
  if (sha256(artifact) !== manifest.artifact.sha256) throw new Error(`${expectedPlatform} artifact hash mismatch`);
  if (sha256(signature) !== manifest.updater.signature_sha256) throw new Error(`${expectedPlatform} signature hash mismatch`);
  if (readFileSync(signature, "utf8").trim() !== manifest.updater.signature) throw new Error(`${expectedPlatform} signature content mismatch`);
  return { dir, manifest, artifact, signature };
}

export function assembleRelease({ linuxDir, windowsDir, outputDir, releaseBaseUrl }) {
  const version = validateVersions(collectVersions());
  const base = validateHttpsAuthority(releaseBaseUrl, "release asset base URL").toString().replace(/\/$/, "");
  const linux = loadCandidate(resolve(linuxDir), config.platform);
  const windows = loadCandidate(resolve(windowsDir), config.windowsPlatform);
  if (linux.manifest.version !== version || windows.manifest.version !== version) throw new Error("platform candidate version drift");
  if (linux.manifest.commit !== windows.manifest.commit) throw new Error("platform candidates were built from different commits");
  if (linux.manifest.ffmpeg.version !== windows.manifest.ffmpeg.version || linux.manifest.ffmpeg.source_sha256 !== windows.manifest.ffmpeg.source_sha256) {
    throw new Error("Linux and Windows candidates do not share the same FFmpeg source authority");
  }

  const output = resolve(outputDir);
  rmSync(output, { recursive: true, force: true });
  mkdirSync(output, { recursive: true });
  const copied = new Set();
  for (const candidate of [linux, windows]) {
    for (const name of readdirSync(candidate.dir)) {
      if (name === "platform-manifest.json") continue;
      if (copied.has(name)) {
        const existing = resolve(output, name);
        if (sha256(existing) !== sha256(resolve(candidate.dir, name))) throw new Error(`shared candidate asset differs across platforms: ${name}`);
        continue;
      }
      copyFileSync(resolve(candidate.dir, name), resolve(output, name));
      copied.add(name);
    }
  }

  const notes = readFileSync(resolve(output, "RELEASE_NOTES.md"), "utf8").trim();
  const pubDate = new Date().toISOString();
  const latest = {
    version,
    notes,
    pub_date: pubDate,
    platforms: {
      [config.platform]: {
        signature: linux.manifest.updater.signature,
        url: `${base}/${linux.manifest.artifact.filename}`,
      },
      [config.windowsPlatform]: {
        signature: windows.manifest.updater.signature,
        url: `${base}/${windows.manifest.artifact.filename}`,
      },
    },
  };
  writeFileSync(resolve(output, "latest.json"), `${JSON.stringify(latest, null, 2)}\n`);

  const releaseManifest = {
    version,
    commit: linux.manifest.commit,
    ffmpeg: {
      version: linux.manifest.ffmpeg.version,
      source_sha256: linux.manifest.ffmpeg.source_sha256,
    },
    platforms: {
      [config.platform]: linux.manifest,
      [config.windowsPlatform]: windows.manifest,
    },
  };
  writeFileSync(resolve(output, "release-manifest.json"), `${JSON.stringify(releaseManifest, null, 2)}\n`);

  const names = readdirSync(output).filter((name) => name !== "SHA256SUMS.txt").sort();
  writeFileSync(
    resolve(output, "SHA256SUMS.txt"),
    `${names.map((name) => `${sha256(resolve(output, name))}  ${basename(name)}`).join("\n")}\n`,
  );
  return { output, latest, releaseManifest };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const releaseBaseUrl = process.env.NIAN_RELEASE_DOWNLOAD_BASE_URL?.trim();
    if (!releaseBaseUrl) throw new Error("NIAN_RELEASE_DOWNLOAD_BASE_URL is required");
    const result = assembleRelease({
      linuxDir: process.env.NIAN_LINUX_CANDIDATE_DIR ?? resolve(root, "dist/candidate-linux"),
      windowsDir: process.env.NIAN_WINDOWS_CANDIDATE_DIR ?? resolve(root, "dist/candidate-windows"),
      outputDir: process.env.NIAN_RELEASE_OUTPUT_DIR ?? resolve(root, "dist/release"),
      releaseBaseUrl,
    });
    process.stdout.write(`multi-platform release assembled at ${result.output}\n`);
  } catch (error) {
    console.error(`release assembly failed: ${error.message}`);
    process.exitCode = 1;
  }
}
