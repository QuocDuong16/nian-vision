import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import {
  copyFileSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { validateHttpsAuthority } from "./release-authority.mjs";
import { collectVersions, validateVersions } from "./version-check.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const stage = resolve(root, "dist/linux-x86_64");
const bundle = resolve(root, "target/release/bundle/appimage");
const output = resolve(root, "dist/release");
const releaseConfig = JSON.parse(readFileSync(resolve(root, "scripts/release/release-config.json"), "utf8"));
const releaseNotes = readFileSync(resolve(root, "RELEASE_NOTES.md"), "utf8").trim();

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function requireHttpsBase(raw) {
  return validateHttpsAuthority(raw, "NIAN_RELEASE_DOWNLOAD_BASE_URL").toString().replace(/\/$/, "");
}

function oneFile(suffix) {
  const matches = readdirSync(bundle).filter((name) => name.endsWith(suffix));
  if (matches.length !== 1) throw new Error(`expected exactly one ${suffix} in ${bundle}, found ${matches.length}`);
  return resolve(bundle, matches[0]);
}

function copyArtifact(source, name) {
  const destination = resolve(output, name);
  copyFileSync(source, destination);
  return destination;
}

try {
  const version = validateVersions(collectVersions());
  const baseUrl = requireHttpsBase(process.env.NIAN_RELEASE_DOWNLOAD_BASE_URL ?? "");
  const appImageSource = oneFile(".AppImage");
  const signatureSource = `${appImageSource}.sig`;
  if (!statSync(signatureSource).isFile()) throw new Error(`updater signature missing: ${signatureSource}`);

  rmSync(output, { recursive: true, force: true });
  mkdirSync(output, { recursive: true });

  const appImageName = `Nian-Vision_${version}_linux-x86_64.AppImage`;
  const signatureName = `${appImageName}.sig`;
  const appImage = copyArtifact(appImageSource, appImageName);
  const signature = copyArtifact(signatureSource, signatureName);
  for (const name of [
    "THIRD_PARTY_NOTICES.txt",
    "FFMPEG-LGPL-2.1.txt",
    "FFMPEG_BUILD_FLAGS.txt",
    "FFMPEG_CONFIG.h",
    "BUILD_METADATA.json",
    "RELEASE_NOTES.md",
  ]) {
    copyArtifact(name === "RELEASE_NOTES.md" ? resolve(root, name) : resolve(stage, name), name);
  }

  const commit = execFileSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" }).trim();
  const pubDate = execFileSync("git", ["show", "-s", "--format=%cI", "HEAD"], { cwd: root, encoding: "utf8" }).trim();
  const updateSignature = readFileSync(signature, "utf8").trim();
  if (!updateSignature) throw new Error("updater signature file is empty");

  const latest = {
    version,
    notes: releaseNotes,
    pub_date: pubDate,
    platforms: {
      "linux-x86_64": {
        signature: updateSignature,
        url: `${baseUrl}/${appImageName}`,
      },
    },
  };
  writeFileSync(resolve(output, "latest.json"), `${JSON.stringify(latest, null, 2)}\n`);

  const ffmpegLibraries = {};
  for (const name of ["libavformat.so.62", "libavcodec.so.62", "libavutil.so.60"]) {
    const path = resolve(stage, "lib/nian-vision", name);
    ffmpegLibraries[name] = { sha256: sha256(path), bytes: statSync(path).size };
  }
  const worker = resolve(stage, "bin/nian-media-worker");
  const manifest = {
    version,
    commit,
    target: releaseConfig.target,
    platform: releaseConfig.platform,
    baseline: releaseConfig.baseline,
    installer: null,
    appimage: {
      filename: appImageName,
      sha256: sha256(appImage),
      bytes: statSync(appImage).size,
    },
    worker: {
      sha256: sha256(worker),
      bytes: statSync(worker).size,
    },
    ffmpeg: {
      version: releaseConfig.ffmpegVersion,
      source_sha256: releaseConfig.ffmpegSourceSha256,
      libraries: ffmpegLibraries,
    },
    updater: {
      artifact: appImageName,
      signature_file: signatureName,
      signature_sha256: sha256(signature),
      signed: true,
      platform_key: "linux-x86_64",
      metadata: "latest.json",
    },
    signing: {
      updater_signature: "required",
      appimage_embedded_gpg_signature: "not_relied_upon",
    },
  };
  writeFileSync(resolve(output, "release-manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);

  const checksumTargets = readdirSync(output)
    .filter((name) => name !== "SHA256SUMS.txt")
    .sort();
  const checksums = checksumTargets
    .map((name) => `${sha256(resolve(output, name))}  ${basename(name)}`)
    .join("\n");
  writeFileSync(resolve(output, "SHA256SUMS.txt"), `${checksums}\n`);
  process.stdout.write(`Linux release finalized at ${output}\n`);
} catch (error) {
  console.error(`Linux release finalization failed: ${error.message}`);
  process.exitCode = 1;
}
