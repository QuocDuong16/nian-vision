import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const buildUrl = new URL("./build-ffmpeg-windows.ps1", import.meta.url);
const buildPath = fileURLToPath(buildUrl);
const build = readNormalizedText(buildUrl);
const preflight = readNormalizedText(new URL("./preflight-windows.ps1", import.meta.url));
const probeUrl = new URL("./test-ffmpeg-msys-escape.ps1", import.meta.url);
const probePath = fileURLToPath(probeUrl);
const probe = readNormalizedText(probeUrl);
const validatorUrl = new URL("./validate-ffmpeg-msys-dependency.ps1", import.meta.url);
const validatorPath = fileURLToPath(validatorUrl);
const validator = readNormalizedText(validatorUrl);
const contract = JSON.parse(readNormalizedText(new URL("./ffmpeg-windows-contract.json", import.meta.url)));

const expectedTools = {
  bash: "/usr/bin/bash",
  make: "/usr/bin/make",
  awk: "/usr/bin/awk",
  sed: "/usr/bin/sed",
  grep: "/usr/bin/grep",
  cygpath: "/usr/bin/cygpath",
};

function findPowerShell() {
  const candidates = process.platform === "win32" ? ["pwsh.exe", "pwsh"] : ["pwsh"];
  return candidates.find((candidate) => {
    const result = spawnSync(candidate, ["-NoProfile", "-NonInteractive", "-Command", "$PSVersionTable.PSVersion.ToString()"], {
      encoding: "utf8",
    });
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

test("Windows FFmpeg MSYS build tools and dependency command are deterministic before compile", () => {
  assert.equal(contract.buildContractVersion, 2);
  assert.deepEqual(contract.msysBuildTools, expectedTools);
  assert.match(probe, /C:\\msys64\\usr\\bin\\bash\.exe/);
  for (const [name, path] of Object.entries(expectedTools).filter(([name]) => name !== "bash")) {
    assert.ok(probe.includes(`${name}:${path}`), `MSYS probe does not pin ${name} to ${path}`);
  }
  assert.match(probe, /export PATH=__MSVC_BIN__:\/usr\/bin:"\$PATH"/);
  assert.match(probe, /assert_msvc_command cl\.exe __CL__/);
  assert.match(probe, /assert_msvc_command cl __CL__/);
  assert.match(probe, /assert_msvc_command lib\.exe __LIB__/);
  assert.match(probe, /assert_msvc_command link\.exe __LINK__/);
  assert.match(probe, /assert_msvc_command link __LINK__/);
  assert.match(probe, /\/usr\/bin\/cygpath -aw "\$resolved"/);
  assert.match(probe, /\/usr\/bin\/make --version/);
  assert.match(probe, /\/usr\/bin\/awk --version/);
  assert.match(probe, /\/usr\/bin\/awk -W version/);
  assert.match(probe, /\/usr\/bin\/sed --version/);
  assert.match(probe, /\/usr\/bin\/grep --version/);
  assert.ok(probe.includes(`gsub(/\\\\/, "/")`));
  assert.ok(probe.includes("C:/foo/bar.h"));
  assert.match(preflight, /test-ffmpeg-msys-escape\.ps1/);

  assert.match(build, /\$VsInstall = Import-VsDevEnvironment/);
  assert.match(build, /VC\\Tools\\MSVC/);
  assert.match(build, /Hostx64/);
  assert.match(build, /\$MsvcBinUnix = Convert-ToMsysPath \$MsvcBin \$Bash/);
  assert.match(build, /export PATH=__MSVC_BIN__:\/usr\/bin:"\$PATH"/);
  assert.equal(build.includes('export PATH="/usr/bin:$PATH"'), false);
  assert.match(build, /compat\/windows\/mslink/);
  assert.match(build, /ExpectedClWindows \$MsvcTools\["cl\.exe"\]/);
  assert.match(build, /ExpectedLibWindows \$MsvcTools\["lib\.exe"\]/);
  assert.match(build, /ExpectedLinkWindows \$MsvcTools\["link\.exe"\]/);

  const configureAt = build.indexOf('Invoke-FfmpegPhase "configure"');
  const validateAt = build.indexOf('Invoke-FfmpegPhase "post-configure MSYS dependency validation"');
  const compileAt = build.indexOf('Invoke-FfmpegPhase "compile"');
  assert.ok(configureAt >= 0 && configureAt < validateAt && validateAt < compileAt);
  assert.match(build, /\/usr\/bin\/make -j\$buildJobs/);
  assert.match(build, /\/usr\/bin\/make install DESTDIR=/);
  assert.equal(/(?:^|[;\s])make -j\$buildJobs/.test(build), false);
  assert.match(build, /configure returned success but emitted sed\/awk syntax errors; compile is blocked/);
  assert.match(build, /validate-ffmpeg-msys-dependency\.ps1/);
  assert.match(build, /-MsvcBinUnix \$MsvcBinUnix/);

  assert.match(validator, /ffbuild\/config\.mak/);
  assert.ok(validator.includes(`gsub(/\\\\/, "/")`));
  assert.match(validator, /\/usr\/bin\/make --no-print-directory/);
  assert.match(validator, /malformed on disk/);
  assert.match(validator, /corrupted during GNU make expansion/);
  assert.match(validator, /\$command = @'/);
  assert.match(validator, /export PATH=__MSVC_BIN__:\/usr\/bin:"\$PATH"/);
  assert.equal(validator.includes('export PATH=\\"/usr/bin:'), false);
  assert.equal(validator.includes("\\t@:"), false);

  for (const source of [build, preflight, probe, validator]) {
    assert.equal(source.includes("MSYS2_ARG_CONV_EXCL"), false);
    assert.equal(source.includes("MSYS_NO_PATHCONV"), false);
  }
});

test("RC8 Windows FFmpeg PowerShell sources parse when PowerShell is available", (t) => {
  const executable = findPowerShell();
  if (!executable) {
    t.skip("PowerShell is unavailable on this host; Windows release CI parses the RC8 FFmpeg scripts");
    return;
  }
  for (const path of [probePath, validatorPath, buildPath]) {
    assertPowerShellParses(executable, path);
  }
});

test("Windows FFmpeg MSYS AWK, tool-precedence, and generated dependency probes execute on Windows", (t) => {
  if (process.platform !== "win32") {
    t.skip("MSYS2/MSVC behavioral probes are validated on the Windows release host");
    return;
  }

  const shell = findPowerShell();
  assert.ok(shell, "PowerShell is required on the Windows release host");
  const probeResult = spawnSync(shell, ["-NoProfile", "-NonInteractive", "-File", probePath], {
    encoding: "utf8",
    timeout: 30000,
  });
  assert.equal(probeResult.error, undefined, probeResult.error?.message);
  assert.equal(probeResult.status, 0, `${probeResult.stdout}\n${probeResult.stderr}`);
  for (const command of ["cl.exe", "cl", "lib.exe", "link.exe", "link"]) {
    assert.match(probeResult.stdout, new RegExp(`FFmpeg MSVC command ${command.replace(".", "\\.")}:`));
  }
  for (const [name, path] of Object.entries(expectedTools).filter(([name]) => name !== "bash")) {
    assert.ok(probeResult.stdout.includes(`FFmpeg MSYS tool ${name}: ${path}`));
  }
  assert.match(probeResult.stdout, /FFmpeg MSYS AWK backslash probe: C:\/foo\/bar\.h/);

  const root = mkdtempSync(join(tmpdir(), "nian-ffmpeg-msys-dep-"));
  try {
    const ffbuild = join(root, "ffbuild");
    mkdirSync(ffbuild, { recursive: true });
    const configMak = join(ffbuild, "config.mak");
    const validCcdep = String.raw`CCDEP=printf '%s\n' 'C:\foo\bar.h' | awk '/including/ { gsub(/\\/, "/"); print }'`;
    writeFileSync(configMak, `${validCcdep}\n`, "utf8");

    const env = { ...process.env, RUNNER_TEMP: process.env.RUNNER_TEMP || root };
    const validateResult = spawnSync(
      shell,
      [
        "-NoProfile",
        "-NonInteractive",
        "-File",
        validatorPath,
        "-Source",
        root,
        "-Bash",
        "C:\\msys64\\usr\\bin\\bash.exe",
        "-MsvcBinUnix",
        "/usr/bin",
      ],
      { encoding: "utf8", timeout: 30000, env },
    );
    assert.equal(validateResult.error, undefined, validateResult.error?.message);
    assert.equal(validateResult.status, 0, `${validateResult.stdout}\n${validateResult.stderr}`);
    assert.match(validateResult.stdout, /sane on disk and after \/usr\/bin\/make expansion/);

    writeFileSync(configMak, `${validCcdep.replace(String.raw`gsub(/\\/, "/")`, String.raw`gsub(/\/, "/")`)}\n`, "utf8");
    const malformedResult = spawnSync(
      shell,
      [
        "-NoProfile",
        "-NonInteractive",
        "-File",
        validatorPath,
        "-Source",
        root,
        "-Bash",
        "C:\\msys64\\usr\\bin\\bash.exe",
        "-MsvcBinUnix",
        "/usr/bin",
      ],
      { encoding: "utf8", timeout: 30000, env },
    );
    assert.equal(malformedResult.error, undefined, malformedResult.error?.message);
    assert.notEqual(malformedResult.status, 0);
    assert.match(`${malformedResult.stdout}\n${malformedResult.stderr}`, /malformed on disk/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
