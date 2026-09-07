import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const helperUrl = new URL("./windows-bounded-process.ps1", import.meta.url);
const helperPath = fileURLToPath(helperUrl);
const helper = readNormalizedText(helperUrl);

function findPowerShell() {
  const candidates = process.platform === "win32" ? ["pwsh.exe", "pwsh"] : ["pwsh"];
  return candidates.find((candidate) => {
    const probe = spawnSync(candidate, ["-NoProfile", "-Command", "$PSVersionTable.PSVersion.ToString()"], { encoding: "utf8" });
    return !probe.error && probe.status === 0;
  });
}

test("bounded Windows process helper kills process trees and fails closed on timeout", () => {
  assert.match(helper, /WaitForExit\(\$TimeoutSeconds \* 1000\)/);
  assert.match(helper, /\.Kill\(\$true\)/);
  assert.match(helper, /timed out after \{1\}s; process-tree termination was requested/);
  assert.match(helper, /if \(\$process\.ExitCode -ne 0\)/);
});

test("bounded Windows process helper timeout prevents later commands when PowerShell is available", (t) => {
  const executable = findPowerShell();
  if (!executable) {
    t.skip("PowerShell is unavailable on this host; Windows release CI executes the timeout probe");
    return;
  }

  const dir = mkdtempSync(join(tmpdir(), "nian-bounded-process-"));
  try {
    const probePath = join(dir, "probe.ps1");
    const quotedHelper = helperPath.replaceAll("'", "''");
    const quotedNode = process.execPath.replaceAll("'", "''");
    writeFileSync(
      probePath,
      [
        `. '${quotedHelper}'`,
        `Invoke-NianBoundedProcess -FilePath '${quotedNode}' -ArgumentList @('-e', 'setTimeout(() => {}, 5000)') -TimeoutSeconds 1 -Label 'controlled extraction timeout'`,
        "Write-Output 'unexpected-continuation'",
        "",
      ].join("\n"),
      "utf8",
    );

    const result = spawnSync(executable, ["-NoProfile", "-NonInteractive", "-File", probePath], { encoding: "utf8", timeout: 10000 });
    assert.equal(result.error, undefined, result.error?.message);
    assert.notEqual(result.status, 0);
    assert.match(`${result.stdout}\n${result.stderr}`, /controlled extraction timeout timed out after 1s/);
    assert.equal(result.stdout.includes("unexpected-continuation"), false);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
