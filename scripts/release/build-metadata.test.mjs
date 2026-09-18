import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const buildMetadata = readNormalizedText(new URL("./build-metadata.mjs", import.meta.url));
const linuxStage = readNormalizedText(new URL("./stage-linux.sh", import.meta.url));
const windowsStage = readNormalizedText(new URL("./stage-windows.ps1", import.meta.url));
const metadataScript = fileURLToPath(new URL("./build-metadata.mjs", import.meta.url));
const mise = readFileSync(new URL("../../mise.toml", import.meta.url), "utf8");
const pinnedPnpmVersion = /^pnpm\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$/m.exec(mise)?.[1];

assert.ok(pinnedPnpmVersion, "mise.toml must pin pnpm for the metadata regression fixture");

test("build metadata receives an OS-native preflighted pnpm version instead of spawning pnpm", () => {
  assert.equal(/command\(["']pnpm["']/.test(buildMetadata), false);
  assert.match(buildMetadata, /argValue\(argv, "--pnpm-version"\)/);
  assert.match(buildMetadata, /mise\.toml must pin an exact pnpm version/);
  assert.match(windowsStage, /Get-Command pnpm\.exe -ErrorAction Stop/);
  assert.match(windowsStage, /--pnpm-version \$PnpmVersion/);
  assert.match(linuxStage, /pnpm_version="\$\(pnpm --version\)"/);
  assert.match(linuxStage, /--pnpm-version "\$pnpm_version"/);
});

test("build metadata accepts the pinned pnpm version without invoking pnpm itself", () => {
  const directory = mkdtempSync(join(tmpdir(), "nian-build-metadata-"));
  const output = join(directory, "BUILD_METADATA.json");
  try {
    const result = spawnSync(
      process.execPath,
      [metadataScript, "--output", output, "--target", "test-target", "--pnpm-version", pinnedPnpmVersion],
      { encoding: "utf8" },
    );
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    const metadata = JSON.parse(readFileSync(output, "utf8"));
    assert.equal(metadata.pnpm, pinnedPnpmVersion);
    assert.equal(metadata.target, "test-target");
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("build metadata rejects pnpm version drift before writing provenance", () => {
  const directory = mkdtempSync(join(tmpdir(), "nian-build-metadata-drift-"));
  const output = join(directory, "BUILD_METADATA.json");
  const driftedVersion = `${pinnedPnpmVersion}.drift`;
  try {
    const result = spawnSync(
      process.execPath,
      [metadataScript, "--output", output, "--target", "test-target", "--pnpm-version", driftedVersion],
      { encoding: "utf8" },
    );
    assert.notEqual(result.status, 0);
    assert.ok(result.stderr.includes(`pnpm version ${driftedVersion} does not match mise.toml ${pinnedPnpmVersion}`));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
