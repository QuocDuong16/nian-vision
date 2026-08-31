import test from "node:test";
import assert from "node:assert/strict";

import { validateVersions } from "./version-check.mjs";

const matching = {
  workspace: "0.1.0",
  tauri: "0.1.0",
  package: "0.1.0",
  ui: "0.1.0",
};

test("matching release versions and tag pass", () => {
  assert.equal(validateVersions(matching, "v0.1.0"), "0.1.0");
});

test("tag/version mismatch fails", () => {
  assert.throws(() => validateVersions(matching, "v0.1.1"), /does not match/);
});

test("surface version drift fails", () => {
  assert.throws(
    () => validateVersions({ ...matching, ui: "0.2.0" }),
    /release version drift/,
  );
});

test("malformed SemVer fails", () => {
  assert.throws(
    () => validateVersions({ ...matching, package: "01.2.3" }),
    /not valid SemVer/,
  );
});

test("valid prerelease SemVer is accepted when every surface matches", () => {
  const versions = Object.fromEntries(
    Object.keys(matching).map((key) => [key, "1.2.3-rc.1+build.5"]),
  );
  assert.equal(validateVersions(versions, "v1.2.3-rc.1+build.5"), "1.2.3-rc.1+build.5");
});
