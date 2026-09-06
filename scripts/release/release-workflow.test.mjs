import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import test from "node:test";

const githubWorkflowPath = new URL("../../.github/workflows/release.yml", import.meta.url);
const forgejoReleasePath = new URL("../../.forgejo/workflows/release.yml", import.meta.url);
const forgejoQualityPath = new URL("../../.forgejo/workflows/quality.yml", import.meta.url);
const workflow = readFileSync(githubWorkflowPath, "utf8");
const viteConfig = readFileSync(new URL("../../ui/vite.config.ts", import.meta.url), "utf8");

function jobBody(name) {
  const marker = `  ${name}:\n`;
  const start = workflow.indexOf(marker);
  assert.ok(start >= 0, `missing job ${name}`);
  const rest = workflow.slice(start + marker.length);
  const next = rest.search(/^  [a-zA-Z0-9_-]+:\n/m);
  return next >= 0 ? rest.slice(0, next) : rest;
}

function stepBody(job, name) {
  const body = jobBody(job);
  const marker = `      - name: ${name}\n`;
  const start = body.indexOf(marker);
  assert.ok(start >= 0, `missing step ${job}/${name}`);
  const rest = body.slice(start + marker.length);
  const next = rest.search(/^      - name: /m);
  return next >= 0 ? rest.slice(0, next) : rest;
}

test("Forgejo production release workflow is removed while normal quality CI remains", () => {
  assert.equal(existsSync(forgejoReleasePath), false);
  assert.equal(existsSync(githubWorkflowPath), true);
  assert.equal(existsSync(forgejoQualityPath), true);
  const quality = readFileSync(forgejoQualityPath, "utf8");
  assert.match(quality, /pull_request:/);
  assert.match(quality, /cargo check --workspace/);
  assert.match(quality, /cargo clippy --workspace --all-targets --all-features -- -D warnings/);
  assert.match(quality, /cargo test --workspace/);
});

test("GitHub release workflow auto-triggers only from v* tag pushes", () => {
  const trigger = workflow.slice(0, workflow.indexOf("permissions:"));
  assert.match(trigger, /push:\n    tags:\n      - "v\*"/);
  assert.equal(trigger.includes("pull_request:"), false);
  assert.equal(trigger.includes("branches:"), false);
  assert.equal(trigger.includes("workflow_dispatch:"), false);
});

test("all GitHub release actions are pinned to immutable commit SHAs", () => {
  const uses = [...workflow.matchAll(/^\s+uses:\s+(\S+)@([^\s]+)(?:\s+#.*)?$/gm)];
  assert.ok(uses.length >= 3);
  for (const [, action, revision] of uses) {
    assert.match(revision, /^[0-9a-f]{40}$/, `${action} is not pinned to a 40-character commit SHA`);
  }
  assert.match(workflow, /actions\/checkout@11bd71901bbe5b1630ceea73d27597364c9af683 # v4\.2\.2/);
  assert.match(workflow, /actions\/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02 # v4\.6\.2/);
  assert.match(workflow, /actions\/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093 # v4\.3\.0/);
  assert.equal(workflow.includes("softprops/action-gh-release"), false);
});

test("default permissions are read-only and only publication can write repository contents", () => {
  assert.match(workflow, /permissions:\n  contents: read/);
  assert.equal([...workflow.matchAll(/contents: write/g)].length, 1);
  assert.match(jobBody("publish-release"), /permissions:\n      contents: write/);
  for (const job of ["release-preflight", "build-linux", "build-windows", "sign-linux", "sign-windows", "verify-release"]) {
    assert.equal(jobBody(job).includes("contents: write"), false, `${job} has repository write permission`);
  }
  assert.equal(/(?:actions|packages|issues|pull-requests): write/.test(workflow), false);
});

test("ordinary compiler jobs are outside the protected release environment", () => {
  for (const job of ["release-preflight", "build-linux", "build-windows"]) {
    assert.equal(jobBody(job).includes("environment: production-release"), false, `${job} unexpectedly requires protected approval`);
  }
  for (const job of ["sign-linux", "sign-windows", "verify-release", "publish-release"]) {
    assert.match(jobBody(job), /environment: production-release/, `${job} must use the protected release environment`);
  }
});

test("updater private key is scoped only to updater-signing steps", () => {
  const linux = stepBody("sign-linux", "Sign Linux updater artifact");
  const windows = stepBody("sign-windows", "Sign final Windows updater artifact");
  assert.match(linux, /secrets\.TAURI_SIGNING_PRIVATE_KEY/);
  assert.match(linux, /secrets\.TAURI_SIGNING_PRIVATE_KEY_PASSWORD/);
  assert.match(windows, /secrets\.TAURI_SIGNING_PRIVATE_KEY/);
  assert.match(windows, /secrets\.TAURI_SIGNING_PRIVATE_KEY_PASSWORD/);
  let outside = workflow.replace(linux, "").replace(windows, "");
  assert.equal(outside.includes("secrets.TAURI_SIGNING_PRIVATE_KEY"), false);
  assert.equal(outside.includes("secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD"), false);
  assert.equal(linux.includes("pnpm install"), false);
  assert.equal(windows.includes("pnpm install"), false);
  assert.equal(viteConfig.includes("envPrefix"), false);
});

test("Windows Authenticode private material is confined to Windows signing steps", () => {
  const binaries = stepBody("sign-windows", "Authenticode-sign Windows application binaries when configured");
  const installer = stepBody("sign-windows", "Authenticode-sign and verify final NSIS installer when configured");
  for (const body of [binaries, installer]) {
    assert.match(body, /secrets\.WINDOWS_SIGNING_PFX_BASE64/);
    assert.match(body, /secrets\.WINDOWS_SIGNING_PFX_PASSWORD/);
  }
  const outside = workflow.replace(binaries, "").replace(installer, "");
  assert.equal(outside.includes("WINDOWS_SIGNING_PFX_BASE64"), false);
  assert.equal(outside.includes("WINDOWS_SIGNING_PFX_PASSWORD"), false);
});

test("release trust contract proves mirrored tag version SHA actor and default-branch reachability", () => {
  const preflight = jobBody("release-preflight");
  assert.match(preflight, /refs\/tags\/v\*/);
  assert.match(preflight, /GITHUB_SHA/);
  assert.match(preflight, /refs\/tags\/\$\{tag\}\^\{commit\}/);
  assert.match(preflight, /merge-base --is-ancestor/);
  assert.match(preflight, /RELEASE_MIRROR_ACTOR/);
  assert.match(preflight, /version-check\.mjs --tag "\$tag" --require-clean/);
});

test("Linux and Windows build jobs are peers and both signed candidates gate verification", () => {
  assert.match(jobBody("build-linux"), /needs: release-preflight/);
  assert.match(jobBody("build-windows"), /needs: release-preflight/);
  assert.equal(jobBody("build-windows").includes("needs: build-linux"), false);
  assert.match(jobBody("sign-linux"), /needs: \[release-preflight, build-linux\]/);
  assert.match(jobBody("sign-windows"), /needs: \[release-preflight, build-windows\]/);
  assert.match(jobBody("verify-release"), /needs: \[release-preflight, sign-linux, sign-windows\]/);
  assert.match(jobBody("publish-release"), /needs: \[release-preflight, verify-release\]/);
});

test("Linux signing is isolated from its compiler and dependency build", () => {
  assert.equal(jobBody("build-linux").includes("TAURI_SIGNING_PRIVATE_KEY"), false);
  assert.equal(jobBody("build-linux").includes("secrets."), false);
  assert.equal(jobBody("sign-linux").includes("pnpm install"), false);
  assert.equal(jobBody("sign-linux").includes("vite"), false);
  assert.equal(jobBody("sign-windows").includes("pnpm install"), false);
  assert.equal(jobBody("sign-windows").includes("vite"), false);
  assert.match(stepBody("build-linux", "Upload unsigned Linux build"), /name: linux-unsigned-build/);
  assert.match(stepBody("sign-linux", "Upload Linux release candidate"), /name: linux-release-candidate/);
});

test("GitHub release uses stable latest metadata and exact tagged asset assembly", () => {
  for (const job of ["build-linux", "build-windows"]) {
    const name = job === "build-linux" ? "Generate release-only Tauri configuration" : "Generate public-only Windows Tauri configuration";
    assert.match(stepBody(job, name), /releases\/latest\/download\/latest\.json/);
  }
  assert.match(stepBody("verify-release", "Assemble one public multi-platform release"), /releases\/download\/\$\{\{ github\.ref_name \}\}/);
  assert.equal(workflow.includes("NIAN_RELEASE_DOWNLOAD_BASE_URL: ${{ secrets."), false);
});

test("updater public key is a non-secret variable used only for config and verification", () => {
  assert.equal(workflow.includes("secrets.NIAN_UPDATER_PUBLIC_KEY"), false);
  const occurrences = [...workflow.matchAll(/vars\.NIAN_UPDATER_PUBLIC_KEY/g)].length;
  assert.equal(occurrences, 6);
  assert.match(stepBody("build-linux", "Generate release-only Tauri configuration"), /vars\.NIAN_UPDATER_PUBLIC_KEY/);
  assert.match(stepBody("build-windows", "Generate public-only Windows Tauri configuration"), /vars\.NIAN_UPDATER_PUBLIC_KEY/);
  assert.match(stepBody("sign-linux", "Verify Linux updater signature"), /vars\.NIAN_UPDATER_PUBLIC_KEY/);
  assert.match(stepBody("sign-windows", "Generate Windows bundle configuration"), /vars\.NIAN_UPDATER_PUBLIC_KEY/);
  assert.match(stepBody("sign-windows", "Verify Windows updater signature"), /vars\.NIAN_UPDATER_PUBLIC_KEY/);
  assert.match(stepBody("verify-release", "Reverify both updater signatures"), /vars\.NIAN_UPDATER_PUBLIC_KEY/);
});

test("publication is draft-first and byte-verifies uploaded assets before publish", () => {
  const publish = jobBody("publish-release");
  const createAt = publish.indexOf("gh release create");
  const uploadAt = publish.indexOf("gh release upload");
  const downloadAt = publish.indexOf("gh release download");
  const compareAt = publish.indexOf("cmp --silent");
  const publishAt = publish.indexOf("--draft=false");
  assert.ok(createAt >= 0 && createAt < uploadAt && uploadAt < downloadAt && downloadAt < compareAt && compareAt < publishAt);
  assert.match(publish, /--draft/);
  assert.match(publish, /refusing to overwrite an already-published release/);
});

test("release candidates remain prereleases and cannot replace the production latest channel", () => {
  const publish = jobBody("publish-release");
  assert.match(publish, /RELEASE_TAG" == \*-\*/);
  assert.match(publish, /release_flags\+=\(--prerelease\)/);
  assert.match(publish, /--draft=false --prerelease --latest=false/);
  assert.match(publish, /--draft=false --latest/);
});

test("release workflow never pushes source changes or creates release tags", () => {
  assert.equal(/git push/.test(workflow), false);
  assert.equal(/git tag(?:\s|$)/.test(workflow), false);
  assert.equal(/version bump|npm version|cargo set-version/.test(workflow), false);
});
