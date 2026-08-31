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
  assert.match(quality, /cargo clippy/);
  assert.match(quality, /cargo test/);
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

test("default GitHub permissions are read-only and only publish-release gets contents write", () => {
  assert.match(workflow, /permissions:\n  contents: read/);
  const writes = [...workflow.matchAll(/contents: write/g)];
  assert.equal(writes.length, 1);
  assert.match(jobBody("publish-release"), /permissions:\n      contents: write/);
  for (const job of ["release-preflight", "build-linux", "verify-release"]) {
    assert.equal(jobBody(job).includes("contents: write"), false, `${job} has repository write permission`);
  }
  assert.equal(/(?:actions|packages|issues|pull-requests): write/.test(workflow), false);
});

test("private signing secrets are scoped only to the Linux signing step", () => {
  const signing = stepBody("build-linux", "Build signed updater AppImage");
  assert.match(signing, /secrets\.TAURI_SIGNING_PRIVATE_KEY/);
  assert.match(signing, /secrets\.TAURI_SIGNING_PRIVATE_KEY_PASSWORD/);
  const withoutSigning = workflow.replace(signing, "");
  assert.equal(withoutSigning.includes("secrets.TAURI_SIGNING_PRIVATE_KEY"), false);
  assert.equal(withoutSigning.includes("secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD"), false);
  assert.equal(signing.includes("pnpm"), false);
  assert.equal(signing.includes("vite"), false);
  assert.equal(viteConfig.includes("envPrefix"), false);
});

test("release trust contract proves tag version SHA mirror actor and default-branch reachability", () => {
  const preflight = jobBody("release-preflight");
  assert.match(preflight, /refs\/tags\/v\*/);
  assert.match(preflight, /GITHUB_SHA/);
  assert.match(preflight, /refs\/tags\/\$\{tag\}\^\{commit\}/);
  assert.match(preflight, /merge-base --is-ancestor/);
  assert.match(preflight, /RELEASE_MIRROR_ACTOR/);
  assert.match(preflight, /version-check\.mjs --tag "\$tag" --require-clean/);
});

test("release topology is preflight to Linux build to verification to publication", () => {
  assert.match(jobBody("build-linux"), /needs: release-preflight/);
  assert.match(jobBody("verify-release"), /needs: \[release-preflight, build-linux\]/);
  assert.match(jobBody("publish-release"), /needs: \[release-preflight, verify-release\]/);
  assert.match(stepBody("build-linux", "Upload Linux release candidate"), /name: linux-release-candidate/);
  assert.match(stepBody("verify-release", "Upload verified release payload"), /name: verified-release/);
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

test("GitHub release uses stable latest metadata and exact tagged asset URLs", () => {
  const generate = stepBody("build-linux", "Generate release-only Tauri configuration");
  assert.match(generate, /releases\/latest\/download\/latest\.json/);
  const finalize = stepBody("build-linux", "Finalize GitHub Release candidate");
  assert.match(finalize, /releases\/download\/\$\{\{ github\.ref_name \}\}/);
  assert.equal(workflow.includes("NIAN_RELEASE_DOWNLOAD_BASE_URL: ${{ secrets."), false);
});

test("release workflow never pushes source changes or creates release tags", () => {
  assert.equal(/git push/.test(workflow), false);
  assert.equal(/git tag(?:\s|$)/.test(workflow), false);
  assert.equal(/version bump|npm version|cargo set-version/.test(workflow), false);
});


test("protected production-release environment is limited to signing verification and publication jobs", () => {
  assert.equal(jobBody("release-preflight").includes("environment: production-release"), false);
  for (const job of ["build-linux", "verify-release", "publish-release"]) {
    assert.match(jobBody(job), /environment: production-release/);
  }
});

test("updater public key is exposed only to configuration and cryptographic verification steps", () => {
  const allowed = [
    stepBody("build-linux", "Generate release-only Tauri configuration"),
    stepBody("build-linux", "Verify updater signature against configured public key"),
    stepBody("verify-release", "Reverify finalized updater signature"),
  ];
  const count = [...workflow.matchAll(/secrets\.NIAN_UPDATER_PUBLIC_KEY/g)].length;
  assert.equal(count, allowed.length);
  for (const body of allowed) assert.match(body, /secrets\.NIAN_UPDATER_PUBLIC_KEY/);
});
