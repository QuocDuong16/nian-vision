import { spawnSync } from "node:child_process";

const pnpm = process.env.PNPM || "pnpm";
const result = spawnSync(pnpm, ["outdated", "--recursive", "--json"], {
  encoding: "utf8",
});

if (result.error) {
  console.error(`Unable to run ${pnpm}: ${result.error.message}`);
  process.exit(127);
}

if (result.signal) {
  if (result.stdout) process.stdout.write(result.stdout);
  console.error(`${pnpm} outdated was interrupted by ${result.signal}.`);
  process.exit(1);
}

if (result.stderr) {
  process.stderr.write(result.stderr);
}

let report;
try {
  report = JSON.parse(result.stdout);
} catch {
  if (result.stdout) process.stdout.write(result.stdout);
  process.exit(result.status || 1);
}

const validReport =
  report !== null &&
  typeof report === "object" &&
  !Array.isArray(report) &&
  Object.values(report).every(
    (dependency) =>
      dependency !== null &&
      typeof dependency === "object" &&
      typeof dependency.current === "string" &&
      typeof dependency.wanted === "string" &&
      typeof dependency.latest === "string",
  );

if (!validReport) {
  if (result.stdout) process.stdout.write(result.stdout);
  process.exit(result.status || 1);
}

const rows = Object.entries(report).map(([name, dependency]) => ({
  name,
  current: dependency.current ?? "—",
  wanted: dependency.wanted ?? "—",
  latest: dependency.latest ?? "—",
  workspace: dependency.dependentPackages?.map(({ name }) => name).join(", ") ?? "root",
}));

if (result.status !== 0 && !(result.status === 1 && rows.length > 0)) {
  process.stdout.write(result.stdout);
  process.exit(result.status ?? 1);
}

if (rows.length === 0) {
  console.log("All pnpm workspace dependencies are up to date.");
  process.exit(0);
}

const columns = [
  ["Package", "name"],
  ["Workspace", "workspace"],
  ["Current", "current"],
  ["Wanted", "wanted"],
  ["Latest", "latest"],
];
const widths = columns.map(([heading, key]) =>
  Math.max(heading.length, ...rows.map((row) => row[key].length)),
);

console.log(columns.map(([heading], index) => heading.padEnd(widths[index])).join("  "));
console.log(widths.map((width) => "─".repeat(width)).join("  "));
for (const row of rows) {
  console.log(columns.map(([, key], index) => row[key].padEnd(widths[index])).join("  "));
}
