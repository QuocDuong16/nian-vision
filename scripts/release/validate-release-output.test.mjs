import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { validateReleaseOutput } from "./validate-release-output.mjs";

const version = "0.1.0";
const platform = "linux-x86_64";
const appImageName = `Nian-Vision_${version}_linux-x86_64.AppImage`;
const signatureName = `${appImageName}.sig`;

function sha256(data) {
  return createHash("sha256").update(data).digest("hex");
}

function fixture() {
  const dir = mkdtempSync(join(tmpdir(), "nian-release-output-"));
  const app = Buffer.from("appimage-final-bytes");
  const sig = Buffer.from("tauri-signature-base64\n");
  writeFileSync(join(dir, appImageName), app);
  writeFileSync(join(dir, signatureName), sig);
  const latest = {
    version,
    notes: "fixture",
    pub_date: "2026-08-31T00:00:00Z",
    platforms: {
      [platform]: {
        signature: sig.toString("utf8").trim(),
        url: `https://downloads.niand.io.vn/releases/${appImageName}`,
      },
    },
  };
  const manifest = {
    version,
    platform,
    appimage: { filename: appImageName, sha256: sha256(app), bytes: app.length },
    updater: {
      artifact: appImageName,
      signature_file: signatureName,
      signature_sha256: sha256(sig),
      platform_key: platform,
      metadata: "latest.json",
    },
  };
  writeFileSync(join(dir, "latest.json"), `${JSON.stringify(latest, null, 2)}\n`);
  writeFileSync(join(dir, "release-manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`);
  const checksumTargets = [
    [appImageName, app],
    [signatureName, sig],
    ["latest.json", readFileSync(join(dir, "latest.json"))],
    ["release-manifest.json", readFileSync(join(dir, "release-manifest.json"))],
  ];
  writeFileSync(
    join(dir, "SHA256SUMS.txt"),
    `${checksumTargets.map(([name, bytes]) => `${sha256(bytes)}  ${name}`).join("\n")}\n`,
  );
  return { dir, latest, manifest };
}

function rewriteJson(dir, name, value) {
  writeFileSync(join(dir, name), `${JSON.stringify(value, null, 2)}\n`);
}

function validate(dir) {
  return validateReleaseOutput({ outputDir: dir, expectedVersion: version, expectedPlatform: platform });
}

test("finalized updater metadata is internally consistent", () => {
  const { dir } = fixture();
  assert.equal(validate(dir).appImageName, appImageName);
});

test("latest.json version drift fails", () => {
  const { dir, latest } = fixture();
  latest.version = "9.9.9";
  rewriteJson(dir, "latest.json", latest);
  assert.throws(() => validate(dir), /latest.json version/);
});

test("latest.json stale artifact filename fails", () => {
  const { dir, latest } = fixture();
  latest.platforms[platform].url = "https://downloads.niand.io.vn/releases/Nian-Vision_old.AppImage";
  rewriteJson(dir, "latest.json", latest);
  assert.throws(() => validate(dir), /URL filename/);
});

test("latest.json signature drift fails", () => {
  const { dir, latest } = fixture();
  latest.platforms[platform].signature = "different-signature";
  rewriteJson(dir, "latest.json", latest);
  assert.throws(() => validate(dir), /signature does not match/);
});

test("release-manifest AppImage hash drift fails", () => {
  const { dir, manifest } = fixture();
  manifest.appimage.sha256 = "0".repeat(64);
  rewriteJson(dir, "release-manifest.json", manifest);
  assert.throws(() => validate(dir), /manifest AppImage hash/);
});

test("SHA256SUMS stale AppImage entry fails", () => {
  const { dir } = fixture();
  const sums = readFileSync(join(dir, "SHA256SUMS.txt"), "utf8").replace(
    /^[0-9a-f]{64}(?=  Nian-Vision_0\.1\.0_linux-x86_64\.AppImage$)/m,
    "0".repeat(64),
  );
  writeFileSync(join(dir, "SHA256SUMS.txt"), sums);
  assert.throws(() => validate(dir), /SHA256SUMS AppImage/);
});
