import { createHash } from "node:crypto";
import { readFileSync, readdirSync } from "node:fs";
import { basename, dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { validateHttpsAuthority } from "./release-authority.mjs";
import { collectVersions, validateVersions } from "./version-check.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const releaseConfig = JSON.parse(readFileSync(resolve(root, "scripts/release/release-config.json"), "utf8"));

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function parseChecksums(text) {
  const entries = new Map();
  for (const line of text.split(/\r?\n/)) {
    if (!line.trim()) continue;
    const match = line.match(/^([0-9a-f]{64})  (.+)$/);
    if (!match) throw new Error("SHA256SUMS.txt contains a malformed entry");
    if (entries.has(match[2])) throw new Error(`SHA256SUMS.txt contains duplicate entry: ${match[2]}`);
    entries.set(match[2], match[1]);
  }
  return entries;
}

export function validateReleaseOutput({
  outputDir,
  expectedVersion,
  expectedPlatform = "linux-x86_64",
  expectedArtifactBaseUrl,
  expectedCommit,
}) {
  const output = resolve(outputDir);
  const appImageName = `Nian-Vision_${expectedVersion}_linux-x86_64.AppImage`;
  const signatureName = `${appImageName}.sig`;
  const appImage = resolve(output, appImageName);
  const signature = resolve(output, signatureName);
  const latest = JSON.parse(readFileSync(resolve(output, "latest.json"), "utf8"));
  const manifest = JSON.parse(readFileSync(resolve(output, "release-manifest.json"), "utf8"));
  const checksums = parseChecksums(readFileSync(resolve(output, "SHA256SUMS.txt"), "utf8"));

  if (latest.version !== expectedVersion) throw new Error("latest.json version does not match application version");
  const platformKeys = Object.keys(latest.platforms ?? {});
  if (platformKeys.length !== 1 || platformKeys[0] !== expectedPlatform) {
    throw new Error("latest.json platform key does not match the Linux updater target");
  }
  const platform = latest.platforms[expectedPlatform];
  const url = validateHttpsAuthority(platform.url, "latest.json artifact URL");
  if (decodeURIComponent(basename(url.pathname)) !== appImageName) {
    throw new Error("latest.json artifact URL filename does not match the finalized AppImage");
  }
  if (expectedArtifactBaseUrl) {
    const expectedBase = validateHttpsAuthority(expectedArtifactBaseUrl, "expected release base URL").toString().replace(/\/$/, "");
    if (url.toString() !== `${expectedBase}/${appImageName}`) {
      throw new Error("latest.json artifact URL does not match the expected tagged GitHub Release asset URL");
    }
  }
  const releaseNotes = readFileSync(resolve(output, "RELEASE_NOTES.md"), "utf8").trim();
  if (!releaseNotes.startsWith(`# Nian Vision ${expectedVersion}\n`) && releaseNotes !== `# Nian Vision ${expectedVersion}`) {
    throw new Error("RELEASE_NOTES.md heading does not match the application version");
  }
  if (latest.notes !== releaseNotes) throw new Error("latest.json notes differ from RELEASE_NOTES.md");
  const signatureText = readFileSync(signature, "utf8").trim();
  if (platform.signature !== signatureText) {
    throw new Error("latest.json signature does not match the finalized signature file");
  }

  const appImageHash = sha256(appImage);
  if (manifest.version !== expectedVersion) throw new Error("release manifest version does not match application version");
  if (expectedCommit && manifest.commit !== expectedCommit) throw new Error("release manifest commit does not match the release tag commit");
  if (manifest.platform !== expectedPlatform || manifest.updater?.platform_key !== expectedPlatform) {
    throw new Error("release manifest platform does not match the Linux updater target");
  }
  if (manifest.appimage?.filename !== appImageName || manifest.updater?.artifact !== appImageName) {
    throw new Error("release manifest points to a non-final AppImage filename");
  }
  if (manifest.updater?.signature_file !== signatureName) {
    throw new Error("release manifest signature filename does not match the finalized signature");
  }
  if (manifest.appimage?.sha256 !== appImageHash) {
    throw new Error("release manifest AppImage hash does not match finalized artifact bytes");
  }
  if (manifest.updater?.signature_sha256 !== sha256(signature)) {
    throw new Error("release manifest signature hash does not match finalized signature bytes");
  }
  if (manifest.updater?.metadata !== "latest.json") {
    throw new Error("release manifest updater metadata filename is invalid");
  }

  if (checksums.get(appImageName) !== appImageHash) {
    throw new Error("SHA256SUMS AppImage entry does not match finalized artifact bytes");
  }
  if (checksums.get(signatureName) !== sha256(signature)) {
    throw new Error("SHA256SUMS signature entry does not match finalized signature bytes");
  }
  for (const name of readdirSync(output)) {
    if (name === "SHA256SUMS.txt") continue;
    if (checksums.get(name) !== sha256(resolve(output, name))) {
      throw new Error(`SHA256SUMS entry is missing or stale for ${name}`);
    }
  }
  if (checksums.size !== readdirSync(output).filter((name) => name !== "SHA256SUMS.txt").length) {
    throw new Error("SHA256SUMS contains an unexpected release filename");
  }

  return { appImageName, signatureName };
}

function main() {
  const expectedVersion = validateVersions(collectVersions());
  validateReleaseOutput({
    outputDir: resolve(root, "dist/release"),
    expectedVersion,
    expectedPlatform: releaseConfig.platform,
    expectedArtifactBaseUrl: process.env.NIAN_EXPECTED_RELEASE_BASE_URL?.trim() || undefined,
    expectedCommit: process.env.NIAN_EXPECTED_RELEASE_COMMIT?.trim() || undefined,
  });
  process.stdout.write("finalized Linux updater metadata validation passed\n");
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  try {
    main();
  } catch (error) {
    console.error(`finalized Linux updater metadata validation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
