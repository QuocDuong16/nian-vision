import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import { readNormalizedText } from "./test-text.mjs";

const helperUrl = new URL("./windows-bash-script.ps1", import.meta.url);
const fixtureUrl = new URL("./test-windows-bash-script.ps1", import.meta.url);
const probeUrl = new URL("./test-ffmpeg-msys-escape.ps1", import.meta.url);
const buildUrl = new URL("./build-ffmpeg-windows.ps1", import.meta.url);
const validatorUrl = new URL("./validate-ffmpeg-msys-dependency.ps1", import.meta.url);
const provisionUrl = new URL("./provision-ffmpeg-msys-tools.ps1", import.meta.url);
const preflightUrl = new URL("./preflight-windows.ps1", import.meta.url);
const contract = JSON.parse(readNormalizedText(new URL("./ffmpeg-windows-contract.json", import.meta.url)));
const helper = readNormalizedText(helperUrl);
const fixture = readNormalizedText(fixtureUrl);
const probe = readNormalizedText(probeUrl);
const build = readNormalizedText(buildUrl);
const validator = readNormalizedText(validatorUrl);
const provision = readNormalizedText(provisionUrl);
const preflight = readNormalizedText(preflightUrl);

function findCommand(candidates, args = ["--version"]) {
  return candidates.find((candidate) => {
    const result = spawnSync(candidate, args, { encoding: "utf8" });
    return !result.error && result.status === 0;
  });
}

function bashQuote(value) {
  return `'${value.replaceAll("'", `'\\''`)}'`;
}

function extractAggregateTemplate() {
  const match = probe.match(/\$probe = @'\n([\s\S]*?)\n'@\n\n\$probe = \$probe\.Replace/);
  assert.ok(match, "aggregate Bash probe template was not found");
  return match[1];
}

function representativeProbe(controlledPath) {
  const pairs = Object.entries(contract.msysBuildTools).map(([name, path]) => `${name} ${path}`).join("\n");
  const replacements = {
    __CONTROLLED_PATH__: bashQuote(controlledPath),
    __FORBIDDEN_ROOTS__: ["/mingw64/bin", "/c/mingw64/bin", "/ucrt64/bin", "/clang64/bin", "/clangarm64/bin"].map(bashQuote).join(" "),
    __MSYS_PAIRS__: pairs,
    __CL__: bashQuote("C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise\\VC\\Tools\\MSVC\\14.99.99999\\bin\\HostX64\\x64\\cl.exe"),
    __LIB__: bashQuote("C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise\\VC\\Tools\\MSVC\\14.99.99999\\bin\\HostX64\\x64\\lib.exe"),
    __LINK__: bashQuote("C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise\\VC\\Tools\\MSVC\\14.99.99999\\bin\\HostX64\\x64\\link.exe"),
    __DUMPBIN__: bashQuote("C:\\Program Files\\Microsoft Visual Studio\\2022\\Enterprise\\VC\\Tools\\MSVC\\14.99.99999\\bin\\HostX64\\x64\\dumpbin.exe"),
    __RC__: bashQuote("C:\\Program Files (x86)\\Windows Kits\\10\\bin\\10.0.99999.0\\x64\\rc.exe"),
    __MAKE_VERSION_LINE__: "GNU Make 4.4.1",
  };
  let script = extractAggregateTemplate();
  for (const [placeholder, value] of Object.entries(replacements)) script = script.replaceAll(placeholder, value);
  return script;
}

test("RC11 generated Bash helper writes LF UTF-8 without BOM, syntax-checks, executes, and cleans up", () => {
  assert.match(helper, /\[Text\.UTF8Encoding\]::new\(\$false\)/);
  assert.match(helper, /\.Replace\("`r`n", "`n"\)\.Replace\("`r", "`n"\)/);
  assert.ok(helper.includes("'__[A-Z][A-Z0-9_]*__'"));
  assert.match(helper, /-ArgumentList @\('--noprofile', '--norc', '-n', \$scriptInvocationPath\)/);
  assert.match(helper, /-ArgumentList @\('--noprofile', '--norc', \$scriptInvocationPath\)/);
  assert.equal(helper.includes("'-c'"), false);
  assert.ok(helper.indexOf("'-n', $scriptInvocationPath") < helper.indexOf("Invoke-NianNative { & $Bash --noprofile --norc $scriptInvocationPath }"));
  assert.match(helper, /finally \{[\s\S]*?Remove-Item -Recurse -Force -LiteralPath \$directory -ErrorAction SilentlyContinue/);
  assert.match(fixture, /syntax failure reached behavioral execution/);
  assert.match(fixture, /UTF-8 BOM fixture failure/);
  assert.match(fixture, /LF normalization fixture failure/);
  assert.match(fixture, /__NIAN_UNRESOLVED__/);
});

test("RC11 large generated Bash bodies are file-backed and no longer transported through -lc", () => {
  for (const source of [probe, validator, build]) assert.match(source, /windows-bash-script\.ps1/);
  assert.match(probe, /-FileName 'nian-ffmpeg-msys-toolchain-probe\.sh'/);
  assert.match(validator, /-FileName 'nian-ffmpeg-ccdep-probe\.sh'/);
  for (const name of ["nian-ffmpeg-extract.sh", "nian-ffmpeg-configure.sh", "nian-ffmpeg-compile.sh", "nian-ffmpeg-install.sh"]) {
    assert.ok(build.includes(`-FileName '${name}'`), `missing file-backed build phase ${name}`);
  }
  assert.equal(/& \$Bash[^\n]*-lc \$probe/.test(probe), false);
  assert.equal(/& \$Bash[^\n]*-lc \$command/.test(validator), false);
  assert.equal(/-lc[^\n]*(?:\$extractCommand|\$configureCommand|\$MsysBuildPreamble)/.test(build), false);

  // The only reviewed -lc sites left are intentionally small fixed commands.
  assert.match(provision, /-lc', \$installCommand/);
  assert.match(provision, /\$installCommand = "\/usr\/bin\/pacman --noconfirm --needed -U/);
  assert.match(preflight, /-lc 'set -Eeuo pipefail; export PATH=\/usr\/bin; test -x \/usr\/bin\/tar; test -x \/usr\/bin\/xz'/);
});

test("RC11 aggregate probe resolves every placeholder before file-backed invocation", () => {
  const invocationAt = probe.indexOf("Invoke-NianBashScript");
  assert.ok(invocationAt > 0);
  for (const placeholder of [
    "__CONTROLLED_PATH__",
    "__FORBIDDEN_ROOTS__",
    "__MSYS_PAIRS__",
    "__CL__",
    "__LIB__",
    "__LINK__",
    "__DUMPBIN__",
    "__RC__",
    "__MAKE_VERSION_LINE__",
  ]) {
    const replacementAt = probe.indexOf(`$probe = $probe.Replace('${placeholder}'`);
    assert.ok(replacementAt >= 0 && replacementAt < invocationAt, `${placeholder} is not replaced before invocation`);
  }
  assert.match(helper, /generated Bash script contains unresolved placeholders/);
});

test("RC11 representative aggregate probe with long VS and SDK paths passes bash -n when Bash is available", (t) => {
  const bash = findCommand(process.platform === "win32" ? ["bash.exe", "bash"] : ["bash"]);
  if (!bash) {
    t.skip("Bash is unavailable on this host");
    return;
  }
  const segment = "/c/Program Files/Microsoft Visual Studio/2022/Enterprise/VC/Tools/MSVC/14.99.99999/bin/HostX64/x64:/c/Program Files (x86)/Windows Kits/10/bin/10.0.99999.0/x64";
  const controlledPath = `${Array.from({ length: 700 }, () => segment).join(":")}:/c/Users/O'Brien/bin:/usr/bin:/c/Windows/System32:/c/mingw64/bin`;
  assert.ok(controlledPath.length > 60000);
  const script = representativeProbe(controlledPath);
  assert.equal(/__[A-Z][A-Z0-9_]*__/.test(script), false);

  const root = mkdtempSync(join(tmpdir(), "nian-rc11-aggregate-bash-"));
  try {
    const path = join(root, "nian-ffmpeg-msys-toolchain-probe.sh");
    writeFileSync(path, script.replace(/\r\n?/g, "\n"), "utf8");
    const bytes = readFileSync(path);
    assert.notDeepEqual([...bytes.subarray(0, 3)], [0xef, 0xbb, 0xbf]);
    assert.equal(bytes.includes(0x0d), false);
    const result = spawnSync(bash, ["--noprofile", "--norc", "-n", path], { encoding: "utf8", timeout: 30000 });
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("RC11 PowerShell Bash helper fixture executes when PowerShell and Bash are available", (t) => {
  const pwsh = findCommand(process.platform === "win32" ? ["pwsh.exe", "pwsh"] : ["pwsh"], ["-NoProfile", "-Command", "$PSVersionTable.PSVersion.ToString()"]);
  const bash = findCommand(process.platform === "win32" ? ["bash.exe", "bash"] : ["bash"]);
  if (!pwsh || !bash) {
    t.skip("PowerShell and Bash are required for the cross-platform generated-script fixture");
    return;
  }
  const result = spawnSync(pwsh, ["-NoProfile", "-NonInteractive", "-File", fileURLToPath(fixtureUrl)], { encoding: "utf8", timeout: 30000 });
  assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
  assert.match(result.stdout, /UTF-8 no BOM and LF only/);
  assert.match(result.stdout, /long PATH fixture PASS/);
  assert.match(result.stdout, /bash -n blocked execution and cleanup ran/);
  assert.match(result.stdout, /unresolved-placeholder fixture PASS/);
});
