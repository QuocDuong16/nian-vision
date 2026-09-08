import { readFileSync } from "node:fs";

const componentFamilies = [
  { prefix: "--enable-protocol=", family: "protocol", suffix: "PROTOCOL" },
  { prefix: "--enable-demuxer=", family: "demuxer", suffix: "DEMUXER" },
  { prefix: "--enable-muxer=", family: "muxer", suffix: "MUXER" },
  { prefix: "--enable-parser=", family: "parser", suffix: "PARSER" },
  { prefix: "--enable-decoder=", family: "decoder", suffix: "DECODER" },
];

function normalizedFlags(configureFlags) {
  const flags = Array.isArray(configureFlags)
    ? configureFlags
    : configureFlags.replace(/\r\n?/g, "\n").split("\n");
  return flags.map((flag) => flag.trim()).filter(Boolean);
}

function macroFor(component, suffix) {
  return `CONFIG_${component.toUpperCase().replaceAll("-", "_")}_${suffix}`;
}

export function requestedComponents(configureFlags) {
  const requested = [];
  for (const flag of normalizedFlags(configureFlags)) {
    for (const { prefix, family, suffix } of componentFamilies) {
      if (!flag.startsWith(prefix)) continue;
      for (const name of flag.slice(prefix.length).split(",").map((value) => value.trim()).filter(Boolean)) {
        requested.push({ family, name, macro: macroFor(name, suffix) });
      }
    }
  }
  return requested;
}

export function requiredComponentMacros(configureFlags) {
  return [...new Set(requestedComponents(configureFlags).map(({ macro }) => macro))].sort();
}

export function macrosFromComponentHeader(componentHeader) {
  const macros = new Map();
  for (const line of componentHeader.replace(/\r\n?/g, "\n").split("\n")) {
    const match = line.match(/^#define\s+(CONFIG_[A-Z0-9_]+)\s+([01])$/);
    if (match) macros.set(match[1], match[2]);
  }
  return macros;
}

export function validateRequestedComponents(componentHeader, configureFlags) {
  const macros = macrosFromComponentHeader(componentHeader);
  for (const component of requestedComponents(configureFlags)) {
    const actual = macros.get(component.macro);
    if (actual !== "1") {
      throw new Error(
        `requested FFmpeg component is not enabled: family=${component.family} name=${component.name} ` +
          `expected=${component.macro} actual=${actual ?? "<missing>"}`,
      );
    }
  }
  return true;
}

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    if (!["--component-header", "--flags"].includes(argv[i]) || !argv[i + 1]) {
      throw new Error(
        "usage: validate-ffmpeg-components.mjs --component-header <config_components.h> --flags <flags.txt>",
      );
    }
    args[argv[i].slice(2)] = argv[i + 1];
  }
  if (!args["component-header"] || !args.flags) {
    throw new Error(
      "usage: validate-ffmpeg-components.mjs --component-header <config_components.h> --flags <flags.txt>",
    );
  }
  return args;
}

if (process.argv[1]?.endsWith("validate-ffmpeg-components.mjs")) {
  try {
    const args = parseArgs(process.argv.slice(2));
    validateRequestedComponents(
      readFileSync(args["component-header"], "utf8"),
      readFileSync(args.flags, "utf8"),
    );
    process.stdout.write("FFmpeg requested component configuration validated\n");
  } catch (error) {
    console.error(`FFmpeg component validation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
