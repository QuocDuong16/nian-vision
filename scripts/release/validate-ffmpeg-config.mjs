import { readFileSync } from "node:fs";

function configuredFlagSet(configureFlags) {
  return new Set(
    configureFlags
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter(Boolean),
  );
}

function macrosFromConfig(configHeader) {
  const macros = new Map();
  for (const line of configHeader.split(/\r?\n/)) {
    const match = line.match(/^#define\s+(CONFIG_[A-Z0-9_]+)\s+([01])$/);
    if (match) macros.set(match[1], match[2]);
  }
  return macros;
}

export function validateFfmpegGlobalConfiguration(configHeader, configureFlags) {
  const configuredFlags = configuredFlagSet(configureFlags);
  const forbiddenFlags = ["--enable-gpl", "--enable-nonfree"];
  for (const flag of forbiddenFlags) {
    if (configuredFlags.has(flag)) {
      throw new Error(`forbidden FFmpeg license mode requested: ${flag}`);
    }
  }

  const requiredFlags = [
    "--enable-shared",
    "--disable-static",
    "--disable-gpl",
    "--disable-nonfree",
  ];
  for (const flag of requiredFlags) {
    if (!configuredFlags.has(flag)) {
      throw new Error(`required FFmpeg release flag is missing: ${flag}`);
    }
  }

  const macros = macrosFromConfig(configHeader);
  for (const macro of ["CONFIG_GPL", "CONFIG_NONFREE"]) {
    if (macros.get(macro) !== "0") {
      throw new Error(`${macro} must be disabled in the shipped FFmpeg build`);
    }
  }
  if (macros.get("CONFIG_SHARED") !== "1") {
    throw new Error("CONFIG_SHARED must be enabled in the shipped FFmpeg build");
  }
  if (configuredFlags.has("--enable-network") && macros.get("CONFIG_NETWORK") !== "1") {
    throw new Error("CONFIG_NETWORK must be enabled when --enable-network is requested");
  }
  return true;
}

export function validateFfmpegCbsConfiguration(configHeader) {
  const macros = macrosFromConfig(configHeader);
  for (const macro of ["CONFIG_CBS_APV_LAVF", "CONFIG_CBS_AV1_LAVF"]) {
    if (macros.get(macro) !== "1") {
      throw new Error(`${macro} must be enabled by the pinned FFmpeg CBS-in-lavf backport`);
    }
  }
  return true;
}

export function validateFfmpegConfiguration(configHeader, configureFlags) {
  validateFfmpegGlobalConfiguration(configHeader, configureFlags);
  validateFfmpegCbsConfiguration(configHeader);
  return true;
}

function parseArgs(argv) {
  const args = { scope: "all" };
  for (let i = 0; i < argv.length; i += 2) {
    if (!["--config-header", "--flags", "--scope"].includes(argv[i]) || !argv[i + 1]) {
      throw new Error(
        "usage: validate-ffmpeg-config.mjs --config-header <config.h> --flags <flags.txt> [--scope all|global|cbs]",
      );
    }
    args[argv[i].slice(2)] = argv[i + 1];
  }
  if (!args["config-header"] || !args.flags || !["all", "global", "cbs"].includes(args.scope)) {
    throw new Error(
      "usage: validate-ffmpeg-config.mjs --config-header <config.h> --flags <flags.txt> [--scope all|global|cbs]",
    );
  }
  return args;
}

if (process.argv[1]?.endsWith("validate-ffmpeg-config.mjs")) {
  try {
    const args = parseArgs(process.argv.slice(2));
    const configHeader = readFileSync(args["config-header"], "utf8");
    const configureFlags = readFileSync(args.flags, "utf8");
    if (args.scope === "global") validateFfmpegGlobalConfiguration(configHeader, configureFlags);
    else if (args.scope === "cbs") validateFfmpegCbsConfiguration(configHeader);
    else validateFfmpegConfiguration(configHeader, configureFlags);
    process.stdout.write(`FFmpeg ${args.scope} configuration validated\n`);
  } catch (error) {
    console.error(`FFmpeg configuration validation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
