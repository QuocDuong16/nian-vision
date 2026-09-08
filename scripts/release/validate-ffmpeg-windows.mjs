import { basename, join, relative, resolve, sep } from "node:path";
import { lstatSync, readFileSync, readdirSync } from "node:fs";

import {
  buildWindowsFfmpegContract,
  loadWindowsFfmpegContractInputs,
  validateWindowsFfmpegFlags,
  validateWindowsFfmpegMetadata,
} from "./ffmpeg-cache-key.mjs";
import { validateFfmpegConfiguration } from "./validate-ffmpeg-config.mjs";

function normalizeRelative(path) {
  return path.split(sep).join("/");
}

function walkFiles(root, current = root, files = []) {
  for (const entry of readdirSync(current, { withFileTypes: true })) {
    const full = join(current, entry.name);
    const stat = lstatSync(full);
    if (stat.isSymbolicLink()) throw new Error(`symbolic link is forbidden in cached FFmpeg output: ${entry.name}`);
    if (entry.isDirectory()) walkFiles(root, full, files);
    else if (entry.isFile()) files.push(full);
  }
  return files;
}

function requireRegularFile(path, label) {
  const stat = lstatSync(path);
  if (!stat.isFile() || stat.isSymbolicLink()) throw new Error(`${label} is not a regular file`);
}

function macrosFromConfig(configHeader) {
  const macros = new Map();
  for (const line of configHeader.replace(/\r\n?/g, "\n").split("\n")) {
    const match = line.match(/^#define\s+(CONFIG_[A-Z0-9_]+)\s+([01])$/);
    if (match) macros.set(match[1], match[2]);
  }
  return macros;
}

export function requiredComponentMacros(configureFlags) {
  const result = new Set();
  const families = new Map([
    ["--enable-protocol=", "PROTOCOL"],
    ["--enable-demuxer=", "DEMUXER"],
    ["--enable-muxer=", "MUXER"],
    ["--enable-parser=", "PARSER"],
    ["--enable-decoder=", "DECODER"],
  ]);
  for (const flag of configureFlags) {
    for (const [prefix, suffix] of families) {
      if (!flag.startsWith(prefix)) continue;
      for (const component of flag.slice(prefix.length).split(",")) {
        result.add(`CONFIG_${component.toUpperCase().replaceAll("-", "_")}_${suffix}`);
      }
    }
  }
  return [...result].sort();
}

function assertNoPrivatePathLeak(evidencePaths) {
  const candidates = [
    process.env.GITHUB_WORKSPACE,
    process.env.RUNNER_TEMP,
    process.env.USERPROFILE,
    process.env.HOME,
  ].filter((value) => typeof value === "string" && value.length >= 4);
  for (const evidencePath of evidencePaths) {
    const text = readFileSync(evidencePath, "utf8");
    for (const candidate of candidates) {
      if (text.toLowerCase().includes(candidate.toLowerCase())) {
        throw new Error(`FFmpeg release evidence contains a private runner path: ${basename(evidencePath)}`);
      }
    }
  }
}

export function validateWindowsFfmpegOutput(outputDir, inputs = loadWindowsFfmpegContractInputs()) {
  const root = resolve(outputDir);
  const { releaseConfig, windowsContract } = inputs;
  const contract = buildWindowsFfmpegContract(releaseConfig, windowsContract);
  const evidencePaths = contract.required_outputs.evidence.map((name) => join(root, name));
  for (const [index, path] of evidencePaths.entries()) requireRegularFile(path, contract.required_outputs.evidence[index]);

  const configHeader = readFileSync(join(root, "FFMPEG_CONFIG.h"), "utf8");
  const componentHeader = readFileSync(join(root, "FFMPEG_CONFIG_COMPONENTS.h"), "utf8");
  const configureFlags = readFileSync(join(root, "FFMPEG_BUILD_FLAGS.txt"), "utf8");
  validateWindowsFfmpegFlags(configureFlags, windowsContract);
  validateFfmpegConfiguration(configHeader, configureFlags);
  validateWindowsFfmpegMetadata(
    JSON.parse(readFileSync(join(root, "FFMPEG_BUILD_METADATA.json"), "utf8")),
    releaseConfig,
    windowsContract,
  );

  const macros = macrosFromConfig(componentHeader);
  for (const macro of requiredComponentMacros(windowsContract.configureFlags)) {
    if (macros.get(macro) !== "1") throw new Error(`required FFmpeg component is not enabled: ${macro}`);
  }

  const files = walkFiles(root);
  const normalizedFiles = files.map((path) => ({
    path,
    relative: normalizeRelative(relative(root, path)),
    basename: basename(path).toLowerCase(),
  }));
  for (const name of contract.required_outputs.dlls) {
    const matches = normalizedFiles.filter((file) => file.basename === name.toLowerCase());
    if (matches.length !== 1 || matches[0].relative !== `bin/${name}`) {
      throw new Error(`required FFmpeg runtime DLL missing, duplicated, or misplaced: ${name}`);
    }
  }
  for (const name of contract.required_outputs.import_libs) {
    const matches = normalizedFiles.filter((file) => file.basename === name.toLowerCase());
    if (matches.length !== 1 || matches[0].relative !== `lib/${name}`) {
      throw new Error(`required MSVC FFmpeg import library missing, duplicated, or misplaced: ${name}`);
    }
  }
  for (const name of contract.required_outputs.headers) {
    const header = join(root, "include", ...name.split("/"));
    requireRegularFile(header, `required FFmpeg development header ${name}`);
  }

  const forbiddenCli = new Set(["ffmpeg.exe", "ffprobe.exe", "ffplay.exe"]);
  const forbidden = normalizedFiles.find((file) => forbiddenCli.has(file.basename));
  if (forbidden) throw new Error(`FFmpeg CLI program unexpectedly present in Windows release runtime: ${forbidden.relative}`);

  assertNoPrivatePathLeak(evidencePaths);
  return true;
}

function parseArgs(argv) {
  if (argv.length === 2 && argv[0] === "--output-dir") return argv[1];
  throw new Error("usage: validate-ffmpeg-windows.mjs --output-dir <dir>");
}

if (process.argv[1]?.endsWith("validate-ffmpeg-windows.mjs")) {
  try {
    const outputDir = parseArgs(process.argv.slice(2));
    validateWindowsFfmpegOutput(outputDir);
    process.stdout.write("Windows FFmpeg cache/runtime provenance and release contract validated\n");
  } catch (error) {
    console.error(`Windows FFmpeg output validation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
