import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

const UNKNOWN = Buffer.from("__TAURI_BUNDLE_TYPE_VAR_UNK", "ascii");
const TYPES = new Map([
  ["nsis", Buffer.from("__TAURI_BUNDLE_TYPE_VAR_NSS", "ascii")],
]);

function countOccurrences(buffer, needle) {
  let count = 0;
  let offset = 0;
  while (true) {
    const index = buffer.indexOf(needle, offset);
    if (index < 0) return count;
    count += 1;
    offset = index + needle.length;
  }
}

try {
  const [rawPath, rawType] = process.argv.slice(2);
  if (!rawPath || !rawType) {
    throw new Error("usage: patch-tauri-bundle-type.mjs <binary> <nsis>");
  }

  const replacement = TYPES.get(rawType.toLowerCase());
  if (!replacement) throw new Error(`unsupported Tauri bundle type: ${rawType}`);
  if (replacement.length !== UNKNOWN.length) {
    throw new Error("Tauri bundle type replacement must preserve the binary token length");
  }

  const binaryPath = resolve(rawPath);
  const bytes = readFileSync(binaryPath);
  const occurrences = countOccurrences(bytes, UNKNOWN);
  if (occurrences !== 1) {
    throw new Error(`expected exactly one unpatched Tauri bundle type token, found ${occurrences}`);
  }

  const index = bytes.indexOf(UNKNOWN);
  replacement.copy(bytes, index);
  writeFileSync(binaryPath, bytes);

  const verified = readFileSync(binaryPath);
  if (countOccurrences(verified, UNKNOWN) !== 0 || !verified.subarray(index, index + replacement.length).equals(replacement)) {
    throw new Error("Tauri bundle type patch verification failed");
  }
  process.stdout.write(`patched Tauri bundle type to ${rawType.toLowerCase()} for ${binaryPath}\n`);
} catch (error) {
  console.error(`Tauri bundle type patch failed: ${error.message}`);
  process.exitCode = 1;
}
