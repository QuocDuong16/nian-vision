import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const build = readNormalizedText(new URL("./build-ffmpeg-windows.ps1", import.meta.url));
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

test("Windows FFmpeg MSYS build tools and dependency command are deterministic before compile", () => {
  assert.equal(contract.buildContractVersion, 2);
  assert.deepEqual(contract.msysBuildTools, expectedTools);
  assert.match(probe, /C:\\msys64\\usr\\bin\\bash\.exe/);
  for (const [name, path] of Object.entries(expectedTools).filter(([name]) => name !== "bash")) {
    assert.ok(probe.includes(`${name}:${path}`), `MSYS probe does not pin ${name} to ${path}`);
  }
  assert.match(probe, /\/usr\/bin\/make --version/);
  assert.match(probe, /\/usr\/bin\/awk --version/);
  assert.match(probe, /\/usr\/bin\/awk -W version/);
  assert.match(probe, /\/usr\/bin\/sed --version/);
  assert.match(probe, /\/usr\/bin\/grep --version/);
  assert.ok(probe.includes(`gsub(/\\\\/, "/")`));
  assert.ok(probe.includes("C:/foo/bar.h"));
  assert.match(preflight, /test-ffmpeg-msys-escape\.ps1/);

  const configureAt = build.indexOf('Invoke-FfmpegPhase "configure"');
  const validateAt = build.indexOf('Invoke-FfmpegPhase "post-configure MSYS dependency validation"');
  const compileAt = build.indexOf('Invoke-FfmpegPhase "compile"');
  assert.ok(configureAt >= 0 && configureAt < validateAt && validateAt < compileAt);
  assert.match(build, /export PATH="\/usr\/bin:\$PATH"/);
  assert.match(build, /\/usr\/bin\/make -j\$buildJobs/);
  assert.match(build, /\/usr\/bin\/make install DESTDIR=/);
  assert.equal(/(?:^|[;\s])make -j\$buildJobs/.test(build), false);
  assert.match(build, /configure returned success but emitted sed\/awk syntax errors; compile is blocked/);
  assert.match(build, /validate-ffmpeg-msys-dependency\.ps1/);
  assert.match(validator, /ffbuild\/config\.mak/);
  assert.ok(validator.includes(`gsub(/\\\\/, "/")`));
  assert.match(validator, /\/usr\/bin\/make --no-print-directory/);
  assert.match(validator, /malformed on disk/);
  assert.match(validator, /corrupted during GNU make expansion/);

  for (const source of [build, preflight, probe, validator]) {
    assert.equal(source.includes("MSYS2_ARG_CONV_EXCL"), false);
    assert.equal(source.includes("MSYS_NO_PATHCONV"), false);
  }
});

test("Windows FFmpeg MSYS AWK and generated dependency escaping probes execute on Windows", (t) => {
  if (process.platform !== "win32") {
    t.skip("MSYS2 AWK escaping is validated on the Windows release host");
    return;
  }

  const shell = "pwsh.exe";
  const probeResult = spawnSync(shell, ["-NoProfile", "-NonInteractive", "-File", probePath], {
    encoding: "utf8",
    timeout: 30000,
  });
  assert.equal(probeResult.error, undefined, probeResult.error?.message);
  assert.equal(probeResult.status, 0, `${probeResult.stdout}\n${probeResult.stderr}`);
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
      ["-NoProfile", "-NonInteractive", "-File", validatorPath, "-Source", root, "-Bash", "C:\\msys64\\usr\\bin\\bash.exe"],
      { encoding: "utf8", timeout: 30000, env },
    );
    assert.equal(validateResult.error, undefined, validateResult.error?.message);
    assert.equal(validateResult.status, 0, `${validateResult.stdout}\n${validateResult.stderr}`);
    assert.match(validateResult.stdout, /sane on disk and after \/usr\/bin\/make expansion/);

    writeFileSync(configMak, `${validCcdep.replace(String.raw`gsub(/\\/, "/")`, String.raw`gsub(/\/, "/")`)}\n`, "utf8");
    const malformedResult = spawnSync(
      shell,
      ["-NoProfile", "-NonInteractive", "-File", validatorPath, "-Source", root, "-Bash", "C:\\msys64\\usr\\bin\\bash.exe"],
      { encoding: "utf8", timeout: 30000, env },
    );
    assert.equal(malformedResult.error, undefined, malformedResult.error?.message);
    assert.notEqual(malformedResult.status, 0);
    assert.match(`${malformedResult.stdout}\n${malformedResult.stderr}`, /malformed on disk/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
