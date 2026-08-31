import { existsSync, writeFileSync } from "node:fs";
import { dirname, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const appDir = resolve(root, "apps/nian-desktop");
const stage = resolve(root, "dist/linux-x86_64");
const target = "x86_64-unknown-linux-gnu";

function requireEnv(name) {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`${name} is required for a release updater build`);
  return value;
}

function validateEndpoint(raw) {
  const url = new URL(raw);
  if (url.protocol !== "https:") throw new Error("NIAN_UPDATER_ENDPOINT must use HTTPS");
  const host = url.hostname.toLowerCase();
  if (
    host === "localhost" ||
    host === "127.0.0.1" ||
    host.endsWith(".localhost") ||
    host === "example.com" ||
    host.endsWith(".example.com")
  ) {
    throw new Error("NIAN_UPDATER_ENDPOINT is not a production update authority");
  }
  return url.toString();
}

function fromApp(path) {
  return relative(appDir, path).split(sep).join("/");
}

function requireFile(path, label) {
  if (!existsSync(path)) throw new Error(`${label} is missing: ${path}`);
}

try {
  const endpoint = validateEndpoint(requireEnv("NIAN_UPDATER_ENDPOINT"));
  const pubkey = requireEnv("NIAN_UPDATER_PUBLIC_KEY");
  if (process.env.PRODUCTION_RELEASE === "true") {
    requireEnv("TAURI_SIGNING_PRIVATE_KEY");
    requireEnv("TAURI_SIGNING_PRIVATE_KEY_PASSWORD");
  }

  const workerBase = resolve(stage, "tauri/nian-media-worker");
  requireFile(`${workerBase}-${target}`, "Tauri worker sidecar staging binary");

  const docs = {
    "/usr/share/doc/nian-vision/THIRD_PARTY_NOTICES.txt": resolve(stage, "THIRD_PARTY_NOTICES.txt"),
    "/usr/share/doc/nian-vision/FFMPEG-LGPL-2.1.txt": resolve(stage, "FFMPEG-LGPL-2.1.txt"),
    "/usr/share/doc/nian-vision/FFMPEG_BUILD_FLAGS.txt": resolve(stage, "FFMPEG_BUILD_FLAGS.txt"),
    "/usr/share/doc/nian-vision/FFMPEG_CONFIG.h": resolve(stage, "FFMPEG_CONFIG.h"),
    "/usr/share/nian-vision/BUILD_METADATA.json": resolve(stage, "BUILD_METADATA.json"),
  };
  const libraries = {
    "/usr/lib/libavformat.so.62": resolve(stage, "lib/nian-vision/libavformat.so.62"),
    "/usr/lib/libavcodec.so.62": resolve(stage, "lib/nian-vision/libavcodec.so.62"),
    "/usr/lib/libavutil.so.60": resolve(stage, "lib/nian-vision/libavutil.so.60"),
  };
  for (const [destination, source] of Object.entries({ ...docs, ...libraries })) {
    requireFile(source, destination);
  }

  const appimageFiles = {};
  for (const [destination, source] of Object.entries({ ...docs, ...libraries })) {
    appimageFiles[destination] = fromApp(source);
  }

  const config = {
    bundle: {
      targets: ["appimage"],
      createUpdaterArtifacts: true,
      externalBin: [fromApp(workerBase)],
      linux: {
        appimage: {
          bundleMediaFramework: false,
          files: appimageFiles,
        },
      },
    },
    plugins: {
      updater: {
        pubkey,
        endpoints: [endpoint],
      },
    },
  };

  const output = resolve(appDir, "tauri.release.generated.conf.json");
  writeFileSync(output, `${JSON.stringify(config, null, 2)}
`);
  process.stdout.write(`${output}
`);
} catch (error) {
  console.error(`Tauri release configuration generation failed: ${error.message}`);
  process.exitCode = 1;
}
