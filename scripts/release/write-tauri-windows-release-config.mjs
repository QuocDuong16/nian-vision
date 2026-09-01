import { existsSync, writeFileSync } from "node:fs";
import { dirname, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { validateHttpsAuthority } from "./release-authority.mjs";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const appDir = resolve(root, "apps/nian-desktop");
const stage = resolve(root, "dist/windows-x86_64");
const releaseConfig = JSON.parse(await import("node:fs").then(({ readFileSync }) =>
  readFileSync(resolve(root, "scripts/release/release-config.json"), "utf8")));

function requireEnv(name) {
  const value = process.env[name]?.trim();
  if (!value) throw new Error(`${name} is required for a release updater build`);
  return value;
}

function fromApp(path) {
  return relative(appDir, path).split(sep).join("/");
}

function requireFile(path, label) {
  if (!existsSync(path)) throw new Error(`${label} is missing: ${path}`);
}

try {
  const endpoint = validateHttpsAuthority(requireEnv("NIAN_UPDATER_ENDPOINT"), "NIAN_UPDATER_ENDPOINT").toString();
  const pubkey = requireEnv("NIAN_UPDATER_PUBLIC_KEY");
  const target = releaseConfig.windowsTarget;
  const workerBase = resolve(stage, "tauri/nian-media-worker");
  requireFile(`${workerBase}-${target}.exe`, "Tauri Windows worker sidecar staging binary");

  const resources = {};
  const runtime = resolve(stage, "runtime");
  for (const name of ["avformat-62.dll", "avcodec-62.dll", "avutil-60.dll"]) {
    const source = resolve(runtime, name);
    requireFile(source, name);
    resources[fromApp(source)] = name;
  }
  for (const entry of await import("node:fs").then(({ readdirSync }) => readdirSync(runtime))) {
    if (/^(?:VCRUNTIME|MSVCP|CONCRT)[0-9A-Z_]*\.dll$/i.test(entry)) {
      resources[fromApp(resolve(runtime, entry))] = entry;
    }
  }
  for (const name of [
    "THIRD_PARTY_NOTICES.txt",
    "FFMPEG-LGPL-2.1.txt",
    "FFMPEG_BUILD_FLAGS.txt",
    "FFMPEG_CONFIG.h",
    "BUILD_METADATA.json",
  ]) {
    const source = resolve(stage, name);
    requireFile(source, name);
    resources[fromApp(source)] = `release-evidence/${name}`;
  }

  const config = {
    build: {
      beforeBuildCommand: "",
    },
    bundle: {
      targets: ["nsis"],
      createUpdaterArtifacts: false,
      externalBin: [fromApp(workerBase)],
      resources,
      windows: {
        webviewInstallMode: { type: "downloadBootstrapper" },
        nsis: {
          installMode: "currentUser",
          installerHooks: "windows/nsis-hooks.nsh",
        },
      },
    },
    plugins: {
      updater: {
        pubkey,
        endpoints: [endpoint],
        windows: { installMode: "passive" },
      },
    },
  };

  const serialized = JSON.stringify(config, null, 2);
  for (const forbidden of ["TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD", "WINDOWS_SIGNING_PFX"]) {
    if (serialized.includes(forbidden)) throw new Error(`generated Windows config contains forbidden private field: ${forbidden}`);
  }
  const output = resolve(appDir, "tauri.windows.release.generated.conf.json");
  writeFileSync(output, `${serialized}\n`);
  process.stdout.write(`${output}\n`);
} catch (error) {
  console.error(`Tauri Windows release configuration generation failed: ${error.message}`);
  process.exitCode = 1;
}
