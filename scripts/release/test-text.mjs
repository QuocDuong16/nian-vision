import { readFileSync } from "node:fs";

export function normalizeNewlines(text) {
  return text.replace(/\r\n?/g, "\n");
}

export function readNormalizedText(path) {
  return normalizeNewlines(readFileSync(path, "utf8"));
}
