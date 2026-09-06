import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const helperUrl = new URL("./windows-native.ps1", import.meta.url);
const helperPath = fileURLToPath(helperUrl);
const helper = readNormalizedText(helperUrl);

test("Windows native helper has modern PowerShell and explicit exit-code fail-closed paths", () => {
  assert.match(helper, /PSNativeCommandUseErrorActionPreference/);
  assert.match(helper, /\$exitCode = \$LASTEXITCODE/);
  assert.match(helper, /if \(\$exitCode -ne 0\)/);
  assert.match(helper, /throw "required native command failed with exit code \$exitCode: \$Label"/);
});

test("a failing native command cannot be masked by a later successful command when PowerShell is available", (t) => {
  const candidates = process.platform === "win32" ? ["pwsh.exe", "pwsh"] : ["pwsh"];
  const executable = candidates.find((candidate) => {
    const probe = spawnSync(candidate, ["-NoProfile", "-Command", "$PSVersionTable.PSVersion.ToString()"], { encoding: "utf8" });
    return !probe.error && probe.status === 0;
  });
  if (!executable) {
    t.skip("PowerShell is unavailable on this host; Windows release CI executes this behavioral probe");
    return;
  }

  const dir = mkdtempSync(join(tmpdir(), "nian-native-failclosed-"));
  try {
    const probePath = join(dir, "probe.ps1");
    const quotedHelper = helperPath.replaceAll("'", "''");
    writeFileSync(
      probePath,
      `. '${quotedHelper}'\nInvoke-NianNative { node -e "process.exit(7)" }\nWrite-Output 'masked-success'\n`,
      "utf8",
    );
    const result = spawnSync(executable, ["-NoProfile", "-File", probePath], { encoding: "utf8" });
    assert.notEqual(result.status, 0);
    assert.equal(result.stdout.includes("masked-success"), false);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
