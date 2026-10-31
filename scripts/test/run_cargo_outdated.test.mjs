import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { test } from "node:test";

import {
  buildRows,
  collectDirectDependencies,
  compareVersions,
  formatTable,
  parseRequirementBase,
  parseVersion,
  selectLatest,
  sparseIndexPath,
  updateType,
  workspaceLabel,
} from "../run_cargo_outdated.mjs";

const repositoryRoot = resolve(import.meta.dirname, "../..");

function dependency(name, req, { source: dependencySource = "registry" } = {}) {
  return {
    name,
    req,
    source:
      dependencySource === "registry"
        ? "registry+https://github.com/rust-lang/crates.io-index"
        : dependencySource,
  };
}

test("sparse index paths follow the crates.io layout", () => {
  assert.equal(sparseIndexPath("a"), "1/a");
  assert.equal(sparseIndexPath("ab"), "2/ab");
  assert.equal(sparseIndexPath("abc"), "3/a/abc");
  assert.equal(sparseIndexPath("clap"), "cl/ap/clap");
  assert.equal(sparseIndexPath("aws-sdk-s3"), "aw/s-/aws-sdk-s3");
  assert.equal(sparseIndexPath("Serde"), "se/rd/serde");
});

test("requirements expose their current version base", () => {
  assert.equal(parseRequirementBase("=4.6.6")?.text, "4.6.6");
  assert.equal(parseRequirementBase("^1.2.3")?.text, "1.2.3");
  assert.equal(parseRequirementBase("~0.1")?.text, "0.1");
  assert.equal(parseRequirementBase(">=0.13.21, <0.14")?.text, "0.13.21");
  assert.equal(parseRequirementBase("*"), null);
});

test("semantic versions order by component and prerelease", () => {
  assert.equal(compareVersions(parseVersion("1.2.3"), parseVersion("1.2.4")), -1);
  assert.equal(compareVersions(parseVersion("1.10.0"), parseVersion("1.9.9")), 1);
  assert.equal(compareVersions(parseVersion("2.0.0"), parseVersion("2.0.0")), 0);
  assert.equal(
    compareVersions(parseVersion("2.0.0-beta"), parseVersion("2.0.0")),
    -1,
  );
  assert.equal(
    compareVersions(parseVersion("2.0.0-alpha"), parseVersion("2.0.0-beta")),
    -1,
  );
});

test("update types distinguish patch, minor, and major", () => {
  assert.equal(updateType(parseVersion("1.2.3"), parseVersion("1.2.4")), "patch");
  assert.equal(updateType(parseVersion("1.2.3"), parseVersion("1.3.0")), "minor");
  assert.equal(updateType(parseVersion("1.2.3"), parseVersion("2.0.0")), "major");
  assert.equal(
    updateType(parseVersion("2.0.0-alpha"), parseVersion("2.0.0-beta")),
    "prerelease",
  );
  assert.equal(updateType(parseVersion("1.2.3"), parseVersion("1.2.3")), null);
});

test("latest selection skips yanked and prerelease versions by default", () => {
  const entries = [
    { vers: "0.9.0", yanked: false },
    { vers: "1.0.0", yanked: false },
    { vers: "1.1.0", yanked: true },
    { vers: "2.0.0-beta", yanked: false },
  ];
  assert.equal(selectLatest(entries, parseVersion("0.9.0"))?.version.text, "1.0.0");
  assert.equal(
    selectLatest(entries, parseVersion("2.0.0-alpha"))?.version.text,
    "2.0.0-beta",
  );
});

test("direct dependency collection groups members and skips non-registry sources", () => {
  const metadata = {
    packages: [
      {
        name: "vault-core",
        dependencies: [
          dependency("clap", "=4.6.6"),
          dependency("local", "*", { source: null }),
          dependency("git-dep", "*", { source: "git+https://example.invalid/repo" }),
        ],
      },
      {
        name: "vault-session",
        dependencies: [dependency("clap", "=4.6.6"), dependency("uuid", "=1.24.1")],
      },
    ],
  };
  const dependencies = collectDirectDependencies(metadata);
  assert.deepEqual(
    dependencies.map((entry) => entry.name),
    ["clap", "uuid"],
  );
  assert.deepEqual([...dependencies[0].requirements], ["=4.6.6"]);
  assert.deepEqual([...dependencies[0].members].sort(), [
    "vault-core",
    "vault-session",
  ]);
});

test("rows report updates with breaking and MSRV notes", () => {
  const dependencies = [
    {
      name: "clap",
      requirements: new Set(["=4.6.6"]),
      members: new Set(["nian-pass-cli"]),
    },
    {
      name: "keepass",
      requirements: new Set(["=0.13.21"]),
      members: new Set(["kdbx"]),
    },
    {
      name: "uuid",
      requirements: new Set(["=1.24.1"]),
      members: new Set(["vault-core", "vault-session"]),
    },
    {
      name: "serde",
      requirements: new Set(["=1.0.229"]),
      members: new Set(["vault-core"]),
    },
  ];
  const indexByName = new Map([
    [
      "clap",
      [
        { vers: "4.6.6", yanked: false, rust_version: "1.85.0" },
        { vers: "4.6.7", yanked: false, rust_version: "1.85.0" },
      ],
    ],
    [
      "keepass",
      [
        { vers: "0.13.21", yanked: false, rust_version: "1.85.1" },
        { vers: "0.17.0", yanked: false, rust_version: "1.89" },
      ],
    ],
    [
      "uuid",
      [
        { vers: "1.24.1", yanked: false },
        { vers: "1.29.0", yanked: false, rust_version: "1.100.0" },
      ],
    ],
    ["serde", [{ vers: "1.0.229", yanked: false }]],
  ]);

  const rows = buildRows(dependencies, indexByName, { rustVersion: "1.99.1" });
  assert.deepEqual(rows, [
    {
      package: "clap",
      workspace: "nian-pass-cli",
      current: "=4.6.6",
      latest: "4.6.7",
      update: "patch",
      note: "",
    },
    {
      package: "keepass",
      workspace: "kdbx",
      current: "=0.13.21",
      latest: "0.17.0",
      update: "minor",
      note: "0.x breaking",
    },
    {
      package: "uuid",
      workspace: "vault-core, vault-session",
      current: "=1.24.1",
      latest: "1.29.0",
      update: "minor",
      note: "requires Rust 1.100.0",
    },
  ]);
});

test("missing index data fails closed", () => {
  assert.throws(
    () =>
      buildRows(
        [
          {
            name: "clap",
            requirements: new Set(["=4.6.6"]),
            members: new Set(["nian-pass-cli"]),
          },
        ],
        new Map(),
      ),
    /No crates\.io index data for clap/,
  );
});

test("workspace labels stay bounded for shared dependencies", () => {
  assert.equal(workspaceLabel(new Set(["vault-session", "kdbx"])), "kdbx, vault-session");
  assert.equal(
    workspaceLabel(new Set(["alpha", "beta", "gamma", "delta"]), 12),
    "alpha, beta, +2 more",
  );
});

test("table rendering keeps columns aligned and notes optional", () => {
  assert.equal(
    formatTable([]),
    "All direct Rust dependencies are up to date.\n",
  );
  const table = formatTable([
    {
      package: "clap",
      workspace: "nian-pass-cli",
      current: "=4.6.6",
      latest: "4.6.7",
      update: "patch",
      note: "",
    },
  ]);
  assert.match(table, /^Package\s+Workspace\s+Current\s+Latest\s+Update\n/m);
  assert.match(table, /clap\s+nian-pass-cli\s+=4\.6\.6\s+4\.6\.7\s+patch\s*$/m);
  assert.doesNotMatch(table, /Note/);
});

test("rust-outdated uses the workspace-aware script", () => {
  const makefile = readFileSync(resolve(repositoryRoot, "Makefile"), "utf8");
  assert.match(
    makefile,
    /rust-outdated: ## [^\n]+\n\tCARGO="\$\(CARGO\)" \$\(MISE\) exec -- node scripts\/run_cargo_outdated\.mjs/,
  );
  assert.doesNotMatch(makefile, /cargo outdated/);
});
