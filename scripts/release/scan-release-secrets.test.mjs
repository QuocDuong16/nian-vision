import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { scanPaths } from "./scan-release-secrets.mjs";

test("final secret sentinel scan accepts clean binary and text files", () => {
  const root = mkdtempSync(join(tmpdir(), "nian-release-secret-clean-"));
  mkdirSync(join(root, "nested"));
  writeFileSync(join(root, "nested", "asset.js"), "console.log(\"safe\")");
  writeFileSync(join(root, "artifact.bin"), Buffer.from([0, 1, 2, 3]));
  assert.equal(scanPaths([root], "sentinel-value").skipped, false);
});

test("final secret sentinel scan fails without printing the sentinel", () => {
  const root = mkdtempSync(join(tmpdir(), "nian-release-secret-hit-"));
  const sentinel = "signing-secret-canary-47";
  writeFileSync(join(root, "bundle.js"), `prefix-${sentinel}-suffix`);
  assert.throws(
    () => scanPaths([root], sentinel),
    (error) => {
      assert.match(error.message, /secret sentinel detected/);
      assert.equal(error.message.includes(sentinel), false);
      return true;
    },
  );
});
