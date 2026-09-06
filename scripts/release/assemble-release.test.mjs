import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { assembleRelease } from "./assemble-release.mjs";
import { validateMultiplatformRelease } from "./validate-multiplatform-release.mjs";

const { version } = JSON.parse(
  readFileSync(new URL("../../package.json", import.meta.url), "utf8"),
);
const commit = "0b468e5520cf89ee4a10c31de92e48a975c3ffe2";
const sourceSha = "6136812ea6d4e68bdba27e33c2a94382711cdf4f8602ffef056ff792bd6f9818";
const releaseBase = `https://github.com/niand/nian-vision/releases/download/v${version}`;

function hash(data) {
  return createHash("sha256").update(data).digest("hex");
}

function candidate(root, platform, filename, { authenticodeSigned } = {}) {
  const dir = join(root, platform);
  const { mkdirSync } = requireFs();
  mkdirSync(dir, { recursive: true });
  const artifact = Buffer.from(`${platform}-artifact`);
  const signature = Buffer.from(`${platform}-signature\n`);
  const worker = Buffer.from(`${platform}-worker`);
  writeFileSync(join(dir, filename), artifact);
  writeFileSync(join(dir, `${filename}.sig`), signature);
  const manifest = {
    version,
    commit,
    platform,
    target: platform === "linux-x86_64" ? "x86_64-unknown-linux-gnu" : "x86_64-pc-windows-msvc",
    artifact: { filename, sha256: hash(artifact), bytes: artifact.length },
    worker: { sha256: hash(worker), bytes: worker.length },
    updater: {
      signature_file: `${filename}.sig`,
      signature_sha256: hash(signature),
      signature: signature.toString("utf8").trim(),
    },
    ffmpeg: { version: "8.0.3", source_sha256: sourceSha, libraries: {} },
  };
  if (platform === "windows-x86_64") manifest.authenticode_signed = Boolean(authenticodeSigned);
  writeFileSync(join(dir, "platform-manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  return { dir, manifest };
}

function requireFs() {
  // Keeps this fixture synchronous without introducing a top-level dynamic import.
  return { mkdirSync: (path, options) => importFs.mkdirSync(path, options) };
}
import * as importFs from "node:fs";

function fixture() {
  const root = mkdtempSync(join(tmpdir(), "nian-multi-release-"));
  const linuxName = `Nian-Vision_${version}_linux-x86_64.AppImage`;
  const windowsName = `Nian-Vision_${version}_windows-x86_64-setup.exe`;
  const linux = candidate(root, "linux-x86_64", linuxName);
  const windows = candidate(root, "windows-x86_64", windowsName, { authenticodeSigned: false });
  const notes = `# Nian Vision ${version}\n\nFixture notes.\n`;
  for (const dir of [linux.dir, windows.dir]) {
    writeFileSync(join(dir, "RELEASE_NOTES.md"), notes);
    writeFileSync(join(dir, "THIRD_PARTY_NOTICES.txt"), "fixture notices\n");
  }
  const output = join(root, "release");
  return { root, linux, windows, output, linuxName, windowsName };
}

test("multi-platform assembly creates one latest.json and one global checksum authority", () => {
  const f = fixture();
  assembleRelease({ linuxDir: f.linux.dir, windowsDir: f.windows.dir, outputDir: f.output, releaseBaseUrl: releaseBase });
  const latest = JSON.parse(readFileSync(join(f.output, "latest.json"), "utf8"));
  assert.deepEqual(Object.keys(latest.platforms).sort(), ["linux-x86_64", "windows-x86_64"]);
  assert.equal(latest.platforms["linux-x86_64"].url, `${releaseBase}/${f.linuxName}`);
  assert.equal(latest.platforms["windows-x86_64"].url, `${releaseBase}/${f.windowsName}`);
  validateMultiplatformRelease({ outputDir: f.output, expectedVersion: version, expectedCommit: commit, expectedBaseUrl: releaseBase });
});

test("assembly rejects candidates built from different commits", () => {
  const f = fixture();
  f.windows.manifest.commit = "f".repeat(40);
  writeFileSync(join(f.windows.dir, "platform-manifest.json"), `${JSON.stringify(f.windows.manifest, null, 2)}\n`);
  assert.throws(
    () => assembleRelease({ linuxDir: f.linux.dir, windowsDir: f.windows.dir, outputDir: f.output, releaseBaseUrl: releaseBase }),
    /different commits/,
  );
});

test("assembly rejects FFmpeg source/checksum mismatch across platforms", () => {
  const f = fixture();
  f.windows.manifest.ffmpeg.source_sha256 = "0".repeat(64);
  writeFileSync(join(f.windows.dir, "platform-manifest.json"), `${JSON.stringify(f.windows.manifest, null, 2)}\n`);
  assert.throws(
    () => assembleRelease({ linuxDir: f.linux.dir, windowsDir: f.windows.dir, outputDir: f.output, releaseBaseUrl: releaseBase }),
    /same FFmpeg source authority/,
  );
});

test("global validation fails when Windows artifact is mutated after assembly", () => {
  const f = fixture();
  assembleRelease({ linuxDir: f.linux.dir, windowsDir: f.windows.dir, outputDir: f.output, releaseBaseUrl: releaseBase });
  writeFileSync(join(f.output, f.windowsName), "mutated");
  assert.throws(
    () => validateMultiplatformRelease({ outputDir: f.output, expectedVersion: version, expectedCommit: commit, expectedBaseUrl: releaseBase }),
    /artifact hash drift/,
  );
});

test("global validation requires per-platform worker provenance", () => {
  const f = fixture();
  delete f.windows.manifest.worker;
  writeFileSync(join(f.windows.dir, "platform-manifest.json"), `${JSON.stringify(f.windows.manifest, null, 2)}\n`);
  assembleRelease({ linuxDir: f.linux.dir, windowsDir: f.windows.dir, outputDir: f.output, releaseBaseUrl: releaseBase });
  assert.throws(
    () => validateMultiplatformRelease({ outputDir: f.output, expectedVersion: version, expectedCommit: commit, expectedBaseUrl: releaseBase }),
    /worker provenance is missing or malformed/,
  );
});

test("Authenticode requirement fails closed for an explicitly unsigned Windows candidate", () => {
  const f = fixture();
  assembleRelease({ linuxDir: f.linux.dir, windowsDir: f.windows.dir, outputDir: f.output, releaseBaseUrl: releaseBase });
  const previous = process.env.NIAN_REQUIRE_WINDOWS_AUTHENTICODE;
  process.env.NIAN_REQUIRE_WINDOWS_AUTHENTICODE = "true";
  try {
    assert.throws(
      () => validateMultiplatformRelease({ outputDir: f.output, expectedVersion: version, expectedCommit: commit, expectedBaseUrl: releaseBase }),
      /Authenticode is required/,
    );
  } finally {
    if (previous === undefined) delete process.env.NIAN_REQUIRE_WINDOWS_AUTHENTICODE;
    else process.env.NIAN_REQUIRE_WINDOWS_AUTHENTICODE = previous;
  }
});
