import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const urls = {
  build: new URL("./build-ffmpeg-windows.ps1", import.meta.url),
  preflight: new URL("./preflight-windows.ps1", import.meta.url),
  probe: new URL("./test-ffmpeg-msys-escape.ps1", import.meta.url),
  validator: new URL("./validate-ffmpeg-msys-dependency.ps1", import.meta.url),
  msvcHelper: new URL("./windows-msvc-toolchain.ps1", import.meta.url),
  msvcFixture: new URL("./test-windows-msvc-toolchain-path.ps1", import.meta.url),
  envHelper: new URL("./windows-ffmpeg-msys-environment.ps1", import.meta.url),
  envFixture: new URL("./test-windows-ffmpeg-msys-path.ps1", import.meta.url),
  provision: new URL("./provision-ffmpeg-msys-tools.ps1", import.meta.url),
  bashHelper: new URL("./windows-bash-script.ps1", import.meta.url),
  bashFixture: new URL("./test-windows-bash-script.ps1", import.meta.url),
  configureDiagnosticsHelper: new URL("./windows-configure-diagnostics.ps1", import.meta.url),
  configureDiagnosticsFixture: new URL("./test-windows-configure-diagnostics.ps1", import.meta.url),
};
const paths = Object.fromEntries(Object.entries(urls).map(([name, url]) => [name, fileURLToPath(url)]));
const source = Object.fromEntries(Object.entries(urls).map(([name, url]) => [name, readNormalizedText(url)]));
const contract = JSON.parse(readNormalizedText(new URL("./ffmpeg-windows-contract.json", import.meta.url)));

const expectedTools = {
  bash: "/usr/bin/bash",
  make: "/usr/bin/make",
  awk: "/usr/bin/awk",
  sed: "/usr/bin/sed",
  grep: "/usr/bin/grep",
  cygpath: "/usr/bin/cygpath",
  tar: "/usr/bin/tar",
  xz: "/usr/bin/xz",
  head: "/usr/bin/head",
  tail: "/usr/bin/tail",
  tr: "/usr/bin/tr",
  cut: "/usr/bin/cut",
  mkdir: "/usr/bin/mkdir",
  rm: "/usr/bin/rm",
  cp: "/usr/bin/cp",
  cmp: "/usr/bin/cmp",
  cat: "/usr/bin/cat",
  sort: "/usr/bin/sort",
  uniq: "/usr/bin/uniq",
  expr: "/usr/bin/expr",
  dirname: "/usr/bin/dirname",
  basename: "/usr/bin/basename",
  uname: "/usr/bin/uname",
  mktemp: "/usr/bin/mktemp",
  touch: "/usr/bin/touch",
  chmod: "/usr/bin/chmod",
  ln: "/usr/bin/ln",
  install: "/usr/bin/install",
};

function findPowerShell() {
  const candidates = process.platform === "win32" ? ["pwsh.exe", "pwsh"] : ["pwsh"];
  return candidates.find((candidate) => {
    const result = spawnSync(candidate, ["-NoProfile", "-NonInteractive", "-Command", "$PSVersionTable.PSVersion.ToString()"], { encoding: "utf8" });
    return !result.error && result.status === 0;
  });
}

function assertPowerShellParses(executable, path) {
  const quotedPath = path.replaceAll("'", "''");
  const command = [
    "$tokens = $null",
    "$errors = $null",
    `[System.Management.Automation.Language.Parser]::ParseFile('${quotedPath}', [ref]$tokens, [ref]$errors) | Out-Null`,
    "if ($errors.Count -ne 0) { $errors | ForEach-Object { [Console]::Error.WriteLine($_.Message) }; exit 1 }",
  ].join("; ");
  const result = spawnSync(executable, ["-NoProfile", "-NonInteractive", "-Command", command], { encoding: "utf8" });
  assert.equal(result.status, 0, `${path}\n${result.stdout}\n${result.stderr}`);
}

function quotePs(value) {
  return `'${value.replaceAll("'", "''")}'`;
}

test("RC10 Windows FFmpeg uses one controlled MSYS environment and aggregate diagnostics", () => {
  assert.equal(contract.buildContractVersion, 4);
  assert.deepEqual(contract.msysBuildTools, expectedTools);
  for (const name of ["build", "preflight", "probe", "validator"]) {
    assert.match(source[name], /windows-ffmpeg-msys-environment\.ps1/);
  }
  for (const name of ["build", "preflight", "probe"]) {
    assert.match(source[name], /New-NianFfmpegMsysEnvironment/);
  }
  assert.match(source.envHelper, /Get-NianForbiddenMsysToolRoots/);
  for (const forbidden of ["/mingw64/bin", "/mingw32/bin", "/ucrt64/bin", "/clang64/bin", "/clangarm64/bin", "/c/mingw64/bin", "/c/mingw32/bin"]) {
    assert.ok(source.envHelper.includes(`'${forbidden}'`));
  }
  assert.match(source.envHelper, /foreach \(\$entry in @\(\$msvc, '\/usr\/bin'\)\)/);
  assert.match(source.envHelper, /PathText = \(@\(\$entries\) -join ':'\)/);
  assert.equal(source.envHelper.includes('"$PATH"'), false);
  assert.match(source.probe, /mismatch_count=0/);
  assert.match(source.probe, /failures=\(\)/);
  assert.match(source.probe, /report_msvc cl\.exe __CL__/);
  assert.match(source.probe, /report_msvc cl __CL__/);
  assert.match(source.probe, /report_msvc link\.exe __LINK__/);
  assert.match(source.probe, /report_msvc link __LINK__/);
  assert.match(source.probe, /report_msvc dumpbin\.exe __DUMPBIN__/);
  assert.match(source.probe, /report_windows_sdk rc\.exe __RC__/);
  assert.match(source.probe, /for name in gcc cc ld ar/);
  assert.match(source.probe, /command -v "\$logical"/);
  assert.match(source.probe, /\/usr\/bin\/cygpath -aw/);
  assert.match(source.probe, /FFmpeg Windows toolchain resolution failures:/);
  assert.ok(source.probe.indexOf("while IFS=' ' read -r logical expected") < source.probe.indexOf("if (( mismatch_count > 0 ))"));
});

test("RC10 FFmpeg compat/windows/makedef required pipeline is locked to sort + uniq + tail", () => {
  assert.deepEqual(
    ["sort", "uniq", "tail"].map((name) => contract.msysBuildTools[name]),
    ["/usr/bin/sort", "/usr/bin/uniq", "/usr/bin/tail"],
  );
  assert.equal(expectedTools.uniq, "/usr/bin/uniq");
  assert.match(source.probe, /while IFS=' ' read -r logical expected/);
  assert.match(source.probe, /\/usr\/bin\/sort \| \/usr\/bin\/uniq/);
});

test("RC10 preserves semantic MSVC authority and explicit GNU make execution", () => {
  assert.match(source.msvcHelper, /function Assert-NianMsvcToolchainPath/);
  assert.match(source.msvcHelper, /function Assert-NianMsvcToolAuthority/);
  assert.match(source.msvcHelper, /function Resolve-NianMsvcToolchain/);
  assert.match(source.msvcHelper, /function Assert-NianFfmpegWindowsToolAuthority/);
  assert.match(source.msvcHelper, /function Assert-NianWindowsSdkRcAuthority/);
  assert.match(source.msvcHelper, /function Get-NianSelectedWindowsSdkX64Bin/);
  assert.match(source.msvcHelper, /Get-Command \$name -CommandType Application/);
  assert.match(source.msvcHelper, /StringComparer\]::OrdinalIgnoreCase/);
  assert.match(source.msvcHelper, /Equals\(\$segments\[2\], 'HostX64'\)/);
  assert.match(source.msvcHelper, /Equals\(\$segments\[3\], 'x64'\)/);
  assert.match(source.msvcHelper, /dumpbin\.exe must resolve from the same validated MSVC directory/);
  assert.match(source.msvcHelper, /rc\.exe must resolve from the selected Windows SDK x64 bin directory/);
  assert.equal(source.msvcHelper.includes("-notmatch"), false);
  assert.ok(source.msvcFixture.includes("C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise"));
  assert.ok(source.msvcFixture.includes("HostX64\\x64"));

  const configureAt = source.build.indexOf('Invoke-FfmpegPhase "configure"');
  const cbsValidateAt = source.build.indexOf('Invoke-FfmpegPhase "post-configure CBS lavf validation"');
  const validateAt = source.build.indexOf('Invoke-FfmpegPhase "post-configure MSYS dependency validation"');
  const cbsCompileAt = source.build.indexOf('Invoke-FfmpegPhase "CBS lavf regression compile"');
  const compileAt = source.build.indexOf('Invoke-FfmpegPhase "compile"');
  assert.ok(
    configureAt >= 0 &&
      configureAt < cbsValidateAt &&
      cbsValidateAt < validateAt &&
      validateAt < cbsCompileAt &&
      cbsCompileAt < compileAt,
  );
  assert.match(source.build, /\/usr\/bin\/make -j1 libavformat\/cbs\.o/);
  assert.match(source.build, /FFmpeg CBS lavf regression compile: PASS/);
  assert.match(source.build, /\/usr\/bin\/make -j\$buildJobs/);
  assert.equal(source.build.includes("/Od"), false);
  assert.equal(source.build.includes("clang-cl"), false);
  assert.match(source.build, /\/usr\/bin\/make install DESTDIR=/);
  assert.doesNotMatch(source.build, /(?:^|[;\s])make -j\$buildJobs/);
  assert.match(source.build, /-ControlledPath \$MsysEnvironment\.PathText/);
  assert.match(source.preflight, /-ControlledPath \$msysEnvironment\.PathText/);
  for (const [name, prefix] of [["build", "Msvc"], ["preflight", "msvc"]]) {
    assert.ok(source[name].includes(`-ExpectedDumpbinWindows $${prefix}.DumpbinPath`));
    assert.ok(source[name].includes(`-ExpectedWindowsSdkBin $${prefix}.WindowsSdkBin`));
    assert.ok(source[name].includes(`-ExpectedRcWindows $${prefix}.RcPath`));
  }
  assert.match(source.msvcHelper, /foreach \(\$name in @\('cl\.exe', 'lib\.exe', 'link\.exe', 'dumpbin\.exe', 'rc\.exe'\)\)/);
  assert.match(source.msvcHelper, /WindowsSdkDir/);
  assert.match(source.msvcHelper, /WindowsSDKVersion/);
  assert.equal(source.msvcHelper.includes('10.0.26100.0'), false);
});

test("RC10 GNU make/AWK and CCDEP contracts fail closed before compile", () => {
  assert.match(source.probe, /\/usr\/bin\/make --version/);
  assert.ok(source.probe.includes(`gsub(/\\\\/, "/")`));
  assert.match(source.probe, /\.RECIPEPREFIX := >/);
  assert.match(source.probe, /\/usr\/bin\/make --no-print-directory -f/);
  assert.match(source.probe, /GNU make\/AWK recipe expansion produced unexpected output/);
  assert.match(source.probe, /\/usr\/bin\/cmp -s "\$cmp_left" "\$cmp_right"/);
  assert.match(source.probe, /cmp_different_status != 1/);
  assert.match(source.probe, /\/usr\/bin\/install -m 644 "\$install_source" "\$install_destination"/);
  assert.match(source.probe, /install behavioral probe copied unexpected contents/);
  assert.match(source.probe, /printf '%s\\n' a a b \| \/usr\/bin\/sort \| \/usr\/bin\/uniq/);
  assert.match(source.probe, /FFmpeg MSYS uniq behavioral probe:/);
  assert.match(source.validator, /ffbuild\/config\.mak/);
  assert.ok(source.validator.includes(`gsub(/\\\\/, "/")`));
  assert.match(source.validator, /requires exactly one CCDEP line/);
  assert.match(source.validator, /\/usr\/bin\/make --no-print-directory/);
  assert.match(source.validator, /corrupted during GNU make expansion/);
  assert.match(source.validator, /forbidden toolchain authority/);
  assert.match(source.build, /configure returned success but emitted sed\/awk syntax errors; compile is blocked/);
});

test("RC10 Windows FFmpeg PowerShell sources parse when PowerShell is available", (t) => {
  const executable = findPowerShell();
  if (!executable) {
    t.skip("PowerShell is unavailable on this host; Windows release CI parses the RC10 FFmpeg scripts");
    return;
  }
  for (const path of Object.values(paths)) assertPowerShellParses(executable, path);
});

test("RC10 pure MSVC and controlled-PATH fixtures execute when PowerShell is available", (t) => {
  const executable = findPowerShell();
  if (!executable) {
    t.skip("PowerShell is unavailable on this host; Windows release CI executes the pure path fixtures");
    return;
  }
  const msvc = spawnSync(executable, ["-NoProfile", "-NonInteractive", "-File", paths.msvcFixture], { encoding: "utf8", timeout: 30000 });
  assert.equal(msvc.status, 0, `${msvc.stdout}\n${msvc.stderr}`);
  assert.match(msvc.stdout, /MSVC path fixture PASS: exact RC8 hosted-runner path/);
  assert.match(msvc.stdout, /MSVC path fixture PASS: Community edition selected root/);
  assert.match(msvc.stdout, /rejected split cl\/lib\/link directories as expected/);
  assert.match(msvc.stdout, /dumpbin shares MSVC bin and rc uses selected Windows SDK x64 bin/);
  assert.match(msvc.stdout, /rejected split dumpbin directory as expected/);
  assert.match(msvc.stdout, /rejected foreign rc\.exe as expected/);

  const env = spawnSync(executable, ["-NoProfile", "-NonInteractive", "-File", paths.envFixture], { encoding: "utf8", timeout: 30000 });
  assert.equal(env.status, 0, `${env.stdout}\n${env.stderr}`);
  assert.match(env.stdout, /selected MSVC first, \/usr\/bin second, forbidden toolchain roots removed/);
});

test("Windows FFmpeg aggregate tool, AWK, make, and synthetic CCDEP probes execute on Windows", (t) => {
  if (process.platform !== "win32") {
    t.skip("MSYS2/MSVC behavioral probes require the Windows release host");
    return;
  }
  const shell = findPowerShell();
  assert.ok(shell, "PowerShell is required on the Windows release host");
  const probeResult = spawnSync(shell, ["-NoProfile", "-NonInteractive", "-File", paths.probe], { encoding: "utf8", timeout: 60000 });
  assert.equal(probeResult.status, 0, `${probeResult.stdout}\n${probeResult.stderr}`);
  for (const command of ["cl.exe", "cl", "lib.exe", "link.exe", "link", "dumpbin.exe", "rc.exe"]) assert.match(probeResult.stdout, new RegExp(`${command.replace(".", "\\.")}\\s+->`));
  for (const path of Object.values(expectedTools)) assert.ok(probeResult.stdout.includes(path), `aggregate report omitted ${path}`);
  assert.match(probeResult.stdout, /FFmpeg MSYS AWK backslash probe: C:\/foo\/bar\.h/);
  assert.match(probeResult.stdout, /FFmpeg MSYS cmp behavioral probe: identical=0 different=1/);
  assert.match(probeResult.stdout, /FFmpeg MSYS install behavioral probe: nian-install-probe/);
  assert.match(probeResult.stdout, /FFmpeg MSYS uniq behavioral probe: a,b/);
  assert.match(probeResult.stdout, /FFmpeg MSYS GNU make behavioral probe: C:\/foo\/bar\.h/);
  assert.match(probeResult.stdout, /FFmpeg Windows toolchain resolution: PASS/);

  const root = mkdtempSync(join(tmpdir(), "nian-ffmpeg-msys-dep-"));
  try {
    const ffbuild = join(root, "ffbuild");
    mkdirSync(ffbuild, { recursive: true });
    const configMak = join(ffbuild, "config.mak");
    const validCcdep = String.raw`CCDEP=printf '%s\n' 'C:\foo\bar.h' | awk '/including/ { gsub(/\\/, "/"); print }'`;
    writeFileSync(configMak, `${validCcdep}\n`, "utf8");
    const command = [
      `. ${quotePs(paths.msvcHelper)}`,
      `. ${quotePs(paths.envHelper)}`,
      "$m = Resolve-NianMsvcToolchain",
      "$e = New-NianFfmpegMsysEnvironment -MsvcBinWindows $m.MsvcBin",
      `& ${quotePs(paths.validator)} -Source ${quotePs(root)} -Bash 'C:\\msys64\\usr\\bin\\bash.exe' -ControlledPath $e.PathText`,
    ].join("; ");
    const env = { ...process.env, RUNNER_TEMP: process.env.RUNNER_TEMP || root };
    const valid = spawnSync(shell, ["-NoProfile", "-NonInteractive", "-Command", command], { encoding: "utf8", timeout: 60000, env });
    assert.equal(valid.status, 0, `${valid.stdout}\n${valid.stderr}`);
    assert.match(valid.stdout, /sane on disk and after \/usr\/bin\/make expansion under the controlled PATH/);

    writeFileSync(configMak, `${validCcdep.replace(String.raw`gsub(/\\/, "/")`, String.raw`gsub(/\/, "/")`)}\n`, "utf8");
    const malformed = spawnSync(shell, ["-NoProfile", "-NonInteractive", "-Command", command], { encoding: "utf8", timeout: 60000, env });
    assert.notEqual(malformed.status, 0);
    assert.match(`${malformed.stdout}\n${malformed.stderr}`, /malformed on disk/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
