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
  assert.equal(validateVersions(versions("1.0.0-rc.15"), "v1.0.0-rc.15"), "1.0.0-rc.15");
});

test("previous RC tag cannot be reused for current RC source", () => {
  for (const previous of [14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1]) {
    assert.throws(() => validateVersions(versions("1.0.0-rc.15"), `v1.0.0-rc.${previous}`), /does not match/);
  }
});

test("RC tag against final source is rejected", () => {
  assert.throws(() => validateVersions(versions("1.0.0"), "v1.0.0-rc.15"), /does not match/);
});

test("final tag against RC source is rejected", () => {
  assert.throws(() => validateVersions(versions("1.0.0-rc.15"), "v1.0.0"), /does not match/);
});

test("valid final source and matching final tag pass", () => {
  assert.equal(validateVersions(versions("1.0.0"), "v1.0.0"), "1.0.0");
});

test("surface version drift fails", () => {
  assert.throws(
    () => validateVersions({ ...versions("1.0.0-rc.15"), ui: "1.0.0" }),
    /release version drift/,
  );
});

test("malformed SemVer fails", () => {
  assert.throws(
    () => validateVersions({ ...versions("1.0.0-rc.15"), package: "01.2.3" }),
    /not valid SemVer/,
  );
});

test("valid prerelease SemVer with build metadata is accepted when every surface matches", () => {
  const value = "1.2.3-rc.1+build.5";
  assert.equal(validateVersions(versions(value), `v${value}`), value);
});
