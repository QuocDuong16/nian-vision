import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { collectVersions, validateVersions } from "./version-check.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

function command(program, args = []) {
  return execFileSync(program, args, { cwd: root, encoding: "utf8" }).trim();
}

function argValue(argv, name, fallback) {
  const index = argv.indexOf(name);
  if (index === -1) return fallback;
  if (!argv[index + 1]) throw new Error(`${name} requires a value`);
  return argv[index + 1];
}

function rejectUnsafeStrings(value) {
  const serialized = JSON.stringify(value);
  const forbidden = [
    /[A-Za-z]:\\Users\\/i,
    /\/home\/[A-Za-z0-9._-]+\//,
    /vcpkg[\\/]+installed/i,
    /msys64/i,
    /BEGIN (?:RSA |OPENSSH )?PRIVATE KEY/,
  ];
  for (const pattern of forbidden) {
    if (pattern.test(serialized)) throw new Error(`build metadata contains forbidden path/secret pattern: ${pattern}`);
  }
}

function validatePnpmVersion(version) {
  if (!version) throw new Error("--pnpm-version is required");
  const mise = readFileSync(resolve(root, "mise.toml"), "utf8");
  const lock = readFileSync(resolve(root, "mise.lock"), "utf8");
  const expected = /^pnpm\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"\s*$/m.exec(mise)?.[1];
  if (!expected) throw new Error("mise.toml must pin an exact pnpm version");
  const locked = /^\[\[tools\.pnpm\]\]\s*\nversion\s*=\s*"([^"]+)"/m.exec(lock)?.[1];
  if (locked !== expected) throw new Error(`mise.lock pnpm version ${locked ?? "missing"} differs from mise.toml ${expected}`);
  if (version !== expected) throw new Error(`pnpm version ${version} does not match mise.toml ${expected}`);
  return version;
}

try {
  const argv = process.argv.slice(2);
  const output = argValue(argv, "--output", resolve(root, "dist/linux-x86_64/BUILD_METADATA.json"));
  const target = argValue(argv, "--target", "x86_64-unknown-linux-gnu");
  const profile = argValue(argv, "--profile", "release");
  const releaseConfig = JSON.parse(
    readFileSync(resolve(root, "scripts/release/release-config.json"), "utf8"),
  );
  const pnpmVersion = validatePnpmVersion(argValue(argv, "--pnpm-version"));
  const version = validateVersions(collectVersions());
  const metadata = {
    product: "Nian Vision",
    version,
    commit: command("git", ["rev-parse", "HEAD"]),
    rust: command("rustc", ["--version"]),
    node: command("node", ["--version"]),
    pnpm: pnpmVersion,
    ffmpeg_version: releaseConfig.ffmpegVersion,
    ffmpeg_source_sha256: releaseConfig.ffmpegSourceSha256,
    target,
    profile,
  };
  rejectUnsafeStrings(metadata);
  mkdirSync(dirname(output), { recursive: true });
  writeFileSync(output, `${JSON.stringify(metadata, null, 2)}\n`);
  process.stdout.write(`${output}\n`);
} catch (error) {
  console.error(`build metadata generation failed: ${error.message}`);
  process.exitCode = 1;
}
