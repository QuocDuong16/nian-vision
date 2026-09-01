import { createHash } from "node:crypto";
import { readFileSync, readdirSync } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { validateHttpsAuthority } from "./release-authority.mjs";
import { collectVersions, validateVersions } from "./version-check.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const config = JSON.parse(readFileSync(resolve(root, "scripts/release/release-config.json"), "utf8"));

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function checksums(path) {
  const map = new Map();
  for (const line of readFileSync(path, "utf8").split(/\r?\n/)) {
    if (!line) continue;
    const match = line.match(/^([0-9a-f]{64})  (.+)$/);
    if (!match || map.has(match[2])) throw new Error("SHA256SUMS.txt is malformed or duplicated");
    map.set(match[2], match[1]);
  }
  return map;
}

export function validateMultiplatformRelease({ outputDir, expectedVersion, expectedCommit, expectedBaseUrl }) {
  const output = resolve(outputDir);
  const latest = JSON.parse(readFileSync(resolve(output, "latest.json"), "utf8"));
  const manifest = JSON.parse(readFileSync(resolve(output, "release-manifest.json"), "utf8"));
  const sums = checksums(resolve(output, "SHA256SUMS.txt"));
  const notes = readFileSync(resolve(output, "RELEASE_NOTES.md"), "utf8").trim();
  const base = expectedBaseUrl
    ? validateHttpsAuthority(expectedBaseUrl, "expected release base URL").toString().replace(/\/$/, "")
    : null;

  if (latest.version !== expectedVersion || manifest.version !== expectedVersion) throw new Error("release version drift");
  if (expectedCommit && manifest.commit !== expectedCommit) throw new Error("release manifest commit drift");
  if (!notes.startsWith(`# Nian Vision ${expectedVersion}\n`) && notes !== `# Nian Vision ${expectedVersion}`) {
    throw new Error("RELEASE_NOTES.md heading does not match the application version");
  }
  if (latest.notes !== notes) throw new Error("latest.json notes differ from RELEASE_NOTES.md");

  const expectedPlatforms = [config.platform, config.windowsPlatform].sort();
  if (JSON.stringify(Object.keys(latest.platforms ?? {}).sort()) !== JSON.stringify(expectedPlatforms)) {
    throw new Error("latest.json does not contain exactly the required Linux and Windows updater platforms");
  }
  if (JSON.stringify(Object.keys(manifest.platforms ?? {}).sort()) !== JSON.stringify(expectedPlatforms)) {
    throw new Error("release manifest does not contain exactly the required Linux and Windows platforms");
  }
  if (manifest.ffmpeg?.version !== config.ffmpegVersion || manifest.ffmpeg?.source_sha256 !== config.ffmpegSourceSha256) {
    throw new Error("release manifest FFmpeg source authority drift");
  }

  for (const platformKey of expectedPlatforms) {
    const fragment = manifest.platforms[platformKey];
    const latestPlatform = latest.platforms[platformKey];
    if (fragment.platform !== platformKey || fragment.version !== expectedVersion || fragment.commit !== manifest.commit) {
      throw new Error(`${platformKey} platform manifest identity drift`);
    }
    if (!/^[0-9a-f]{64}$/.test(fragment.worker?.sha256 ?? "") || !Number.isSafeInteger(fragment.worker?.bytes) || fragment.worker.bytes <= 0) {
      throw new Error(`${platformKey} worker provenance is missing or malformed`);
    }
    const artifact = resolve(output, fragment.artifact.filename);
    const signature = resolve(output, fragment.updater.signature_file);
    if (sha256(artifact) !== fragment.artifact.sha256) throw new Error(`${platformKey} artifact hash drift`);
    if (sha256(signature) !== fragment.updater.signature_sha256) throw new Error(`${platformKey} updater signature hash drift`);
    const signatureText = readFileSync(signature, "utf8").trim();
    if (signatureText !== fragment.updater.signature || latestPlatform.signature !== signatureText) {
      throw new Error(`${platformKey} updater signature content drift`);
    }
    const url = validateHttpsAuthority(latestPlatform.url, `${platformKey} updater URL`);
    if (decodeURIComponent(basename(url.pathname)) !== fragment.artifact.filename) {
      throw new Error(`${platformKey} updater URL filename drift`);
    }
    if (base && url.toString() !== `${base}/${fragment.artifact.filename}`) {
      throw new Error(`${platformKey} updater URL is not the exact tagged GitHub Release asset URL`);
    }
    if (fragment.ffmpeg?.version !== manifest.ffmpeg.version || fragment.ffmpeg?.source_sha256 !== manifest.ffmpeg.source_sha256) {
      throw new Error(`${platformKey} FFmpeg source authority differs from the release authority`);
    }
  }

  if (manifest.platforms[config.windowsPlatform].authenticode_signed !== true && process.env.NIAN_REQUIRE_WINDOWS_AUTHENTICODE === "true") {
    throw new Error("Windows Authenticode is required but the candidate is classified unsigned");
  }

  const publicFiles = readdirSync(output).filter((name) => name !== "SHA256SUMS.txt").sort();
  for (const name of publicFiles) {
    if (sums.get(name) !== sha256(resolve(output, name))) throw new Error(`SHA256SUMS entry is missing or stale for ${name}`);
  }
  if (sums.size !== publicFiles.length) throw new Error("SHA256SUMS contains an unexpected filename");
  return manifest;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const version = validateVersions(collectVersions());
    validateMultiplatformRelease({
      outputDir: process.env.NIAN_RELEASE_OUTPUT_DIR ?? resolve(root, "dist/release"),
      expectedVersion: version,
      expectedCommit: process.env.NIAN_EXPECTED_RELEASE_COMMIT?.trim() || undefined,
      expectedBaseUrl: process.env.NIAN_EXPECTED_RELEASE_BASE_URL?.trim() || undefined,
    });
    process.stdout.write("multi-platform release validation passed\n");
  } catch (error) {
    console.error(`multi-platform release validation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
