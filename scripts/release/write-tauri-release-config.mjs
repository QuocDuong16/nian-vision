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

export function createAppImageRuntimeFiles(stageRoot = stage) {
  return {
    "/usr/share/doc/nian-vision/THIRD_PARTY_NOTICES.txt": resolve(stageRoot, "THIRD_PARTY_NOTICES.txt"),
    "/usr/share/doc/nian-vision/FFMPEG-LGPL-2.1.txt": resolve(stageRoot, "FFMPEG-LGPL-2.1.txt"),
    "/usr/share/doc/nian-vision/FFMPEG_BUILD_FLAGS.txt": resolve(stageRoot, "FFMPEG_BUILD_FLAGS.txt"),
    "/usr/share/doc/nian-vision/FFMPEG_CONFIG.h": resolve(stageRoot, "FFMPEG_CONFIG.h"),
    "/usr/share/nian-vision/BUILD_METADATA.json": resolve(stageRoot, "BUILD_METADATA.json"),
    // libappindicator-sys loads the tray library with dlopen, so linuxdeploy cannot
    // discover it from the desktop binary. Seed the Bookworm-built tray library
    // into AppDir explicitly; linuxdeploy then deploys its matching dependency
    // closure alongside the GTK/GLib runtime already bundled into the AppImage.
    "/usr/lib/libayatana-appindicator3.so.1": resolve(stageRoot, "lib/appimage/libayatana-appindicator3.so.1"),
    "/usr/lib/nian-vision/libavformat.so.62": resolve(stageRoot, "lib/nian-vision/libavformat.so.62"),
    "/usr/lib/nian-vision/libavcodec.so.62": resolve(stageRoot, "lib/nian-vision/libavcodec.so.62"),
    "/usr/lib/nian-vision/libavutil.so.60": resolve(stageRoot, "lib/nian-vision/libavutil.so.60"),
  };
}

export function generateReleaseConfig() {
  const endpoint = validateEndpoint(requireEnv("NIAN_UPDATER_ENDPOINT"));
  const pubkey = requireEnv("NIAN_UPDATER_PUBLIC_KEY");
  const workerBase = resolve(stage, "tauri/nian-media-worker");
  requireFile(`${workerBase}-${target}`, "Tauri worker sidecar staging binary");

  const runtimeFiles = createAppImageRuntimeFiles();
  for (const [destination, source] of Object.entries(runtimeFiles)) {
    requireFile(source, destination);
  }

  const appimageFiles = {};
  for (const [destination, source] of Object.entries(runtimeFiles)) {
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
