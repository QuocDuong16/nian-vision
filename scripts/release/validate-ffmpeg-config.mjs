import { readFileSync } from "node:fs";

export function validateFfmpegConfiguration(configHeader, configureFlags) {
  const forbiddenFlags = ["--enable-gpl", "--enable-nonfree"];
  for (const flag of forbiddenFlags) {
    if (configureFlags.split(/\r?\n/).some((line) => line.trim() === flag)) {
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
    if (!configureFlags.split(/\r?\n/).some((line) => line.trim() === flag)) {
      throw new Error(`required FFmpeg release flag is missing: ${flag}`);
    }
  }

  const macros = new Map();
  for (const line of configHeader.split(/\r?\n/)) {
    const match = line.match(/^#define\s+(CONFIG_[A-Z0-9_]+)\s+([01])$/);
    if (match) macros.set(match[1], match[2]);
  }
  for (const macro of ["CONFIG_GPL", "CONFIG_NONFREE"]) {
    if (macros.get(macro) !== "0") {
      throw new Error(`${macro} must be disabled in the shipped FFmpeg build`);
    }
  }
  for (const macro of ["CONFIG_SHARED"]) {
    if (macros.get(macro) !== "1") {
      throw new Error(`${macro} must be enabled in the shipped FFmpeg build`);
    }
  }
  for (const macro of ["CONFIG_CBS_APV_LAVF", "CONFIG_CBS_AV1_LAVF"]) {
    if (macros.get(macro) !== "1") {
      throw new Error(`${macro} must be enabled by the pinned FFmpeg CBS-in-lavf backport`);
    }
  }
  return true;
}

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) {
    if (!["--config-header", "--flags"].includes(argv[i]) || !argv[i + 1]) {
      throw new Error("usage: validate-ffmpeg-config.mjs --config-header <config.h> --flags <flags.txt>");
    }
    args[argv[i].slice(2)] = argv[i + 1];
  }
  return args;
}

if (process.argv[1]?.endsWith("validate-ffmpeg-config.mjs")) {
  try {
    const args = parseArgs(process.argv.slice(2));
    validateFfmpegConfiguration(
      readFileSync(args["config-header"], "utf8"),
      readFileSync(args.flags, "utf8"),
    );
    process.stdout.write("FFmpeg license/runtime configuration validated\n");
  } catch (error) {
    console.error(`FFmpeg configuration validation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
