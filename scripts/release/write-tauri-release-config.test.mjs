import assert from "node:assert/strict";
import test from "node:test";

import { createReleaseConfig, validateEndpoint } from "./write-tauri-release-config.mjs";

test("release config contains only public updater data and bundle configuration", () => {
  const privateKey = "TEST-PRIVATE-UPDATER-KEY";
  const privatePassword = "TEST-PRIVATE-UPDATER-PASSWORD";
  const endpoint = "https://updates.niand.io.vn/latest.json";
  const pubkey = "PUBLIC-UPDATER-KEY";
  const config = createReleaseConfig({
    endpoint,
    pubkey,
    workerBase: "/workspace/dist/linux-x86_64/tauri/nian-media-worker",
    appimageFiles: { "/usr/share/nian-vision/BUILD_METADATA.json": "../../dist/BUILD_METADATA.json" },
  });
  const serialized = JSON.stringify(config);

  assert.deepEqual(Object.keys(config).sort(), ["build", "bundle", "plugins"]);
  assert.equal(config.build.beforeBuildCommand, "");
  assert.equal(config.plugins.updater.pubkey, pubkey);
  assert.deepEqual(config.plugins.updater.endpoints, [endpoint]);
  assert.equal(config.bundle.createUpdaterArtifacts, false);
  assert.equal(serialized.includes(privateKey), false);
  assert.equal(serialized.includes(privatePassword), false);
  assert.equal(serialized.includes("TAURI_SIGNING_PRIVATE_KEY"), false);
  assert.equal(serialized.includes("TAURI_SIGNING_PRIVATE_KEY_PASSWORD"), false);
});

test("Linux updater artifacts can be deliberately enabled for compatibility-only builds", () => {
  const config = createReleaseConfig({
    endpoint: "https://updates.niand.io.vn/latest.json",
    pubkey: "PUBLIC-UPDATER-KEY",
    workerBase: "/workspace/dist/linux-x86_64/tauri/nian-media-worker",
    appimageFiles: {},
    createUpdaterArtifacts: true,
  });
  assert.equal(config.bundle.createUpdaterArtifacts, true);
});

test("production updater authority must be HTTPS and non-placeholder", () => {
  assert.equal(validateEndpoint("https://updates.niand.io.vn/latest.json"), "https://updates.niand.io.vn/latest.json");
  for (const invalid of [
    "http://updates.nian.invalid/latest.json",
    "https://localhost/latest.json",
    "https://127.0.0.1/latest.json",
    "https://example.com/latest.json",
    "https://cdn.example.com/latest.json",
    "https://updates.invalid/latest.json",
    "https://updates.test/latest.json",
  ]) {
    assert.throws(() => validateEndpoint(invalid));
  }
});
