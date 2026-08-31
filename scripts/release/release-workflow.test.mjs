import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const workflow = readFileSync(new URL("../../.forgejo/workflows/release.yml", import.meta.url), "utf8");
const viteConfig = readFileSync(new URL("../../ui/vite.config.ts", import.meta.url), "utf8");

function stepsByName() {
  const parts = workflow.split(/^      - name: /m).slice(1);
  return new Map(parts.map((part) => {
    const newline = part.indexOf("\n");
    return [part.slice(0, newline).trim(), part.slice(newline + 1)];
  }));
}

const steps = stepsByName();

test("release job-level environment contains no release secrets", () => {
  const beforeSteps = workflow.slice(0, workflow.indexOf("    steps:"));
  assert.equal(beforeSteps.includes("${{ secrets."), false);
  assert.equal(beforeSteps.includes("TAURI_SIGNING_PRIVATE_KEY"), false);
  assert.equal(beforeSteps.includes("TAURI_SIGNING_PRIVATE_KEY_PASSWORD"), false);
});

test("updater private signing material is scoped only to the signed AppImage step", () => {
  const build = steps.get("Build signed updater AppImage");
  assert.ok(build);
  assert.match(build, /TAURI_SIGNING_PRIVATE_KEY: \${{ secrets\.TAURI_SIGNING_PRIVATE_KEY }}/);
  assert.match(build, /TAURI_SIGNING_PRIVATE_KEY_PASSWORD: \${{ secrets\.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}/);
  for (const [name, body] of steps) {
    if (name === "Build signed updater AppImage") continue;
    assert.equal(body.includes("secrets.TAURI_SIGNING_PRIVATE_KEY"), false, `${name} receives updater private key material`);
    assert.equal(body.includes("TAURI_SIGNING_PRIVATE_KEY_PASSWORD"), false, `${name} receives updater private key password`);
  }
  assert.equal(build.includes("pnpm"), false);
  assert.equal(build.includes("vite"), false);
});

test("frontend build cannot inherit updater signing variables", () => {
  const frontend = steps.get("Frontend quality and release asset build");
  assert.ok(frontend);
  assert.equal(frontend.includes("TAURI_SIGNING_"), false);
  assert.equal(viteConfig.includes("envPrefix"), false);
  const generate = steps.get("Generate release-only Tauri configuration");
  assert.ok(generate);
  assert.equal(generate.includes("TAURI_SIGNING_"), false);
});

test("all production release actions are pinned to immutable revisions", () => {
  const uses = [...workflow.matchAll(/^\s+uses:\s+(\S+)@([^\s]+)(?:\s+#.*)?$/gm)];
  assert.ok(uses.length > 0);
  for (const [, action, revision] of uses) {
    assert.match(revision, /^[0-9a-f]{40}$/, `${action} is not pinned to a 40-character commit SHA`);
  }
  assert.match(
    workflow,
    /forgejo\/upload-artifact@16871d9e8cfcf27ff31822cac382bbb5450f1e1e # v4/,
  );
});

test("public updater and authority values are scoped only to the steps that need them", () => {
  assert.match(steps.get("Generate release-only Tauri configuration"), /secrets\.NIAN_UPDATER_ENDPOINT/);
  assert.match(steps.get("Generate release-only Tauri configuration"), /secrets\.NIAN_UPDATER_PUBLIC_KEY/);
  assert.match(steps.get("Verify updater signature against configured public key"), /secrets\.NIAN_UPDATER_PUBLIC_KEY/);
  assert.match(steps.get("Finalize manifest, updater metadata and SHA-256 checksums"), /secrets\.NIAN_RELEASE_DOWNLOAD_BASE_URL/);
  assert.equal(steps.get("Upload Linux release artifacts").includes("secrets."), false);
});

test("secret sentinel reaches stage, extracted AppImage smoke, and final artifact scan only", () => {
  const expected = new Set([
    "Stage and clean-smoke Linux runtime",
    "Smoke actual AppImage runtime and desktop startup",
    "Final secret sentinel scan",
  ]);
  for (const [name, body] of steps) {
    assert.equal(body.includes("NIAN_RELEASE_SECRET_SENTINEL"), expected.has(name), `${name} has unexpected sentinel scope`);
  }
});

test("signature verification precedes config disposal, smoke, finalization, and upload", () => {
  const verifyAt = workflow.indexOf("- name: Verify updater signature against configured public key");
  const disposeAt = workflow.indexOf("- name: Dispose generated Tauri release configuration");
  const smokeAt = workflow.indexOf("- name: Smoke actual AppImage runtime and desktop startup");
  const finalizeAt = workflow.indexOf("- name: Finalize manifest, updater metadata and SHA-256 checksums");
  const uploadAt = workflow.indexOf("- name: Upload Linux release artifacts");
  assert.ok(verifyAt > 0 && verifyAt < disposeAt && disposeAt < smokeAt && smokeAt < finalizeAt && finalizeAt < uploadAt);
});
