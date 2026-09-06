import assert from "node:assert/strict";
import test from "node:test";

import { normalizeNewlines, readNormalizedText } from "./test-text.mjs";

const workflow = readNormalizedText(new URL("../../.github/workflows/release.yml", import.meta.url));
const windowsConfigWriter = readNormalizedText(new URL("./write-tauri-windows-release-config.mjs", import.meta.url));
const windowsFfmpeg = readNormalizedText(new URL("./build-ffmpeg-windows.ps1", import.meta.url));
const windowsNative = readNormalizedText(new URL("./windows-native.ps1", import.meta.url));
const releaseConfig = JSON.parse(readNormalizedText(new URL("./release-config.json", import.meta.url)));
const windowsStage = readNormalizedText(new URL("./stage-windows.ps1", import.meta.url));
const windowsPreflight = readNormalizedText(new URL("./preflight-windows.ps1", import.meta.url));
const windowsAuthenticode = readNormalizedText(new URL("./sign-authenticode-windows.ps1", import.meta.url));
const windowsRuntimeClosure = readNormalizedText(new URL("./windows-runtime-closure.ps1", import.meta.url));
const windowsRuntimeClassifierTest = readNormalizedText(new URL("./test-windows-runtime-classifier.ps1", import.meta.url));
const windowsInstallerSmoke = readNormalizedText(new URL("./smoke-windows-installer.ps1", import.meta.url));
const windowsNsisHooks = readNormalizedText(new URL("../../apps/nian-desktop/windows/nsis-hooks.nsh", import.meta.url));
const desktop = readNormalizedText(new URL("../../apps/nian-desktop/src/lib.rs", import.meta.url));
const worker = readNormalizedText(new URL("../../apps/nian-media-worker/src/main.rs", import.meta.url));

test("Windows release contract text normalization is identical for LF and CRLF", () => {
  const crlf = workflow.replace(/\n/g, "\r\n");
  assert.equal(normalizeNewlines(crlf), workflow);
});

test("Windows FFmpeg build is source-pinned MSVC shared LGPL with bounded resources", () => {
  assert.equal(releaseConfig.ffmpegVersion, "8.0.3");
  assert.equal(releaseConfig.ffmpegSourceUrl, "https://ffmpeg.org/releases/ffmpeg-8.0.3.tar.xz");
  assert.equal(releaseConfig.ffmpegSourceSha256, "6136812ea6d4e68bdba27e33c2a94382711cdf4f8602ffef056ff792bd6f9818");
  assert.match(windowsFfmpeg, /--toolchain=msvc/);
  assert.match(windowsFfmpeg, /--enable-shared/);
  assert.match(windowsFfmpeg, /--disable-static/);
  assert.match(windowsFfmpeg, /--disable-gpl/);
  assert.match(windowsFfmpeg, /--disable-nonfree/);
  assert.match(windowsFfmpeg, /Get-FileHash -Algorithm SHA256/);
  assert.match(windowsFfmpeg, /Invoke-NianNative \{ curl\.exe/);
  assert.match(windowsFfmpeg, /Invoke-NianNative \{ tar\.exe/);
  assert.match(windowsFfmpeg, /\[Environment\]::ProcessorCount/);
  assert.match(windowsFfmpeg, /\[Math\]::Max\(2, \[Math\]::Min\(\$reportedCpuCount, 8\)\)/);
  assert.match(windowsFfmpeg, /make -j\$buildJobs/);
  assert.equal(windowsFfmpeg.includes("make -j2"), false);
  for (const phase of ["download", "SHA-256 verification", "extraction", "configure", "compile", "install", "configuration and license validation", "runtime staging and validation"]) {
    assert.ok(windowsFfmpeg.includes(`Invoke-FfmpegPhase "${phase}"`), `missing FFmpeg phase timing for ${phase}`);
  }
  assert.match(windowsFfmpeg, /avformat\.lib/);
  assert.match(windowsFfmpeg, /avcodec\.lib/);
  assert.match(windowsFfmpeg, /avutil\.lib/);
  assert.match(windowsFfmpeg, /avformat-62\.dll/);
  assert.match(windowsFfmpeg, /avcodec-62\.dll/);
  assert.match(windowsFfmpeg, /avutil-60\.dll/);
  assert.equal(/gyan\.dev|github\.com\/BtbN|prebuilt/i.test(windowsFfmpeg), false);
  assert.match(workflow, /Build pinned FFmpeg 8\.0\.3 Windows MSVC runtime[\s\S]*?timeout-minutes: 60[\s\S]*?build-ffmpeg-windows\.ps1/);
});

test("Windows release PowerShell scripts share the fail-closed native helper", () => {
  for (const [name, source] of [
    ["preflight", windowsPreflight],
    ["FFmpeg", windowsFfmpeg],
    ["staging", windowsStage],
    ["Authenticode", windowsAuthenticode],
    ["installer smoke", windowsInstallerSmoke],
  ]) {
    assert.match(source, /windows-native\.ps1/, `${name} does not load the common native helper`);
  }
  assert.match(windowsAuthenticode, /Invoke-NianNative \{ & \$signTool sign/);
  assert.match(windowsAuthenticode, /Invoke-NianNative \{ & \$signTool verify/);
  assert.match(windowsStage, /Invoke-NianNative \{ dumpbin\.exe/);
  assert.match(windowsInstallerSmoke, /Invoke-NianNative \{ cargo\.exe run --quiet -p nian-settings-fixture/);
});

test("Windows release workflow routes required native tools through one fail-closed helper", () => {
  assert.match(windowsNative, /PSNativeCommandUseErrorActionPreference/);
  assert.match(windowsNative, /\$LASTEXITCODE/);
  assert.match(windowsNative, /throw "required native command failed/);
  for (const tool of ["git.exe", "rustup.exe", "rustc.exe", "npm.cmd", "pnpm.cmd", "cargo.exe", "node.exe"]) {
    assert.ok(workflow.includes(`Invoke-NianNative { ${tool}`), `workflow does not route ${tool} through fail-closed helper`);
  }
  assert.match(workflow, /Invoke-NianNative \{ npm\.cmd install --global --prefix \$corepackRoot corepack@0\.35\.0 \}/);
  assert.match(workflow, /Invoke-NianNative \{ & \$corepack enable --install-directory \$corepackRoot \}/);
  assert.match(workflow, /Invoke-NianNative \{ & \.\.\\\.\.\\ui\\node_modules\\\.bin\\tauri\.CMD build/);
  assert.match(workflow, /Invoke-NianNative \{ cargo\.exe tauri bundle/);
  assert.match(workflow, /Invoke-NianNative \{ cargo\.exe tauri signer sign/);
  assert.match(workflow, /Invoke-NianNative \{ cargo\.exe run --quiet -p nian-release-verifier/);
  assert.match(windowsFfmpeg, /Invoke-NianNative \{ & \$Bash[\s\S]*?make -j\$buildJobs/);
  assert.match(windowsFfmpeg, /Invoke-NianNative \{ & \$Bash[\s\S]*?make install/);
  assert.match(windowsFfmpeg, /Invoke-NianNative \{ cmd\.exe/);
  const windowsJobs = workflow.slice(workflow.indexOf("  build-windows:"), workflow.indexOf("  verify-release:"));
  assert.equal(/(?:^|\n)\s+npm install --global corepack@0\.35\.0/.test(windowsJobs), false);
  const quality = workflow.slice(workflow.indexOf("- name: Release scripts and frontend quality"), workflow.indexOf("- name: Build pinned FFmpeg 8.0.3 Windows MSVC runtime"));
  for (const command of ["release:test", "lint", "typecheck", "test", "build"]) {
    assert.match(quality, new RegExp(`Invoke-NianNative \\{ pnpm\\.cmd ${command.replace(":", "\\:")} \\}`));
  }
});

test("Windows staging mechanically rejects missing and developer-resolved DLLs", () => {
  assert.match(windowsStage, /dumpbin\.exe \/nologo \/dependents/);
  assert.match(windowsRuntimeClosure, /application-owned Windows dependency is missing from stage/);
  assert.match(windowsStage, /msys64/);
  assert.match(windowsStage, /vcpkg/);
  assert.match(windowsStage, /target\/\$Target\/release\/nian-desktop\.exe/);
  assert.match(workflow, /Restage with desktop dependency closure/);
  assert.match(windowsStage, /Remove-Item Env:NIAN_FFMPEG_LIB_DIR/);
  assert.match(windowsStage, /stage-runtime-smoke\.mjs/);
});

test("Windows VC runtime classification precedes generic System32 detection and stages application-local", () => {
  const localAt = windowsRuntimeClosure.indexOf("Kind = 'ApplicationLocal'");
  const vcAt = windowsRuntimeClosure.indexOf("Test-VcRedistributableDependency $Name");
  const apiAt = windowsRuntimeClosure.indexOf("API-MS-WIN-");
  const system32At = windowsRuntimeClosure.indexOf("Join-Path $System32 $Name");
  assert.ok(localAt >= 0 && vcAt > localAt && apiAt > vcAt && system32At > vcAt);
  assert.match(windowsRuntimeClosure, /Copy-Item -Force \$resolution\.Source \$local/);
  assert.match(windowsStage, /Stage-WindowsDependency \$dep \$object \$Runtime \$System32 \$env:VCToolsRedistDir/);
  assert.match(workflow, /Test Windows VC runtime classifier and application-local staging[\s\S]*?test-windows-runtime-classifier\.ps1/);

  for (const name of ["VCRUNTIME140.dll", "MSVCP140.dll", "VCRUNTIME140_1.dll", "CONCRT140.dll"]) {
    assert.ok(windowsRuntimeClassifierTest.includes(name));
  }
  assert.match(windowsRuntimeClassifierTest, /runner-system-copy/);
  assert.match(windowsRuntimeClassifierTest, /did not resolve from the configured VC redist source/);
  assert.match(windowsRuntimeClassifierTest, /API-MS-WIN-CORE-FILE-L1-1-0\.DLL/);
});

test("final Windows closure includes the desktop and every application-local DLL", () => {
  assert.match(windowsStage, /if \(Test-Path \$DesktopSource\) \{ \$closureRoots \+= \$DesktopSource \}/);
  assert.match(windowsStage, /Get-ChildItem \$Runtime -File -Filter '\*\.dll'/);
  assert.match(windowsStage, /Ensure-DependencyClosure @\(\$finalClosureRoots \| Sort-Object -Unique\)/);
  assert.match(workflow, /Restage with desktop dependency closure/);
});

test("Windows Tauri config is NSIS-only with normal WebView2 bootstrapper and no updater private key", () => {
  assert.match(windowsConfigWriter, /targets: \["nsis"\]/);
  assert.match(windowsConfigWriter, /createUpdaterArtifacts: false/);
  assert.match(windowsConfigWriter, /downloadBootstrapper/);
  assert.match(windowsConfigWriter, /beforeBuildCommand: ""/);
  assert.match(windowsConfigWriter, /TAURI_SIGNING_PRIVATE_KEY/);
  assert.match(windowsConfigWriter, /WINDOWS_SIGNING_PFX/);
  assert.equal(windowsConfigWriter.includes("webviewInstallMode: { type: \"skip\""), false);
});

test("GitHub workflow has parallel Windows build/sign path on explicit windows-2022", () => {
  assert.match(workflow, /^  build-windows:/m);
  assert.match(workflow, /^  sign-windows:/m);
  assert.match(workflow, /build-windows:[\s\S]*?runs-on: windows-2022/);
  assert.match(workflow, /build-linux:[\s\S]*?needs: release-preflight/);
  assert.match(workflow, /build-windows:[\s\S]*?needs: release-preflight/);
  assert.equal(/build-windows:[\s\S]*?needs: build-linux/.test(workflow), false);
});

test("verify-release requires both signed platform candidates", () => {
  assert.match(workflow, /verify-release:[\s\S]*?needs: \[release-preflight, sign-linux, sign-windows\]/);
  assert.match(workflow, /publish-release:[\s\S]*?needs: \[release-preflight, verify-release\]/);
});

test("Windows private signing material appears only in sign-windows", () => {
  const signAt = workflow.indexOf("  sign-windows:");
  const verifyAt = workflow.indexOf("  verify-release:");
  assert.ok(signAt > 0 && verifyAt > signAt);
  const signBody = workflow.slice(signAt, verifyAt);
  const outside = workflow.slice(0, signAt) + workflow.slice(verifyAt);
  for (const secret of [
    "WINDOWS_SIGNING_PFX_BASE64",
    "WINDOWS_SIGNING_PFX_PASSWORD",
    "TAURI_SIGNING_PRIVATE_KEY",
    "TAURI_SIGNING_PRIVATE_KEY_PASSWORD",
  ]) {
    assert.match(signBody, new RegExp(secret));
    if (secret.startsWith("WINDOWS_")) assert.equal(outside.includes(secret), false);
  }
});

test("only publish-release retains contents write authority", () => {
  assert.equal([...workflow.matchAll(/contents: write/g)].length, 1);
  assert.match(workflow, /publish-release:[\s\S]*?permissions:\n      contents: write/);
});

test("Windows installer smoke proves disposable install and exact bundled bytes", () => {
  assert.match(windowsInstallerSmoke, /\/D=\$InstallRoot/);
  assert.match(windowsInstallerSmoke, /Get-FileHash -Algorithm SHA256/);
  assert.match(windowsInstallerSmoke, /installed desktop bytes differ from the exact signed bundle input/);
  assert.match(windowsInstallerSmoke, /installed runtime bytes differ from staged release input/);
  assert.match(windowsInstallerSmoke, /release-evidence\\BUILD_METADATA\.json/);
  assert.match(windowsInstallerSmoke, /stage-runtime-smoke\.mjs/);
});

test("Windows desktop smoke proves native power subscription and Job Object hard-death containment", () => {
  assert.match(desktop, /NIAN_DESKTOP_POWER_SMOKE_FILE/);
  assert.match(desktop, /windows_power_subscription_ready/);
  assert.match(desktop, /NIAN_DESKTOP_CONTAINMENT_SMOKE_FILE/);
  assert.match(desktop, /contain_worker_process/);
  assert.match(desktop, /__containment-smoke/);
  assert.match(worker, /Some\("__containment-smoke"\)/);
  assert.match(worker, /NIAN_WORKER_CONTAINMENT_SMOKE/);
  assert.match(worker, /Deliberately independent of stdin and IPC/);
  assert.match(windowsInstallerSmoke, /Windows Job Object did not reap the installed media worker/);
  assert.match(windowsInstallerSmoke, /did not prove the native Windows power subscription/);
});

test("Windows installer upgrade relies on desktop ownership instead of global worker-name killing", () => {
  assert.equal(/taskkill[\s\S]*?nian-media-worker\.exe/i.test(windowsNsisHooks), false);
  assert.match(windowsNsisHooks, /Windows Job Object/);
  assert.match(windowsNsisHooks, /DeleteRegValue HKCU/);
  assert.match(windowsInstallerSmoke, /Start-DesktopContainmentSmoke/);
  assert.match(windowsInstallerSmoke, /NSIS direct reinstall with running desktop/);
  assert.match(windowsInstallerSmoke, /owned worker shutdown during direct reinstall/);
  assert.match(windowsInstallerSmoke, /owned worker restarted or survived during installer file replacement/);
  assert.match(windowsInstallerSmoke, /Run-DesktopSmoke \(Join-Path \$InstallRoot "nian-desktop\.exe"\)/);
});

test("Windows in-app updater policy supports NSIS without APPIMAGE and avoids a post-handoff restart", () => {
  assert.match(desktop, /UpdateInstallPlatform::WindowsNsis/);
  assert.match(desktop, /validate_update_install_platform\(platform, std::env::var_os\("APPIMAGE"\)\.is_some\(\)\)/);
  assert.match(desktop, /if platform == UpdateInstallPlatform::LinuxAppImage[\s\S]*?restart\(\)/);
  assert.equal(desktop.includes('application updates are only packaged for Linux'), false);
});

test("Windows install upgrade and uninstall preserve authoritative user data", () => {
  assert.match(windowsInstallerSmoke, /nian-settings-fixture -- create/);
  assert.match(windowsInstallerSmoke, /nian-settings-fixture -- verify/);
  assert.match(windowsInstallerSmoke, /fresh install unexpectedly enabled launch-at-login/);
  assert.match(windowsInstallerSmoke, /M7 launch-at-login reconciliation did not repair the executable path/);
  assert.match(windowsInstallerSmoke, /uninstall deleted authoritative settings\.sqlite3/);
  assert.match(windowsInstallerSmoke, /uninstall deleted recording footage/);
  assert.match(windowsInstallerSmoke, /uninstall left stale Nian Vision launch-at-login registration/);
});
