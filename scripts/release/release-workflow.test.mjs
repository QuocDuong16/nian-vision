import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import test from "node:test";

import { normalizeNewlines, readNormalizedText } from "./test-text.mjs";

const githubWorkflowPath = new URL("../../.github/workflows/release.yml", import.meta.url);
const forgejoReleasePath = new URL("../../.forgejo/workflows/release.yml", import.meta.url);
const forgejoQualityPath = new URL("../../.forgejo/workflows/quality.yml", import.meta.url);
const workflow = readNormalizedText(githubWorkflowPath);
const viteConfig = readNormalizedText(new URL("../../ui/vite.config.ts", import.meta.url));
const linuxAppImagePrepare = readNormalizedText(new URL("./prepare-linux-appimage.sh", import.meta.url));

function jobBodyFrom(source, name) {
  const marker = `  ${name}:\n`;
  const start = source.indexOf(marker);
  assert.ok(start >= 0, `missing job ${name}`);
  const rest = source.slice(start + marker.length);
  const next = rest.search(/^  [a-zA-Z0-9_-]+:\n/m);
  return next >= 0 ? rest.slice(0, next) : rest;
}

function jobBody(name) {
  return jobBodyFrom(workflow, name);
}

function stepBodyFrom(source, job, name) {
  const body = jobBodyFrom(source, job);
  const marker = `      - name: ${name}\n`;
  const start = body.indexOf(marker);
  assert.ok(start >= 0, `missing step ${job}/${name}`);
  const rest = body.slice(start + marker.length);
  const next = rest.search(/^      - name: /m);
  return next >= 0 ? rest.slice(0, next) : rest;
}

function stepBody(job, name) {
  return stepBodyFrom(workflow, job, name);
}

test("Forgejo production release workflow is removed while normal quality CI remains", () => {
  assert.equal(existsSync(forgejoReleasePath), false);
  assert.equal(existsSync(githubWorkflowPath), true);
  assert.equal(existsSync(forgejoQualityPath), true);
  const quality = readNormalizedText(forgejoQualityPath);
  assert.match(quality, /pull_request:/);
  assert.match(quality, /cargo check --workspace/);
  assert.match(quality, /cargo clippy --workspace --all-targets --all-features -- -D warnings/);
  assert.match(quality, /cargo test --workspace/);
});

test("workflow contract parsing is identical for LF and CRLF source text", () => {
  const crlf = workflow.replace(/\n/g, "\r\n");
  const normalized = normalizeNewlines(crlf);
  assert.equal(normalized, workflow);
  for (const job of ["release-preflight", "build-linux", "build-windows", "sign-linux", "sign-windows", "verify-release", "publish-release"]) {
    assert.equal(jobBodyFrom(normalized, job), jobBodyFrom(workflow, job));
  }
  assert.equal(
    stepBodyFrom(normalized, "build-linux", "Re-prove release source identity"),
    stepBodyFrom(workflow, "build-linux", "Re-prove release source identity"),
  );
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
  assert.match(workflow, /actions\/cache\/restore@0057852bfaa89a56745cba8c7296529d2fc39830 # v4\.3\.0/);
  assert.match(workflow, /actions\/cache\/save@0057852bfaa89a56745cba8c7296529d2fc39830 # v4\.3\.0/);
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

test("Linux container build explicitly executes run steps with Bash", () => {
  const linux = jobBody("build-linux");
  assert.match(linux, /container:\n      image: rust:1\.98\.0-bookworm/);
  assert.match(linux, /defaults:\n      run:\n        shell: bash/);
  assert.match(linux, /set -Eeuo pipefail/);
  assert.match(linux, /\[\[/);
  assert.equal(jobBody("build-windows").includes("defaults:\n      run:\n        shell: bash"), false);
});

test("Linux release image explicitly provisions missing rustfmt and clippy before release work", () => {
  const linux = jobBody("build-linux");
  const provisionAt = linux.indexOf("- name: Provision Rust quality components");
  const frontendAt = linux.indexOf("- name: Install frontend dependencies");
  const ffmpegAt = linux.indexOf("- name: Build pinned FFmpeg 8.0.3 Linux runtime");
  assert.ok(provisionAt >= 0 && provisionAt < frontendAt && provisionAt < ffmpegAt);
  const provision = stepBody("build-linux", "Provision Rust quality components");
  assert.match(provision, /rustup component add rustfmt clippy/);
  assert.match(provision, /rustc --version/);
  assert.match(provision, /cargo --version/);
  assert.match(provision, /cargo fmt --version/);
  assert.match(provision, /cargo clippy --version/);
});

test("Linux container trusts only the exact checkout before repository Git operations", () => {
  const linux = jobBody("build-linux");
  const checkoutMarker = "      - name: Check out exact release source\n";
  const trustMarker = "      - name: Trust checked-out workspace ownership\n";
  const identityMarker = "      - name: Re-prove release source identity\n";
  const checkout = linux.indexOf(checkoutMarker);
  const trust = linux.indexOf(trustMarker);
  const identity = linux.indexOf(identityMarker);

  assert.ok(checkout >= 0, "missing Linux checkout step");
  assert.ok(trust > checkout, "workspace trust must follow checkout");
  assert.ok(identity > trust, "workspace trust must precede source identity proof");
  assert.match(
    stepBody("build-linux", "Trust checked-out workspace ownership"),
    /git config --global --add safe\.directory "\$GITHUB_WORKSPACE"/,
  );
  assert.equal(/safe\.directory\s+(?:"\*"|'\*'|\*)/.test(workflow), false);
  assert.equal(jobBody("build-windows").includes("safe.directory"), false);

  const firstDirectRepositoryGit = linux.search(/^\s+git (?:fetch|status|rev-parse|show|merge-base)\b/m);
  assert.ok(firstDirectRepositoryGit > trust, "workspace trust must precede direct repository Git commands");
  assert.ok(linux.indexOf("version-check.mjs") > trust, "workspace trust must precede release scripts that inspect Git state");
});

test("Linux release compilation owns and disposes quality and worker intermediates before Tauri", () => {
  const linux = jobBody("build-linux");
  for (const step of [
    "Release script and updater-verifier tests",
    "Validate candidate FFmpeg against media fixtures",
    "Rust quality against release FFmpeg",
  ]) {
    assert.match(stepBody("build-linux", step), /CARGO_TARGET_DIR="\$RUNNER_TEMP\/nian-quality-target"/);
  }

  const qualityCleanup = stepBody("build-linux", "Dispose Linux quality Cargo target");
  assert.match(qualityCleanup, /if: always\(\)/);
  assert.match(qualityCleanup, /rm -rf "\$RUNNER_TEMP\/nian-quality-target"/);

  const stageAt = linux.indexOf("- name: Stage and clean-smoke Linux runtime");
  const handoffAt = linux.indexOf("- name: Release Linux Cargo intermediates before desktop build");
  const tauriBuildAt = linux.indexOf("- name: Build unsigned Linux application");
  const prepareAt = linux.indexOf("- name: Prepare Linux AppImage bundle disk");
  const bundleAt = linux.indexOf("- name: Bundle unsigned AppImage");
  assert.ok(stageAt >= 0 && stageAt < handoffAt && handoffAt < tauriBuildAt && tauriBuildAt < prepareAt && prepareAt < bundleAt);
  const handoff = stepBody("build-linux", "Release Linux Cargo intermediates before desktop build");
  assert.match(handoff, /rm -rf target\/release/);
  assert.equal(handoff.includes("rm -rf dist/linux-x86_64"), false);
  assert.equal(handoff.includes("rm -rf ui/dist"), false);

  const guard = stepBody("build-linux", "Guard Linux disk before Tauri build");
  assert.match(guard, /df -h "\$GITHUB_WORKSPACE"/);
  assert.match(guard, /du -sh target dist ui\/dist/);
  assert.match(guard, /minimum_free_kb=\$\(\(10 \* 1024 \* 1024\)\)/);
  assert.match(guard, /available_kb/);
  assert.match(stepBody("build-linux", "Upload unsigned Linux build"), /dist\/linux-x86_64\//);
});

test("Linux AppImage bundling diagnoses, narrowly prunes, guards, then bundles the already-built executable", () => {
  const build = stepBody("build-linux", "Build unsigned Linux application");
  assert.match(build, /tauri build --config tauri\.release\.generated\.conf\.json --no-bundle --ci/);
  assert.match(build, /test -x \.\.\/\.\.\/target\/release\/nian-desktop/);
  assert.equal(build.includes("tauri bundle"), false);

  const prepare = stepBody("build-linux", "Prepare Linux AppImage bundle disk");
  assert.match(prepare, /bash scripts\/release\/prepare-linux-appimage\.sh/);
  assert.match(linuxAppImagePrepare, /Post-Tauri-build disk diagnostics before release-target pruning:/);
  assert.match(linuxAppImagePrepare, /df -h/);
  assert.match(linuxAppImagePrepare, /df -Pk "\$workspace"/);
  assert.match(linuxAppImagePrepare, /du -sh "\$target_root"/);
  assert.match(linuxAppImagePrepare, /du -sh "\$release_dir"/);
  assert.match(linuxAppImagePrepare, /for intermediate in deps build \.fingerprint incremental/);
  assert.match(linuxAppImagePrepare, /rm -rf -- "\$release_dir\/\$intermediate"/);
  assert.equal(linuxAppImagePrepare.includes("rm -rf target/release"), false);
  assert.match(linuxAppImagePrepare, /desktop_sha_after.*desktop_sha_before/s);
  assert.match(linuxAppImagePrepare, /minimum_free_kb=\$\(\(4 \* 1024 \* 1024\)\)/);
  assert.match(linuxAppImagePrepare, /\[\[ "\$available_kb" =~ \^\[0-9\]\+\$ \]\]/);
  assert.match(linuxAppImagePrepare, /below the required 4 GiB guard/);

  const bundle = stepBody("build-linux", "Bundle unsigned AppImage");
  assert.match(bundle, /NIAN_UPDATER_CONFIGURED: "1"/);
  assert.match(bundle, /"\$tauri_cli" --version/);
  assert.match(bundle, /APPIMAGE_EXTRACT_AND_RUN=/);
  assert.match(bundle, /df -Pk "\$GITHUB_WORKSPACE"/);
  assert.match(bundle, /"\$tauri_cli" bundle --config tauri\.release\.generated\.conf\.json --bundles appimage --ci --no-sign --verbose/);
  assert.match(bundle, /bundle failed with exit code/);
  assert.match(bundle, /bundle_finished - bundle_started/);

  const post = stepBody("build-linux", "Report Linux AppImage bundle disk state");
  assert.match(post, /if: always\(\)/);
  assert.match(post, /df -Pk "\$GITHUB_WORKSPACE"/);
  assert.match(post, /target\/release\/bundle\/appimage/);
  assert.match(workflow, /APPIMAGE_EXTRACT_AND_RUN: "1"/);
});

test("generated release-only Tauri configs are disposed even after a preceding failure", () => {
  assert.match(stepBody("build-linux", "Dispose generated Tauri release configuration"), /if: always\(\)/);
  const windowsDisposals = [...jobBody("build-windows").matchAll(/- name: Dispose generated Windows Tauri configuration\n([\s\S]*?)(?=\n      - name: )/g)];
  assert.equal(windowsDisposals.length, 1);
  assert.match(windowsDisposals[0][1], /if: always\(\)/);
  assert.match(stepBody("sign-windows", "Dispose generated Windows Tauri configuration"), /if: always\(\)/);
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
