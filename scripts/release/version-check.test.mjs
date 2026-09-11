import test from "node:test";
import assert from "node:assert/strict";

import { validateVersions } from "./version-check.mjs";

function versions(version) {
  return {
    workspace: version,
    tauri: version,
    package: version,
    ui: version,
  };
}

test("valid RC source and matching RC tag pass", () => {
  assert.equal(validateVersions(versions("1.0.0-rc.37"), "v1.0.0-rc.37"), "1.0.0-rc.37");
});

test("previous RC tag cannot be reused for current RC source", () => {
  for (const previous of [36, 35, 34, 33, 32, 31, 30, 29, 28, 27, 26, 25, 24, 23, 22, 21, 20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1]) {
    assert.throws(() => validateVersions(versions("1.0.0-rc.37"), `v1.0.0-rc.${previous}`), /does not match/);
  }
});

test("RC tag against final source is rejected", () => {
  assert.throws(() => validateVersions(versions("1.0.0"), "v1.0.0-rc.37"), /does not match/);
});

test("final tag against RC source is rejected", () => {
  assert.throws(() => validateVersions(versions("1.0.0-rc.37"), "v1.0.0"), /does not match/);
});

test("valid final source and matching final tag pass", () => {
  assert.equal(validateVersions(versions("1.0.0"), "v1.0.0"), "1.0.0");
});

test("surface version drift fails", () => {
  assert.throws(
    () => validateVersions({ ...versions("1.0.0-rc.37"), ui: "1.0.0" }),
    /release version drift/,
  );
});

test("malformed SemVer fails", () => {
  assert.throws(
    () => validateVersions({ ...versions("1.0.0-rc.37"), package: "01.2.3" }),
    /not valid SemVer/,
  );
});

test("valid prerelease SemVer with build metadata is accepted when every surface matches", () => {
  const value = "1.2.3-rc.1+build.5";
  assert.equal(validateVersions(versions(value), `v${value}`), value);
});
