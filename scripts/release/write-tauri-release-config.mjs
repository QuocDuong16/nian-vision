import { existsSync, writeFileSync } from "node:fs";
import { dirname, relative, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

import { validateHttpsAuthority } from "./release-authority.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const appDir = resolve(root, "apps/nian-desktop");
const stage = resolve(root, "dist/linux-x86_64");
const target = "x86_64-unknown-linux-gnu";

function requireEnv(name) {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`${name} is required for a release updater build`);
  return value;
}

export function validateEndpoint(raw) {
  return validateHttpsAuthority(raw, "NIAN_UPDATER_ENDPOINT").toString();
}

function fromApp(path) {
  return relative(appDir, path).split(sep).join("/");
}

function requireFile(path, label) {
  if (!existsSync(path)) throw new Error(`${label} is missing: ${path}`);
}

export function createReleaseConfig({ endpoint, pubkey, workerBase, appimageFiles, createUpdaterArtifacts = false }) {
  return {
    // Frontend assets are built before the signing step. Keeping this hook
    // empty prevents Vite from inheriting TAURI_SIGNING_* at bundle time.
    build: {
      beforeBuildCommand: "",
    },
    bundle: {
      targets: ["appimage"],
      createUpdaterArtifacts,
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
}

export function generateReleaseConfig() {
  const endpoint = validateEndpoint(requireEnv("NIAN_UPDATER_ENDPOINT"));
  const pubkey = requireEnv("NIAN_UPDATER_PUBLIC_KEY");
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

  const config = createReleaseConfig({
    endpoint,
    pubkey,
    workerBase,
    appimageFiles,
    createUpdaterArtifacts: process.env.NIAN_CREATE_UPDATER_ARTIFACTS === "true",
  });
  const output = resolve(appDir, "tauri.release.generated.conf.json");
  writeFileSync(output, `${JSON.stringify(config, null, 2)}
`);
  return output;
}

if (process.argv[1] && pathToFileURL(resolve(process.argv[1])).href === import.meta.url) {
  try {
    const output = generateReleaseConfig();
    process.stdout.write(`${output}
`);
  } catch (error) {
    console.error(`Tauri release configuration generation failed: ${error.message}`);
    process.exitCode = 1;
  }
}
