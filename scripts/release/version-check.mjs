import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
export const repoRoot = resolve(scriptDir, "../..");

const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/;

function jsonVersion(path) {
  return JSON.parse(readFileSync(path, "utf8")).version;
}

export function workspaceVersion(root = repoRoot) {
  const cargo = readFileSync(resolve(root, "Cargo.toml"), "utf8");
  const section = cargo.match(/\[workspace\.package\]([\s\S]*?)(?:\n\[|$)/)?.[1];
  const version = section?.match(/^version\s*=\s*"([^"]+)"\s*$/m)?.[1];
  if (!version) throw new Error("workspace.package.version is missing");
  return version;
}

export function collectVersions(root = repoRoot) {
  return {
    workspace: workspaceVersion(root),
    tauri: jsonVersion(resolve(root, "apps/nian-desktop/tauri.conf.json")),
    package: jsonVersion(resolve(root, "package.json")),
    ui: jsonVersion(resolve(root, "ui/package.json")),
  };
}

export function validateVersions(versions, tag) {
  const entries = Object.entries(versions);
  for (const [name, version] of entries) {
    if (typeof version !== "string" || !SEMVER.test(version)) {
      throw new Error(`${name} version is not valid SemVer: ${String(version)}`);
    }
  }
  const authoritative = versions.workspace;
  const mismatched = entries.filter(([, version]) => version !== authoritative);
  if (mismatched.length) {
    throw new Error(
      `release version drift: workspace=${authoritative}; ${mismatched
        .map(([name, version]) => `${name}=${version}`)
        .join(", ")}`,
    );
  }
  if (tag !== undefined && tag !== `v${authoritative}`) {
    throw new Error(`release tag ${tag} does not match application version ${authoritative}`);
  }
  return authoritative;
}

export function assertClean(root = repoRoot) {
  const status = execFileSync("git", ["status", "--porcelain"], {
    cwd: root,
    encoding: "utf8",
  });
  if (status.trim()) throw new Error("release source tree is dirty");
}

function parseArgs(argv) {
  const result = { tag: undefined, requireClean: false };
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === "--tag") {
      const tag = argv[i + 1];
      if (!tag) throw new Error("--tag requires a value");
      result.tag = tag;
      i += 1;
    } else if (argv[i] === "--require-clean") {
      result.requireClean = true;
    } else {
      throw new Error(`unknown argument: ${argv[i]}`);
    }
  }
  return result;
}

export function main(argv = process.argv.slice(2)) {
  const args = parseArgs(argv);
  const version = validateVersions(collectVersions(), args.tag);
  if (args.requireClean) assertClean();
  process.stdout.write(`${version}\n`);
}

const invokedPath = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : "";
if (import.meta.url === invokedPath) {
  try {
    main();
  } catch (error) {
    console.error(`release version validation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
