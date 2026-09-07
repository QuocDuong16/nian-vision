import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";

const releaseConfigUrl = new URL("./release-config.json", import.meta.url);
const windowsContractUrl = new URL("./ffmpeg-windows-contract.json", import.meta.url);

function readJson(url) {
  return JSON.parse(readFileSync(url, "utf8"));
}

export function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value && typeof value === "object") {
    const entries = Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`);
    return `{${entries.join(",")}}`;
  }
  return JSON.stringify(value);
}

export function sha256Hex(value) {
  return createHash("sha256").update(value, "utf8").digest("hex");
}

export function buildWindowsFfmpegContract(releaseConfig, windowsContract) {
  return {
    schema: "nian-vision.windows-ffmpeg-build-contract.v1",
    build_contract_version: windowsContract.buildContractVersion,
    output_runtime_contract_version: windowsContract.outputRuntimeContractVersion,
    ffmpeg: {
      version: releaseConfig.ffmpegVersion,
      source_sha256: releaseConfig.ffmpegSourceSha256,
      source_url: releaseConfig.ffmpegSourceUrl,
      libavformat_major: releaseConfig.libavformatMajor,
      libavcodec_major: releaseConfig.libavcodecMajor,
      libavutil_major: releaseConfig.libavutilMajor,
    },
    target: {
      triple: releaseConfig.windowsTarget,
      platform: releaseConfig.windowsPlatform,
      architecture: windowsContract.architecture,
      toolchain: windowsContract.toolchain,
    },
    msys_build_tools: { ...windowsContract.msysBuildTools },
    provisioned_msys_packages: Object.fromEntries(
      Object.entries(windowsContract.provisionedMsysPackages ?? {}).map(([name, pkg]) => [
        name,
        { ...pkg },
      ]),
    ),
    configure_flags: [...windowsContract.configureFlags],
    required_outputs: {
      dlls: [
        `avformat-${releaseConfig.libavformatMajor}.dll`,
        `avcodec-${releaseConfig.libavcodecMajor}.dll`,
        `avutil-${releaseConfig.libavutilMajor}.dll`,
      ],
      import_libs: ["avformat.lib", "avcodec.lib", "avutil.lib"],
      headers: [
        "libavformat/avformat.h",
        "libavcodec/avcodec.h",
        "libavutil/avutil.h",
      ],
      evidence: [
        "FFMPEG_CONFIG.h",
        "FFMPEG_CONFIG_COMPONENTS.h",
        "FFMPEG_BUILD_FLAGS.txt",
        "FFMPEG-LGPL-2.1.txt",
        "FFMPEG_BUILD_METADATA.json",
      ],
    },
  };
}

export function buildContractDigest(releaseConfig, windowsContract) {
  return sha256Hex(canonicalJson(buildWindowsFfmpegContract(releaseConfig, windowsContract)));
}

export function expectedWindowsFfmpegMetadata(releaseConfig, windowsContract) {
  const contract = buildWindowsFfmpegContract(releaseConfig, windowsContract);
  return {
    ffmpeg_version: releaseConfig.ffmpegVersion,
    source_sha256: releaseConfig.ffmpegSourceSha256,
    source_url: releaseConfig.ffmpegSourceUrl,
    target: releaseConfig.windowsTarget,
    toolchain: windowsContract.toolchain,
    configure_flags_sha256: sha256Hex(windowsContract.configureFlags.join("\n")),
    build_contract_sha256: sha256Hex(canonicalJson(contract)),
    build_contract_version: windowsContract.buildContractVersion,
    output_runtime_contract_version: windowsContract.outputRuntimeContractVersion,
  };
}

export function validateWindowsFfmpegMetadata(actual, releaseConfig, windowsContract) {
  const expected = expectedWindowsFfmpegMetadata(releaseConfig, windowsContract);
  if (canonicalJson(actual) !== canonicalJson(expected)) {
    throw new Error("FFmpeg build metadata does not match the current Windows build contract");
  }
  return true;
}

export function validateWindowsFfmpegFlags(actualFlags, windowsContract) {
  const normalized = actualFlags.replace(/\r\n?/g, "\n");
  const expected = windowsContract.configureFlags.join("\n");
  if (normalized !== expected) {
    throw new Error("FFmpeg configure flags do not exactly match the Windows build contract");
  }
  return true;
}

export function loadWindowsFfmpegContractInputs() {
  return {
    releaseConfig: readJson(releaseConfigUrl),
    windowsContract: readJson(windowsContractUrl),
  };
}

function parseCli(argv) {
  if (argv.length === 0) return { mode: "digest" };
  if (argv.length === 2 && argv[0] === "--write-metadata") return { mode: "write", path: argv[1] };
  if (argv.length === 2 && argv[0] === "--validate-metadata") return { mode: "validate-metadata", path: argv[1] };
  if (argv.length === 2 && argv[0] === "--validate-flags") return { mode: "validate-flags", path: argv[1] };
  throw new Error(
    "usage: ffmpeg-cache-key.mjs [--write-metadata <path> | --validate-metadata <path> | --validate-flags <path>]",
  );
}

if (process.argv[1]?.endsWith("ffmpeg-cache-key.mjs")) {
  try {
    const cli = parseCli(process.argv.slice(2));
    const { releaseConfig, windowsContract } = loadWindowsFfmpegContractInputs();
    if (cli.mode === "digest") {
      process.stdout.write(`${buildContractDigest(releaseConfig, windowsContract)}\n`);
    } else if (cli.mode === "write") {
      const metadata = expectedWindowsFfmpegMetadata(releaseConfig, windowsContract);
      writeFileSync(cli.path, `${JSON.stringify(metadata, null, 2)}\n`, "utf8");
    } else if (cli.mode === "validate-metadata") {
      validateWindowsFfmpegMetadata(JSON.parse(readFileSync(cli.path, "utf8")), releaseConfig, windowsContract);
      process.stdout.write("FFmpeg build metadata matches the current Windows build contract\n");
    } else {
      validateWindowsFfmpegFlags(readFileSync(cli.path, "utf8"), windowsContract);
      process.stdout.write("FFmpeg configure flags match the current Windows build contract\n");
    }
  } catch (error) {
    console.error(`Windows FFmpeg build contract validation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
