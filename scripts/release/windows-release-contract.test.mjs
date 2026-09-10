import assert from "node:assert/strict";
import test from "node:test";

import { normalizeNewlines, readNormalizedText } from "./test-text.mjs";

const workflow = readNormalizedText(new URL("../../.github/workflows/release.yml", import.meta.url));
const gitAttributes = readNormalizedText(new URL("../../.gitattributes", import.meta.url));
const windowsConfigWriter = readNormalizedText(new URL("./write-tauri-windows-release-config.mjs", import.meta.url));
const windowsFfmpeg = readNormalizedText(new URL("./build-ffmpeg-windows.ps1", import.meta.url));
const windowsFfmpegValidator = readNormalizedText(new URL("./validate-ffmpeg-windows.mjs", import.meta.url));
const windowsFfmpegValidatorPs = readNormalizedText(new URL("./validate-ffmpeg-windows.ps1", import.meta.url));
const windowsFfmpegArtifactCache = readNormalizedText(new URL("./restore-ffmpeg-windows-artifact-cache.ps1", import.meta.url));
const windowsFfmpegContract = JSON.parse(readNormalizedText(new URL("./ffmpeg-windows-contract.json", import.meta.url)));
const windowsNative = readNormalizedText(new URL("./windows-native.ps1", import.meta.url));
const windowsBoundedProcess = readNormalizedText(new URL("./windows-bounded-process.ps1", import.meta.url));
const windowsBashScript = readNormalizedText(new URL("./windows-bash-script.ps1", import.meta.url));
const windowsMsvcToolchain = readNormalizedText(new URL("./windows-msvc-toolchain.ps1", import.meta.url));
const windowsFfmpegMsysEnvironment = readNormalizedText(new URL("./windows-ffmpeg-msys-environment.ps1", import.meta.url));
const windowsFfmpegProvision = readNormalizedText(new URL("./provision-ffmpeg-msys-tools.ps1", import.meta.url));
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
const storageManager = readNormalizedText(new URL("../../crates/nian-application/src/storage_manager.rs", import.meta.url));
const cameraServiceIntegration = readNormalizedText(new URL("../../crates/nian-application/tests/camera_service.rs", import.meta.url));
const recordingControllerIntegration = readNormalizedText(new URL("../../crates/nian-application/tests/recording_controller.rs", import.meta.url));
const recordingController = readNormalizedText(new URL("../../crates/nian-application/src/recording_controller.rs", import.meta.url));
const workerSupervisorStubs = readNormalizedText(new URL("../../crates/nian-application/tests/worker_supervisor_stubs.rs", import.meta.url));
const workerLive = readNormalizedText(new URL("../../apps/nian-media-worker/src/live.rs", import.meta.url));

test("Windows all-target Clippy does not import Unix-only NaiveDate into the shared test module", () => {
  assert.equal(storageManager.includes("use chrono::{NaiveDate, NaiveDateTime};"), false);
  assert.match(storageManager, /use chrono::NaiveDateTime;/);
  assert.match(storageManager, /chrono::NaiveDate::from_ymd_opt/);
});

test("Windows test fixtures do not hard-code Unix-only storage or live-output paths", () => {
  for (const [name, source] of [
    ["camera service integration", cameraServiceIntegration],
    ["recording controller integration", recordingControllerIntegration],
    ["recording controller unit tests", recordingController],
    ["worker supervisor stubs", workerSupervisorStubs],
  ]) {
    assert.equal(/storage_root:\s*(?:Some\(PathBuf::from\()?"\//.test(source), false, `${name} hard-codes a Unix-only storage_root fixture`);
  }
  assert.equal(/"output_dir":\s*"\/tmp\//.test(workerLive), false);
  assert.equal(/"path":\s*"\/tmp\//.test(workerLive), false);
});

test("Windows worker supervisor bash fixtures use the preflighted MSYS2 interpreter and cygpath contract", () => {
  assert.match(workerSupervisorStubs, /WINDOWS_BASH:\s*&str\s*=\s*r"C:\\msys64\\usr\\bin\\bash\.exe"/);
  assert.match(workerSupervisorStubs, /WINDOWS_CYGPATH:\s*&str\s*=\s*r"C:\\msys64\\usr\\bin\\cygpath\.exe"/);
  assert.match(workerSupervisorStubs, /fn script_program_path\(path: &std::path::Path\) -> String/);
  assert.match(workerSupervisorStubs, /Command::new\(WINDOWS_CYGPATH\)[\s\S]*?\.arg\("-u"\)[\s\S]*?\.arg\(path\)/);
  assert.match(workerSupervisorStubs, /bash_command\(\)\s*\.args\(\["--noprofile", "--norc"\]\)\s*\.arg\(&self\.program\)/s);
  assert.equal(/\.arg\("-c"\)/.test(workerSupervisorStubs), false);
  assert.equal(/return\s+String::from_utf8\(converted\.stdout\)/.test(workerSupervisorStubs), false);
  assert.match(workerSupervisorStubs, /unsupported ipc protocol version: 99/);
});

test("retention pre-delete race fixture is manager-local and cannot block sibling tests", () => {
  assert.equal(/static\s+RETENTION_PRE_DELETE_GATE/.test(storageManager), false);
  assert.match(storageManager, /retention_pre_delete_gate:\s*Option<RetentionTestGate>/);
  assert.match(storageManager, /self\.retention_pre_delete_gate\.take\(\)/);
  assert.match(storageManager, /manager\.retention_pre_delete_gate\s*=\s*Some\(RetentionTestGate/);
});

test("release verifier signed artifact fixture is checkout-byte-stable on Windows", () => {
  assert.match(gitAttributes, /^tools\/nian-release-verifier\/tests\/fixtures\/artifact\.bin binary$/m);
});

test("Windows release contract text normalization is identical for LF and CRLF", () => {
  const crlf = workflow.replace(/\n/g, "\r\n");
  assert.equal(normalizeNewlines(crlf), workflow);
});

test("Windows FFmpeg build is source-pinned MSVC shared LGPL with bounded resources", () => {
  assert.equal(releaseConfig.ffmpegVersion, "8.0.3");
  assert.equal(releaseConfig.ffmpegSourceUrl, "https://ffmpeg.org/releases/ffmpeg-8.0.3.tar.xz");
  assert.equal(releaseConfig.ffmpegSourceSha256, "6136812ea6d4e68bdba27e33c2a94382711cdf4f8602ffef056ff792bd6f9818");
  assert.equal(windowsFfmpegContract.buildContractVersion, 4);
  assert.equal(windowsFfmpegContract.toolchain, "msvc");
  assert.equal(windowsFfmpegContract.architecture, "x86_64");
  for (const flag of ["--toolchain=msvc", "--enable-shared", "--disable-static", "--disable-gpl", "--disable-nonfree", "--disable-autodetect", "--disable-everything"]) {
    assert.ok(windowsFfmpegContract.configureFlags.includes(flag), `missing authoritative FFmpeg configure flag ${flag}`);
  }
  assert.ok(windowsFfmpegContract.configureFlags.includes("--enable-protocol=file,tcp,rtp,udp"));
  assert.equal(windowsFfmpegContract.configureFlags.some((flag) => flag.includes("--enable-protocol=") && flag.split("=")[1].split(",").includes("rtsp")), false);
  assert.ok(windowsFfmpegContract.configureFlags.includes("--enable-demuxer=matroska,mov,rtsp"));
  assert.match(windowsFfmpeg, /ffmpeg-windows-contract\.json/);
  assert.match(windowsFfmpeg, /Get-FileHash -Algorithm SHA256/);
  assert.match(windowsFfmpeg, /Invoke-NianNative \{ & \$Curl/);
  assert.match(windowsFfmpeg, /System32\\curl\.exe/);
  const shaAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "SHA-256 verification"');
  const extractionAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "extraction"');
  const patchAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "apply upstream CBS lavf backport"');
  const configureAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "configure"');
  assert.ok(shaAt >= 0 && shaAt < extractionAt && extractionAt < patchAt && patchAt < configureAt);
  assert.match(windowsFfmpeg, /\$Tar = ['"]C:\\msys64\\usr\\bin\\tar\.exe['"]/);
  assert.match(windowsFfmpeg, /\$Xz = ['"]C:\\msys64\\usr\\bin\\xz\.exe['"]/);
  assert.match(windowsFfmpeg, /\/usr\/bin\/xz --decompress --stdout/);
  assert.match(windowsFfmpeg, /\/usr\/bin\/tar --extract --file - --directory/);
  assert.equal(/Invoke-NianNative \{ tar\.exe/.test(windowsFfmpeg), false);
  assert.match(windowsFfmpeg, /Invoke-NianBashScript/);
  assert.match(windowsBashScript, /Invoke-NianBoundedProcess -FilePath \$Bash/);
  assert.match(windowsFfmpeg, /-TimeoutSeconds 600/);
  assert.match(windowsFfmpeg, /FFmpeg extraction tar version:/);
  assert.match(windowsFfmpeg, /FFmpeg extraction archive size:/);
  assert.match(windowsFfmpeg, /FFmpeg extraction start UTC:/);
  assert.match(windowsFfmpeg, /FFmpeg source extraction did not produce the expected source directory/);
  for (const path of ["/usr/bin/make", "/usr/bin/awk", "/usr/bin/sed", "/usr/bin/grep", "/usr/bin/cygpath", "/usr/bin/tar", "/usr/bin/xz", "/usr/bin/head", "/usr/bin/tail", "/usr/bin/tr", "/usr/bin/cut", "/usr/bin/mkdir", "/usr/bin/rm", "/usr/bin/cp", "/usr/bin/cmp", "/usr/bin/cat", "/usr/bin/sort", "/usr/bin/uniq", "/usr/bin/chmod", "/usr/bin/install"]) {
    assert.ok(JSON.stringify(windowsFfmpegContract.msysBuildTools).includes(path));
  }
  assert.match(windowsPreflight, /test-ffmpeg-msys-escape\.ps1/);
  assert.match(windowsFfmpeg, /post-configure MSYS dependency validation/);
  const globalConfigAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "post-configure global validation"');
  const componentConfigAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "post-configure component validation"');
  const cbsConfigAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "post-configure CBS lavf validation"');
  const postConfigureAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "post-configure MSYS dependency validation"');
  const cbsCompileAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "CBS lavf regression compile"');
  const fullCompileAt = windowsFfmpeg.indexOf('Invoke-FfmpegPhase "compile"');
  assert.ok(
    configureAt < globalConfigAt &&
      globalConfigAt < componentConfigAt &&
      componentConfigAt < cbsConfigAt &&
      cbsConfigAt < postConfigureAt &&
      postConfigureAt < cbsCompileAt &&
      cbsCompileAt < fullCompileAt,
  );
  assert.match(windowsFfmpeg, /validate-ffmpeg-components\.mjs/);
  assert.match(windowsFfmpeg, /\/usr\/bin\/make -j1 libavformat\/cbs\.o/);
  assert.equal(windowsFfmpeg.includes("/Od"), false);
  assert.match(windowsFfmpeg, /configure returned success but emitted sed\/awk syntax errors; compile is blocked/);
  assert.match(windowsBoundedProcess, /\.Kill\(\$true\)/);
  assert.match(windowsBoundedProcess, /timed out after \{1\}s; process-tree termination was requested/);
  assert.match(windowsPreflight, /C:\\msys64\\usr\\bin\\tar\.exe/);
  assert.match(windowsPreflight, /C:\\msys64\\usr\\bin\\xz\.exe/);
  assert.match(windowsFfmpeg, /\[Environment\]::ProcessorCount/);
  assert.match(windowsFfmpeg, /\[Math\]::Max\(2, \[Math\]::Min\(\$reportedCpuCount, 8\)\)/);
  assert.match(windowsFfmpeg, /\/usr\/bin\/make -j\$buildJobs/);
  assert.equal(windowsFfmpeg.includes("make -j2"), false);
  for (const phase of ["download", "SHA-256 verification", "extraction", "apply upstream CBS lavf backport", "configure", "post-configure global validation", "post-configure component validation", "post-configure CBS lavf validation", "post-configure MSYS dependency validation", "CBS lavf regression compile", "compile", "install", "configuration and license validation", "runtime staging and validation", "publish validated output"]) {
    assert.ok(windowsFfmpeg.includes(`Invoke-FfmpegPhase "${phase}"`), `missing FFmpeg phase timing for ${phase}`);
  }
  for (const name of ["avformat.lib", "avcodec.lib", "avutil.lib"]) assert.match(windowsFfmpeg, new RegExp(name.replace(".", "\\.")));
  assert.match(windowsFfmpegValidator, /required FFmpeg runtime DLL missing, duplicated, or misplaced/);
  assert.match(windowsFfmpegValidator, /required MSVC FFmpeg import library missing, duplicated, or misplaced/);
  assert.equal(/gyan\.dev|github\.com\/BtbN|prebuilt/i.test(windowsFfmpeg), false);
  assert.match(workflow, /Build pinned FFmpeg 8\.0\.3 Windows MSVC runtime[\s\S]*?timeout-minutes: 150[\s\S]*?build-ffmpeg-windows\.ps1/);
});

test("Windows release PowerShell scripts share the fail-closed native helper", () => {
  for (const [name, source] of [
    ["preflight", windowsPreflight],
    ["FFmpeg", windowsFfmpeg],
    ["FFmpeg MSYS provisioning", windowsFfmpegProvision],
    ["FFmpeg MSYS environment", windowsFfmpegMsysEnvironment],
    ["generated Bash execution", windowsBashScript],
    ["FFmpeg validator", windowsFfmpegValidatorPs],
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
  assert.match(windowsNative, /throw \("required native command failed with exit code \{0\}: \{1\}" -f \$exitCode, \$Label\)/);
  for (const tool of ["git.exe", "rustup.exe", "rustc.exe", "npm.cmd", "pnpm.cmd", "cargo.exe", "node.exe"]) {
    assert.ok(workflow.includes(`Invoke-NianNative { ${tool}`), `workflow does not route ${tool} through fail-closed helper`);
  }
  assert.match(workflow, /Invoke-NianNative \{ npm\.cmd install --global --prefix \$corepackRoot corepack@0\.35\.0 \}/);
  assert.match(workflow, /Invoke-NianNative \{ & \$corepack enable --install-directory \$corepackRoot \}/);
  assert.match(workflow, /Invoke-NianNative \{ & \.\.\\\.\.\\ui\\node_modules\\\.bin\\tauri\.CMD build/);
  assert.match(workflow, /Invoke-NianNative \{ cargo\.exe tauri bundle/);
  assert.match(workflow, /Invoke-NianNative \{ cargo\.exe tauri signer sign/);
  assert.match(workflow, /Invoke-NianNative \{ cargo\.exe run --quiet -p nian-release-verifier/);
  assert.match(windowsFfmpeg, /\$compileScript = [^\n]*\/usr\/bin\/make -j\$buildJobs/);
  assert.match(windowsFfmpeg, /Invoke-NianBashScript[\s\S]*?-Script \$compileScript/);
  assert.match(windowsFfmpeg, /\$installScript = [^\n]*\/usr\/bin\/make install DESTDIR=/);
  assert.match(windowsFfmpeg, /Invoke-NianBashScript[\s\S]*?-Script \$installScript/);
  assert.match(windowsBashScript, /Invoke-NianNative \{ & \$Bash --noprofile --norc \$scriptInvocationPath \}/);
  assert.match(windowsMsvcToolchain, /Invoke-NianNative \{ cmd\.exe/);
  const windowsJobs = workflow.slice(workflow.indexOf("  build-windows:"), workflow.indexOf("  verify-release:"));
  assert.equal(/(?:^|\n)\s+npm install --global corepack@0\.35\.0/.test(windowsJobs), false);
  const quality = workflow.slice(workflow.indexOf("- name: Release scripts and frontend quality"), workflow.indexOf("- name: Build pinned FFmpeg 8.0.3 Windows MSVC runtime"));
  for (const command of ["release:test", "lint", "typecheck", "test", "build"]) {
    assert.match(quality, new RegExp(`Invoke-NianNative \\{ pnpm\\.cmd ${command.replace(":", "\\:")} \\}`));
  }
});

test("Windows build provisions and preflights pinned Rust quality components before expensive work", () => {
  const windows = workflow.slice(workflow.indexOf("  build-windows:"), workflow.indexOf("  sign-windows:"));
  const signing = workflow.slice(workflow.indexOf("  sign-windows:"), workflow.indexOf("  verify-release:"));
  const rustInstallAt = windows.indexOf("- name: Install pinned Rust 1.98.0 MSVC toolchain");
  const frontendInstallAt = windows.indexOf("- name: Install frontend dependencies");
  const ffmpegBuildAt = windows.indexOf("- name: Build pinned FFmpeg 8.0.3 Windows MSVC runtime");
  const rustQualityAt = windows.indexOf("- name: Run remaining Windows Rust quality and release worker build");

  assert.ok(
    rustInstallAt >= 0 && rustInstallAt < frontendInstallAt && rustInstallAt < ffmpegBuildAt && rustInstallAt < rustQualityAt,
  );
  assert.match(
    windows,
    /Invoke-NianNative \{ rustup\.exe toolchain install 1\.98\.0-x86_64-pc-windows-msvc --profile minimal --component rustfmt --component clippy \}/,
  );
  assert.match(windows, /Invoke-NianNative \{ rustup\.exe override set 1\.98\.0 \}/);
  for (const command of [
    "rustc.exe --version",
    "cargo.exe --version",
    "cargo.exe fmt --version",
    "cargo.exe clippy --version",
  ]) {
    assert.ok(windows.includes(`Invoke-NianNative { ${command} }`), `missing Rust preflight: ${command}`);
  }
  assert.match(windows, /Invoke-NianNative \{ cargo\.exe fmt --all --check \}/);
  assert.match(windows, /Invoke-NianNative \{ cargo\.exe clippy --all-targets -- -D warnings \}/);
  assert.equal(/rustup\.exe (?:toolchain install|override set) (?:stable|latest|default)(?:\s|})/.test(windows), false);

  assert.match(
    signing,
    /Invoke-NianNative \{ rustup\.exe toolchain install 1\.98\.0-x86_64-pc-windows-msvc --profile minimal \}/,
  );
  assert.equal(signing.includes("--component rustfmt"), false);
  assert.equal(signing.includes("--component clippy"), false);
  assert.equal(signing.includes("cargo.exe fmt"), false);
  assert.equal(signing.includes("cargo.exe clippy"), false);
});

test("Windows FFmpeg cache is exact, validated, bounded, and source-build backed", () => {
  const windows = workflow.slice(workflow.indexOf("  build-windows:"), workflow.indexOf("  sign-windows:"));
  const computeAt = windows.indexOf("- name: Compute Windows FFmpeg build contract");
  const restoreAt = windows.indexOf("- name: Restore exact Windows FFmpeg build cache");
  const validateAt = windows.indexOf("- name: Validate restored Windows FFmpeg cache");
  const buildAt = windows.indexOf("- name: Build pinned FFmpeg 8.0.3 Windows MSVC runtime");
  const resolvedAt = windows.indexOf("- name: Validate resolved Windows FFmpeg runtime");
  const mediaIntegrationAt = windows.indexOf("- name: Validate Windows FFmpeg media integration");
  const saveAt = windows.indexOf("- name: Save validated Windows FFmpeg build cache");
  const artifactUploadAt = windows.indexOf("- name: Upload validated Windows FFmpeg cross-tag cache artifact");
  const rustAt = windows.indexOf("- name: Run remaining Windows Rust quality and release worker build");
  assert.ok(computeAt >= 0 && computeAt < restoreAt && restoreAt < validateAt && validateAt < buildAt && buildAt < resolvedAt && resolvedAt < mediaIntegrationAt && mediaIntegrationAt < saveAt && saveAt < artifactUploadAt && artifactUploadAt < rustAt);

  assert.match(windows, /actions\/cache\/restore@0057852bfaa89a56745cba8c7296529d2fc39830 # v4\.3\.0/);
  assert.match(windows, /actions\/cache\/save@0057852bfaa89a56745cba8c7296529d2fc39830 # v4\.3\.0/);
  assert.match(windows, /path: dist\/ffmpeg-windows-x86_64/);
  assert.match(windows, /key: \${{ steps\.ffmpeg-contract\.outputs\.key }}/);
  assert.equal(/restore-keys:/.test(windows), false);
  assert.match(windows, /Invoke-NianNative \{ node\.exe scripts\/release\/ffmpeg-cache-key\.mjs \}/);
  assert.match(windows, /FFmpeg cache key:/);
  assert.match(windows, /FFmpeg cache hit:/);
  assert.match(windows, /FFmpeg source build required:/);
  assert.match(windows, /validate-ffmpeg-windows\.ps1/);
  assert.match(windows, /restore-ffmpeg-windows-artifact-cache\.ps1/);
  assert.match(windows, /restored FFmpeg cache failed release-contract validation and will be discarded/);
  assert.match(windows, /Remove-Item -Recurse -Force .*ffmpeg-windows-x86_64/);
  assert.match(windows, /if: steps\.ffmpeg-cache-state\.outputs\.source-build-required == 'true'/);
  assert.match(windows, /Build pinned FFmpeg 8\.0\.3 Windows MSVC runtime[\s\S]*?timeout-minutes: 150/);
  const mediaIntegration = windows.slice(mediaIntegrationAt, saveAt);
  assert.match(mediaIntegration, /\$env:NIAN_FFMPEG_LIB_DIR = Join-Path \$pwd 'dist\\ffmpeg-windows-x86_64\\lib'/);
  assert.match(mediaIntegration, /\$env:PATH = "\$\(Join-Path \$pwd 'dist\\ffmpeg-windows-x86_64\\bin'\);\$env:PATH"/);
  assert.match(mediaIntegration, /Invoke-NianNative \{ cargo\.exe test -p nian-media-ffmpeg --test media_integration \}/);
  const remainingRust = windows.slice(rustAt);
  assert.equal(remainingRust.includes("cargo.exe test -p nian-media-ffmpeg --test media_integration"), false);
  assert.match(remainingRust, /Invoke-NianNative \{ cargo\.exe fmt --all --check \}/);
  assert.match(remainingRust, /Invoke-NianNative \{ cargo\.exe build -p nian-media-worker --target x86_64-pc-windows-msvc --release \}/);
  assert.match(windows, /^    timeout-minutes: 240$/m);
  assert.match(windows, /permissions:\n      contents: read\n      actions: read/);
  assert.equal(windows.includes("actions: write"), false);
  assert.match(windows, /name: windows-ffmpeg-cache-\${{ steps\.ffmpeg-contract\.outputs\.digest }}/);
  assert.match(windows, /retention-days: 90/);
  assert.equal(windows.includes("secrets.TAURI_SIGNING_PRIVATE_KEY"), false);
  assert.equal(windows.includes("WINDOWS_SIGNING_PFX"), false);

  assert.match(windowsFfmpegArtifactCache, /actions\/workflows\/release\.yml/);
  assert.match(windowsFfmpegArtifactCache, /run\.workflow_id -ne \$workflow\.id/);
  assert.match(windowsFfmpegArtifactCache, /run\.event -ne "push" -or \$run\.conclusion -ne "success"/);
  assert.match(windowsFfmpegArtifactCache, /run\.head_repository\.full_name -ne \$repo/);
  assert.match(windowsFfmpegArtifactCache, /validate-ffmpeg-windows\.ps1/);
  assert.match(windowsFfmpegArtifactCache, /failed validation and was discarded/);
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
