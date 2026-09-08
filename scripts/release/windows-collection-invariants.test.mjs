import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const buildUrl = new URL("./build-ffmpeg-windows.ps1", import.meta.url);
const helperUrl = new URL("./windows-configure-diagnostics.ps1", import.meta.url);
const fixtureUrl = new URL("./test-windows-configure-diagnostics.ps1", import.meta.url);
const build = readNormalizedText(buildUrl);
const helper = readNormalizedText(helperUrl);
const fixturePath = fileURLToPath(fixtureUrl);

function findPowerShell() {
  const candidates = process.platform === "win32" ? ["pwsh.exe", "pwsh"] : ["pwsh"];
  return candidates.find((candidate) => {
    const result = spawnSync(candidate, ["-NoProfile", "-NonInteractive", "-Command", "$PSVersionTable.PSVersion.ToString()"], { encoding: "utf8" });
    return !result.error && result.status === 0;
  });
}

test("RC12 configure diagnostics are normalized at the assignment boundary", () => {
  assert.match(build, /windows-configure-diagnostics\.ps1/);
  assert.match(
    build,
    /\[string\[\]\]\$configureDiagnostics\s*=\s*@\(\s*Get-NianConfigureDiagnostics -LiteralPath \$ConfigureStderr\s*\)/,
  );
  assert.match(
    build,
    /\[string\[\]\]\$syntaxFailures\s*=\s*@\(\s*Get-NianConfigureSyntaxFailures -Diagnostics \$configureDiagnostics\s*\)/,
  );
  assert.match(helper, /Test-Path -LiteralPath \$LiteralPath -PathType Leaf/);
  assert.match(helper, /Get-Content -LiteralPath \$LiteralPath/);
  assert.match(helper, /(?:sed\|awk).*unterminated.*syntax error.*expression #\[0-9\]\+/);
  assert.match(build, /FFmpeg configure stderr \(first 40 lines\):/);
  assert.match(build, /Select-Object -First 40/);
  assert.match(build, /configure returned success but emitted sed\/awk syntax errors; compile is blocked/);
});

test("release PowerShell rejects the RC11 conditional inner-array anti-pattern", () => {
  const releaseDir = dirname(fileURLToPath(import.meta.url));
  const antiPattern = /\$[A-Za-z_][A-Za-z0-9_]*\s*=\s*if\s*\([^)]*\)\s*\{\s*@\(\s*Get-/s;
  for (const name of readdirSync(releaseDir).filter((entry) => entry.endsWith(".ps1"))) {
    const source = readNormalizedText(new URL(`./${name}`, import.meta.url));
    assert.equal(antiPattern.test(source), false, `${name} reintroduced statement-output scalar unwrapping around an inner @()`);
  }
});

test("RC12 configure diagnostics fixture proves absent zero one many and syntax cardinalities", (t) => {
  const executable = findPowerShell();
  if (!executable) {
    t.skip("PowerShell is unavailable on this host; Windows release CI executes the cardinality fixture");
    return;
  }
  const result = spawnSync(executable, ["-NoProfile", "-NonInteractive", "-File", fixturePath], {
    encoding: "utf8",
    timeout: 30000,
  });
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  for (const expected of [
    "absent file -> Count 0",
    "empty file -> Count 0",
    "one line -> Count 1 and index 0 preserved",
    "many lines -> exact Count and ordering preserved",
    "zero matches -> Count 0",
    "one match -> Count 1 and index 0 preserved",
    "many matches -> exact Count and ordering preserved",
  ]) {
    assert.ok(result.stdout.includes(expected), `fixture output omitted: ${expected}`);
  }
});
