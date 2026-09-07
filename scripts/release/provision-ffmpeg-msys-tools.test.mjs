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

const diffutilsPackage = {
  name: "diffutils",
  version: "3.12-1",
  url: "https://mirror.msys2.org/msys/x86_64/diffutils-3.12-1-x86_64.pkg.tar.zst",
  sha256: "7902c8ce3d4dd69a0f5e98dc9d5c83c17b23314ba486169db57ef6e2835ce3b6",
  installedExecutable: "C:\\msys64\\usr\\bin\\cmp.exe",
  msysExecutable: "/usr/bin/cmp",
};

test("RC10 provisions exact official MSYS2 GNU make and diffutils packages", () => {
  assert.equal(contract.buildContractVersion, 3);
  assert.deepEqual(contract.provisionedMsysPackages, {
    make: makePackage,
    diffutils: diffutilsPackage,
  });
  for (const pkg of [makePackage, diffutilsPackage]) {
    for (const value of Object.values(pkg)) assert.ok(provision.includes(value));
  }
  assert.match(provision, /System32\\curl\.exe/);
  assert.match(provision, /Invoke-NianBoundedProcess -FilePath \$Curl/);
  assert.match(provision, /pinned MSYS2 \$\(\$expected\.Name\) download/);
  assert.match(provision, /Get-FileHash -Algorithm SHA256/);
  assert.match(provision, /package SHA-256 mismatch; installation is blocked/);
  assert.match(provision, /pacman --noconfirm --needed -U/);
  assert.doesNotMatch(provision, /pacman[^\n]*-Syu|pacman[^\n]*-Syyu|winget upgrade|choco upgrade|rustup update|npm update/i);
  assert.match(provision, /Invoke-NianBoundedProcess -FilePath \$Bash/);
  assert.match(provision, /Invoke-NianNative \{ & \$Pacman -Q \$expected\.Name \}/);
  assert.match(provision, /GNU Make 4\.4\.1/);
  assert.match(provision, /diffutils 3\.12-1\s+-> libiconv, libintl, sh/);
  assert.match(provision, /foreach \(\$dependency in @\('libiconv', 'libintl'\)\)/);
  assert.match(provision, /& \$Pacman -Q bash/);
  assert.match(provision, /C:\\msys64\\usr\\bin\\sh\.exe/);
});

test("RC10 verifies each package SHA before any local package installation", () => {
  const installLoopAt = provision.lastIndexOf("foreach ($expected in $ExpectedPackages)");
  assert.ok(installLoopAt >= 0);
  const loop = provision.slice(installLoopAt);
  const downloadAt = loop.indexOf("Invoke-NianBoundedProcess -FilePath $Curl");
  const bytesAt = loop.indexOf("downloaded byte count:");
  const hashAt = loop.indexOf("Get-FileHash -Algorithm SHA256");
  const mismatchAt = loop.indexOf("package SHA-256 mismatch; installation is blocked");
  const hashPassAt = loop.indexOf("SHA-256 verification: PASS");
  const installCommandAt = loop.indexOf("$installCommand =");
  const installProcessAt = loop.indexOf("Invoke-NianBoundedProcess -FilePath $Bash", installCommandAt);
  assert.ok(
    downloadAt >= 0 &&
      downloadAt < bytesAt &&
      bytesAt < hashAt &&
      hashAt < mismatchAt &&
      mismatchAt < hashPassAt &&
      hashPassAt < installCommandAt &&
      installCommandAt < installProcessAt,
  );
  const dependencyCheckAt = provision.indexOf("& $Pacman -Q $dependency");
  const shProviderAt = provision.indexOf("& $Pacman -Q bash");
  assert.ok(dependencyCheckAt >= 0 && dependencyCheckAt < installLoopAt);
  assert.ok(shProviderAt >= 0 && shProviderAt < installLoopAt);
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
  assert.equal([...windows.matchAll(/Provision pinned MSYS FFmpeg build tools/g)].length, 1);
});
