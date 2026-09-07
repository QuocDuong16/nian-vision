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
  assert.equal(validateVersions(versions("1.0.0-rc.8"), "v1.0.0-rc.8"), "1.0.0-rc.8");
});

test("previous RC tag cannot be reused for current RC source", () => {
  assert.throws(() => validateVersions(versions("1.0.0-rc.8"), "v1.0.0-rc.7"), /does not match/);
  assert.throws(() => validateVersions(versions("1.0.0-rc.8"), "v1.0.0-rc.6"), /does not match/);
  assert.throws(() => validateVersions(versions("1.0.0-rc.8"), "v1.0.0-rc.5"), /does not match/);
  assert.throws(() => validateVersions(versions("1.0.0-rc.8"), "v1.0.0-rc.4"), /does not match/);
  assert.throws(() => validateVersions(versions("1.0.0-rc.8"), "v1.0.0-rc.3"), /does not match/);
  assert.throws(() => validateVersions(versions("1.0.0-rc.8"), "v1.0.0-rc.2"), /does not match/);
  assert.throws(() => validateVersions(versions("1.0.0-rc.8"), "v1.0.0-rc.1"), /does not match/);
});

test("RC tag against final source is rejected", () => {
  assert.throws(() => validateVersions(versions("1.0.0"), "v1.0.0-rc.8"), /does not match/);
});

test("final tag against RC source is rejected", () => {
  assert.throws(() => validateVersions(versions("1.0.0-rc.8"), "v1.0.0"), /does not match/);
});

test("valid final source and matching final tag pass", () => {
  assert.equal(validateVersions(versions("1.0.0"), "v1.0.0"), "1.0.0");
});

test("surface version drift fails", () => {
  assert.throws(
    () => validateVersions({ ...versions("1.0.0-rc.8"), ui: "1.0.0" }),
    /release version drift/,
  );
});

test("malformed SemVer fails", () => {
  assert.throws(
    () => validateVersions({ ...versions("1.0.0-rc.8"), package: "01.2.3" }),
    /not valid SemVer/,
  );
});

test("valid prerelease SemVer with build metadata is accepted when every surface matches", () => {
  const value = "1.2.3-rc.1+build.5";
  assert.equal(validateVersions(versions(value), `v${value}`), value);
});
