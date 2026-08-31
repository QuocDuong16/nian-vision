import { readFileSync, readdirSync, statSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

function filesUnder(path) {
  const stat = statSync(path);
  if (stat.isFile()) return [path];
  if (!stat.isDirectory()) return [];
  return readdirSync(path).flatMap((entry) => filesUnder(resolve(path, entry)));
}

export function scanPaths(paths, sentinel) {
  if (!sentinel) return { scanned: 0, skipped: true };
  const needle = Buffer.from(sentinel, "utf8");
  let scanned = 0;
  for (const input of paths) {
    for (const path of filesUnder(resolve(input))) {
      scanned += 1;
      const contents = readFileSync(path);
      if (contents.indexOf(needle) !== -1) {
        throw new Error(`release secret sentinel detected in ${path}`);
      }
    }
  }
  return { scanned, skipped: false };
}

function main() {
  const paths = process.argv.slice(2);
  if (paths.length === 0) throw new Error("at least one release path is required");
  const result = scanPaths(paths, process.env.NIAN_RELEASE_SECRET_SENTINEL ?? "");
  if (result.skipped) {
    process.stdout.write("release secret sentinel is not configured; scan skipped\n");
  } else {
    process.stdout.write(`release secret sentinel scan passed across ${result.scanned} files\n`);
  }
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  try {
    main();
  } catch (error) {
    console.error(`release secret scan failed: ${error.message}`);
    process.exitCode = 1;
  }
}
