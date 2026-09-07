import assert from "node:assert/strict";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const workflow = readNormalizedText(new URL("../../.github/workflows/release.yml", import.meta.url));
const provision = readNormalizedText(new URL("./provision-ffmpeg-msys-tools.ps1", import.meta.url));
const contract = JSON.parse(readNormalizedText(new URL("./ffmpeg-windows-contract.json", import.meta.url)));

const makePackage = {
  name: "make",
  version: "4.4.1-3",
  url: "https://repo.msys2.org/msys/x86_64/make-4.4.1-3-x86_64.pkg.tar.zst",
  sha256: "af0bdba17f06fe037f0194069adaa31a8fe45f1a11381501896aea1fae37bd5d",
  installedExecutable: "C:\\msys64\\usr\\bin\\make.exe",
  msysExecutable: "/usr/bin/make",
  versionLine: "GNU Make 4.4.1",
};

test("RC10 provisions one exact official MSYS2 GNU make package", () => {
  assert.equal(contract.buildContractVersion, 3);
  assert.deepEqual(contract.provisionedMsysPackages, { make: makePackage });
  for (const value of Object.values(makePackage)) assert.ok(provision.includes(value));
  assert.match(provision, /System32\\curl\.exe/);
  assert.match(provision, /Invoke-NianBoundedProcess -FilePath \$Curl/);
  assert.match(provision, /-TimeoutSeconds 180 -Label 'pinned MSYS2 GNU make download'/);
  assert.match(provision, /Get-FileHash -Algorithm SHA256/);
  assert.match(provision, /MSYS2 GNU make package SHA-256 mismatch; installation is blocked/);
  assert.match(provision, /pacman --noconfirm --needed/);
  assert.match(provision, /-U /);
  assert.doesNotMatch(provision, /pacman[^\n]*-Syu|winget upgrade|choco upgrade|rustup update|npm update/i);
  assert.match(provision, /Invoke-NianBoundedProcess -FilePath \$Bash/);
  assert.match(provision, /Invoke-NianNative \{ & \$Pacman -Q make \}/);
  assert.match(provision, /GNU Make 4\.4\.1/);
});

test("RC10 verifies package SHA before local package installation", () => {
  const hashAt = provision.indexOf("Get-FileHash -Algorithm SHA256");
  const hashPassAt = provision.indexOf("SHA-256 verification: PASS");
  const installAt = provision.indexOf("pacman --noconfirm --needed");
  assert.ok(hashAt >= 0 && hashAt < hashPassAt && hashPassAt < installAt);
  const dependencyCheckAt = provision.indexOf("& $Pacman -Q $dependency");
  assert.ok(dependencyCheckAt >= 0 && dependencyCheckAt < installAt);
});

test("Windows release provisions pinned MSYS tools before preflight and quality/cache work", () => {
  const windows = workflow.slice(workflow.indexOf("  build-windows:"), workflow.indexOf("  sign-windows:"));
  const pnpmAt = windows.indexOf("- name: Install pinned pnpm");
  const provisionAt = windows.indexOf("- name: Provision pinned MSYS FFmpeg build tools");
  const preflightAt = windows.indexOf("- name: Windows runner preflight");
  const qualityAt = windows.indexOf("- name: Release scripts and frontend quality");
  const contractAt = windows.indexOf("- name: Compute Windows FFmpeg build contract");
  assert.ok(pnpmAt >= 0 && pnpmAt < provisionAt && provisionAt < preflightAt && preflightAt < qualityAt && qualityAt < contractAt);
  assert.match(windows.slice(provisionAt, preflightAt), /scripts\/release\/provision-ffmpeg-msys-tools\.ps1/);
});
