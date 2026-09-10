import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import test from "node:test";

const patcher = fileURLToPath(new URL("./patch-tauri-bundle-type.mjs", import.meta.url));
const unknown = Buffer.from("__TAURI_BUNDLE_TYPE_VAR_UNK", "ascii");
const nsis = Buffer.from("__TAURI_BUNDLE_TYPE_VAR_NSS", "ascii");

function runFixture(chunks, type = "nsis") {
  const directory = mkdtempSync(join(tmpdir(), "nian-tauri-bundle-type-"));
  const binary = join(directory, "nian-desktop.exe");
  writeFileSync(binary, Buffer.concat(chunks));
  const result = spawnSync(process.execPath, [patcher, binary, type], { encoding: "utf8" });
  return { directory, binary, result };
}

test("NSIS bundle type patch replaces exactly the Tauri unknown token without changing file length", () => {
  const prefix = Buffer.from([0x4d, 0x5a, 0x00, 0x01, 0x02]);
  const suffix = Buffer.from([0xaa, 0xbb, 0xcc, 0xdd]);
  const fixture = runFixture([prefix, unknown, suffix]);
  try {
    assert.equal(fixture.result.status, 0, fixture.result.stderr);
    const patched = readFileSync(fixture.binary);
    assert.equal(patched.length, prefix.length + unknown.length + suffix.length);
    assert.equal(patched.indexOf(unknown), -1);
    assert.equal(patched.indexOf(nsis), prefix.length);
    assert.deepEqual(patched.subarray(0, prefix.length), prefix);
    assert.deepEqual(patched.subarray(-suffix.length), suffix);
  } finally {
    rmSync(fixture.directory, { recursive: true, force: true });
  }
});

test("bundle type patch fails closed when the Tauri token is missing or duplicated", () => {
  for (const chunks of [
    [Buffer.from("MZ-no-token")],
    [unknown, Buffer.from("middle"), unknown],
  ]) {
    const fixture = runFixture(chunks);
    try {
      assert.notEqual(fixture.result.status, 0);
      assert.match(fixture.result.stderr, /expected exactly one unpatched Tauri bundle type token/);
    } finally {
      rmSync(fixture.directory, { recursive: true, force: true });
    }
  }
});

test("bundle type patch rejects unsupported package types instead of guessing", () => {
  const fixture = runFixture([unknown], "msi");
  try {
    assert.notEqual(fixture.result.status, 0);
    assert.match(fixture.result.stderr, /unsupported Tauri bundle type: msi/);
    assert.notEqual(readFileSync(fixture.binary).indexOf(unknown), -1);
  } finally {
    rmSync(fixture.directory, { recursive: true, force: true });
  }
});
