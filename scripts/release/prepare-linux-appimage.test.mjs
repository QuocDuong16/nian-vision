import assert from "node:assert/strict";
import { chmodSync, existsSync, mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

const script = new URL("./prepare-linux-appimage.sh", import.meta.url);
const scriptText = readFileSync(script, "utf8").replace(/\r\n/g, "\n");

function fixture(availableKb) {
  const root = mkdtempSync(join(tmpdir(), "nian-appimage-disk-"));
  const release = join(root, "target", "release");
  const fakeBin = join(root, "fake-bin");
  mkdirSync(release, { recursive: true });
  mkdirSync(fakeBin, { recursive: true });

  const desktop = join(release, "nian-desktop");
  writeFileSync(desktop, "desktop-executable-bytes");
  chmodSync(desktop, 0o755);

  for (const name of ["deps", "build", ".fingerprint", "incremental"]) {
    mkdirSync(join(release, name), { recursive: true });
    writeFileSync(join(release, name, "intermediate.bin"), name);
  }
  writeFileSync(join(release, "libnian_desktop.so"), "preserve-top-level-file");
  mkdirSync(join(release, "bundle"));
  writeFileSync(join(release, "bundle", "keep.txt"), "keep-bundle-output");

  const df = join(fakeBin, "df");
  writeFileSync(
    df,
    `#!/usr/bin/env bash\nset -Eeuo pipefail\nif [[ "\${1:-}" == "-h" ]]; then\n  printf 'Filesystem Size Used Avail Use%% Mounted on\\nfake 14G 1G 13G 8%% /\\n'\n  exit 0\nfi\nif [[ "\${1:-}" == "-Pk" ]]; then\n  printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\\nfake 14680064 1024 ${availableKb} 1%% /\\n'\n  exit 0\nfi\necho "unexpected df arguments: $*" >&2\nexit 2\n`,
  );
  chmodSync(df, 0o755);

  return { root, release, desktop, fakeBin };
}

function runFixture(fx) {
  return spawnSync("bash", [script.pathname, fx.release, fx.root], {
    encoding: "utf8",
    env: { ...process.env, PATH: `${fx.fakeBin}:${process.env.PATH}` },
  });
}

test("Linux AppImage preparation prunes only proven compilation intermediates and preserves the desktop binary", (t) => {
  if (process.platform === "win32") {
    t.skip("Linux AppImage behavior is validated on the Linux release host");
    return;
  }

  const fx = fixture(5 * 1024 * 1024);
  try {
    const before = readFileSync(fx.desktop);
    const result = runFixture(fx);
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    assert.deepEqual(readFileSync(fx.desktop), before);
    for (const name of ["deps", "build", ".fingerprint", "incremental"]) {
      assert.equal(existsSync(join(fx.release, name)), false, `${name} was not pruned`);
    }
    assert.equal(existsSync(join(fx.release, "libnian_desktop.so")), true);
    assert.equal(existsSync(join(fx.release, "bundle", "keep.txt")), true);
    assert.match(result.stdout, /Post-Tauri-build disk diagnostics before release-target pruning:/);
    assert.match(result.stdout, /Post-prune disk diagnostics before AppImage free-space guard:/);
    assert.match(result.stdout, /required minimum: 4194304 KiB \(4 GiB\)/);
  } finally {
    rmSync(fx.root, { recursive: true, force: true });
  }
});

test("Linux AppImage preparation fails closed below the post-prune 4 GiB guard", (t) => {
  if (process.platform === "win32") {
    t.skip("Linux AppImage behavior is validated on the Linux release host");
    return;
  }

  const fx = fixture(4 * 1024 * 1024 - 1);
  try {
    const result = runFixture(fx);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /below the required 4 GiB guard/);
  } finally {
    rmSync(fx.root, { recursive: true, force: true });
  }
});

test("Linux AppImage preparation fails closed when df reports non-numeric available space", (t) => {
  if (process.platform === "win32") {
    t.skip("Linux AppImage behavior is validated on the Linux release host");
    return;
  }

  const fx = fixture("not-a-number");
  try {
    const result = runFixture(fx);
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /non-numeric available KiB/);
  } finally {
    rmSync(fx.root, { recursive: true, force: true });
  }
});

test("Linux AppImage pruning source is narrow and never removes target/release wholesale", () => {
  assert.match(scriptText, /for intermediate in deps build \.fingerprint incremental/);
  assert.match(scriptText, /rm -rf -- "\$release_dir\/\$intermediate"/);
  assert.doesNotMatch(scriptText, /rm -rf --? "?\$release_dir"?(?:\s|$)/m);
  assert.doesNotMatch(scriptText, /rm -rf target\/release/);
  assert.match(scriptText, /desktop_sha_after.*desktop_sha_before/s);
});
