import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

import { parse as parseToml } from "smol-toml";

const CRATES_IO_SOURCE = "registry+https://github.com/rust-lang/crates.io-index";
const CRATES_IO_INDEX = "https://index.crates.io";
const FETCH_TIMEOUT_MS = 30_000;
const FETCH_ATTEMPTS = 2;
const MAX_BUFFER_BYTES = 64 * 1024 * 1024;
const CONCURRENCY = 4;
const REPOSITORY_ROOT = resolve(import.meta.dirname, "..");
const WORKSPACE_LABEL_LIMIT = 48;

const VERSION_PATTERN =
  /^(\d+)(?:\.(\d+))?(?:\.(\d+))?(?:-([0-9A-Za-z.-]+))?(?:\+([0-9A-Za-z.-]+))?$/;

export function parseVersion(text) {
  const match = VERSION_PATTERN.exec(text.trim());
  if (match === null) return null;
  return {
    text: text.trim(),
    major: Number(match[1]),
    minor: Number(match[2] ?? 0),
    patch: Number(match[3] ?? 0),
    pre: match[4] ?? "",
  };
}

export function parseRequirementBase(requirement) {
  const match = /\d+(?:\.\d+){0,2}(?:-[0-9A-Za-z.-]+)?/.exec(requirement);
  return match === null ? null : parseVersion(match[0]);
}

export function compareVersions(left, right) {
  for (const key of ["major", "minor", "patch"]) {
    if (left[key] !== right[key]) return left[key] < right[key] ? -1 : 1;
  }
  if (left.pre === right.pre) return 0;
  if (left.pre === "") return 1;
  if (right.pre === "") return -1;

  const leftParts = left.pre.split(".");
  const rightParts = right.pre.split(".");
  for (let index = 0; index < Math.max(leftParts.length, rightParts.length); index += 1) {
    const leftPart = leftParts[index];
    const rightPart = rightParts[index];
    if (leftPart === undefined) return -1;
    if (rightPart === undefined) return 1;
    if (leftPart === rightPart) continue;
    const leftNumeric = /^\d+$/.test(leftPart);
    const rightNumeric = /^\d+$/.test(rightPart);
    if (leftNumeric && rightNumeric) return Number(leftPart) < Number(rightPart) ? -1 : 1;
    if (leftNumeric !== rightNumeric) return leftNumeric ? -1 : 1;
    return leftPart < rightPart ? -1 : 1;
  }
  return 0;
}

export function updateType(current, latest) {
  if (latest.major !== current.major) return "major";
  if (latest.minor !== current.minor) return "minor";
  if (latest.patch !== current.patch) return "patch";
  if (latest.pre !== current.pre) return "prerelease";
  return null;
}

export function sparseIndexPath(name) {
  const lower = name.toLowerCase();
  if (lower.length === 1) return `1/${lower}`;
  if (lower.length === 2) return `2/${lower}`;
  if (lower.length === 3) return `3/${lower[0]}/${lower}`;
  return `${lower.slice(0, 2)}/${lower.slice(2, 4)}/${lower}`;
}

export function selectLatest(entries, current) {
  const allowPrerelease = current.pre !== "";
  let best = null;
  for (const entry of entries) {
    if (entry.yanked === true) continue;
    const version = parseVersion(String(entry.vers));
    if (version === null) continue;
    if (version.pre !== "" && !allowPrerelease) continue;
    if (best === null || compareVersions(version, best.version) > 0) {
      best = {
        version,
        rustVersion:
          typeof entry.rust_version === "string" ? entry.rust_version : null,
      };
    }
  }
  return best;
}

export function collectDirectDependencies(metadata) {
  const dependencies = new Map();
  for (const pkg of metadata.packages ?? []) {
    for (const dependency of pkg.dependencies ?? []) {
      if (dependency.source !== CRATES_IO_SOURCE) continue;
      const entry = dependencies.get(dependency.name) ?? {
        name: dependency.name,
        requirements: new Set(),
        members: new Set(),
      };
      entry.requirements.add(dependency.req);
      entry.members.add(pkg.name);
      dependencies.set(dependency.name, entry);
    }
  }
  return [...dependencies.values()].sort((left, right) =>
    left.name.localeCompare(right.name),
  );
}

function compareRustVersions(left, right) {
  const leftParts = String(left).split(".").map(Number);
  const rightParts = String(right).split(".").map(Number);
  for (let index = 0; index < 3; index += 1) {
    const leftPart = Number.isFinite(leftParts[index]) ? leftParts[index] : 0;
    const rightPart = Number.isFinite(rightParts[index]) ? rightParts[index] : 0;
    if (leftPart !== rightPart) return leftPart < rightPart ? -1 : 1;
  }
  return 0;
}

export function workspaceLabel(members, limit = WORKSPACE_LABEL_LIMIT) {
  const names = [...members].sort();
  let label = "";
  for (let index = 0; index < names.length; index += 1) {
    const candidate = label === "" ? names[index] : `${label}, ${names[index]}`;
    if (candidate.length > limit && label !== "") {
      return `${label}, +${names.length - index} more`;
    }
    label = candidate;
  }
  return label;
}

export function buildRows(dependencies, indexByName, { rustVersion = null } = {}) {
  const rows = [];
  for (const dependency of dependencies) {
    const requirements = [...dependency.requirements].sort();
    const bases = requirements.map(parseRequirementBase).filter((base) => base !== null);
    if (bases.length === 0) continue;
    const current = bases.reduce((left, right) =>
      compareVersions(left, right) >= 0 ? left : right,
    );
    const entries = indexByName.get(dependency.name);
    if (entries === undefined) {
      throw new Error(`No crates.io index data for ${dependency.name}`);
    }
    const latest = selectLatest(entries, current);
    if (latest === null || compareVersions(latest.version, current) <= 0) continue;

    const type = updateType(current, latest.version);
    const notes = [];
    if (current.major === 0 && (type === "minor" || type === "major")) {
      notes.push("0.x breaking");
    }
    if (
      latest.rustVersion !== null &&
      rustVersion !== null &&
      compareRustVersions(latest.rustVersion, rustVersion) > 0
    ) {
      notes.push(`requires Rust ${latest.rustVersion}`);
    }
    rows.push({
      package: dependency.name,
      workspace: workspaceLabel(dependency.members),
      current: requirements.join(" / "),
      latest: latest.version.text,
      update: type,
      note: notes.join("; "),
    });
  }
  return rows;
}

export function formatTable(rows) {
  if (rows.length === 0) return "All direct Rust dependencies are up to date.\n";
  const withNotes = rows.some((row) => row.note !== "");
  const columns = [
    ["Package", "package"],
    ["Workspace", "workspace"],
    ["Current", "current"],
    ["Latest", "latest"],
    ["Update", "update"],
    ...(withNotes ? [["Note", "note"]] : []),
  ];
  const widths = columns.map(([heading, key]) =>
    Math.max(heading.length, ...rows.map((row) => row[key].length)),
  );
  const lines = [
    columns.map(([heading], index) => heading.padEnd(widths[index])).join("  "),
    widths.map((width) => "─".repeat(width)).join("  "),
  ];
  for (const row of rows) {
    lines.push(
      columns.map(([, key], index) => row[key].padEnd(widths[index])).join("  "),
    );
  }
  return `${lines.join("\n")}\n`;
}

function loadCargoMetadata() {
  const cargo = process.env.CARGO || "cargo";
  const result = spawnSync(
    cargo,
    ["metadata", "--no-deps", "--format-version", "1", "--locked"],
    { encoding: "utf8", maxBuffer: MAX_BUFFER_BYTES, timeout: 120_000 },
  );
  if (result.error) {
    throw new Error(`Unable to run ${cargo} metadata: ${result.error.message}`);
  }
  if (result.status !== 0) {
    throw new Error(
      `${cargo} metadata failed:\n${result.stderr || result.stdout || ""}`.trimEnd(),
    );
  }
  try {
    return JSON.parse(result.stdout);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`Unable to parse cargo metadata output: ${message}`);
  }
}

function loadWorkspaceRustVersion(root = REPOSITORY_ROOT) {
  const manifest = parseToml(readFileSync(resolve(root, "Cargo.toml"), "utf8"));
  const rustVersion = manifest.workspace?.package?.["rust-version"];
  return typeof rustVersion === "string" ? rustVersion : null;
}

async function fetchIndexEntries(name, { fetchImpl = fetch } = {}) {
  const url = `${CRATES_IO_INDEX}/${sparseIndexPath(name)}`;
  let lastError = null;
  for (let attempt = 1; attempt <= FETCH_ATTEMPTS; attempt += 1) {
    try {
      const response = await fetchImpl(url, {
        headers: { "user-agent": "nian-vision-outdated-check" },
        signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
      });
      if (response.status === 404) {
        throw new Error(`${name} is not published on crates.io`);
      }
      if (!response.ok) {
        lastError = new Error(`${name}: crates.io index returned HTTP ${response.status}`);
        continue;
      }
      const body = await response.text();
      return body
        .split("\n")
        .filter((line) => line.trim() !== "")
        .map((line) => JSON.parse(line));
    } catch (error) {
      lastError = error instanceof Error ? error : new Error(String(error));
    }
  }
  throw lastError ?? new Error(`Unable to fetch crates.io index data for ${name}`);
}

async function mapWithConcurrency(items, limit, mapper) {
  const results = new Array(items.length);
  let next = 0;
  async function worker() {
    while (next < items.length) {
      const index = next;
      next += 1;
      results[index] = await mapper(items[index], index);
    }
  }
  await Promise.all(
    Array.from({ length: Math.min(limit, items.length) }, () => worker()),
  );
  return results;
}

export async function runOutdated({
  metadata = loadCargoMetadata,
  indexEntries = fetchIndexEntries,
  loadRustVersion = loadWorkspaceRustVersion,
  output = process.stdout,
  errorOutput = process.stderr,
} = {}) {
  try {
    const manifest = metadata();
    const dependencies = collectDirectDependencies(manifest);
    if (dependencies.length === 0) {
      output.write(formatTable([]));
      return 0;
    }

    const indexByName = new Map();
    await mapWithConcurrency(dependencies, CONCURRENCY, async (dependency) => {
      indexByName.set(dependency.name, await indexEntries(dependency.name));
    });

    const rows = buildRows(dependencies, indexByName, {
      rustVersion: loadRustVersion(),
    });
    output.write(formatTable(rows));
    return 0;
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    errorOutput.write(`${message}\n`);
    return 1;
  }
}

const isMain =
  process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href;
if (isMain) process.exitCode = await runOutdated();
