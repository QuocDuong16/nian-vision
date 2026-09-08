import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

const releaseConfigUrl = new URL("./release-config.json", import.meta.url);

export const expectedFfmpegUpstreamPatch = Object.freeze({
  contractVersion: 1,
  repository: "FFmpeg/FFmpeg",
  commit: "6a59c847b50c6bc30630df7fca56ccd6cd8a5a8c",
  subject: "configure: Redo enabling cbs in lavf",
  files: ["configure", "libavformat/Makefile", "libavformat/cbs.h"],
});

export const ffmpegUpstreamTransformations = Object.freeze([
  {
    file: "configure",
    label: "CONFIG_EXTRA CBS lavf selectors",
    preimage: "    cbs_apv\n    cbs_av1\n",
    postimage: "    cbs_apv\n    cbs_apv_lavf\n    cbs_av1\n    cbs_av1_lavf\n",
  },
  {
    file: "configure",
    label: "MOV muxer CBS lavf dependency selection",
    preimage:
      'mov_muxer_select="iso_media iso_writer riffenc rtpenc_chain vp9_superframe_bsf aac_adtstoasc_bsf ac3_parser"',
    postimage:
      'mov_muxer_select="cbs_apv_lavf cbs_av1_lavf iso_media iso_writer riffenc rtpenc_chain vp9_superframe_bsf aac_adtstoasc_bsf ac3_parser"',
  },
  {
    file: "libavformat/Makefile",
    label: "CBS lavf subsystem object rules",
    preimage: [
      "# subsystems",
      "OBJS-$(CONFIG_ISO_MEDIA)                 += isom.o",
    ].join("\n"),
    postimage: [
      "# subsystems",
      "OBJS-$(CONFIG_CBS_APV_LAVF)              += cbs_apv.o cbs.o",
      "OBJS-$(CONFIG_CBS_AV1_LAVF)              += cbs_av1.o cbs.o",
      "OBJS-$(CONFIG_ISO_MEDIA)                 += isom.o",
    ].join("\n"),
  },
  {
    file: "libavformat/Makefile",
    label: "MOV muxer direct CBS object removal",
    preimage: [
      "OBJS-$(CONFIG_MOV_MUXER)                 += movenc.o \\",
      "                                            movenchint.o mov_chan.o rtp.o \\",
      "                                            movenccenc.o movenc_ttml.o rawutils.o \\",
      "                                            apv.o dovi_isom.o evc.o cbs.o cbs_av1.o cbs_apv.o",
      "OBJS-$(CONFIG_MP2_MUXER)                 += rawenc.o",
    ].join("\n"),
    postimage: [
      "OBJS-$(CONFIG_MOV_MUXER)                 += movenc.o \\",
      "                                            movenchint.o mov_chan.o rtp.o \\",
      "                                            movenccenc.o movenc_ttml.o rawutils.o \\",
      "                                            apv.o dovi_isom.o evc.o",
      "OBJS-$(CONFIG_MP2_MUXER)                 += rawenc.o",
    ].join("\n"),
  },
  {
    file: "libavformat/cbs.h",
    label: "CBS lavf header configuration mapping",
    preimage: [
      "#ifndef AVFORMAT_CBS_H",
      "#define AVFORMAT_CBS_H",
      "",
      "#define CBS_PREFIX lavf_cbs",
      "#define CBS_WRITE 0",
      "#define CBS_TRACE 0",
      "#define CBS_H264 0",
    ].join("\n"),
    postimage: [
      "#ifndef AVFORMAT_CBS_H",
      "#define AVFORMAT_CBS_H",
      "",
      '#include "config.h"',
      "",
      "#define CBS_PREFIX lavf_cbs",
      "#define CBS_WRITE 0",
      "#define CBS_TRACE 0",
      "#define CBS_APV CONFIG_CBS_APV_LAVF",
      "#define CBS_AV1 CONFIG_CBS_AV1_LAVF",
      "#define CBS_H264 0",
    ].join("\n"),
  },
]);

function canonicalJson(value) {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value);
}

function countOccurrences(text, needle) {
  let count = 0;
  let offset = 0;
  while (true) {
    const index = text.indexOf(needle, offset);
    if (index < 0) return count;
    count += 1;
    offset = index + needle.length;
  }
}

function readUtf8Lf(path) {
  const decoder = new TextDecoder("utf-8", { fatal: true });
  const text = decoder.decode(readFileSync(path));
  if (text.includes("\r")) throw new Error(`FFmpeg upstream patch source must use LF line endings: ${path}`);
  return text;
}

export function validateFfmpegUpstreamPatchContract(contract) {
  if (canonicalJson(contract) !== canonicalJson(expectedFfmpegUpstreamPatch)) {
    throw new Error(
      `FFmpeg upstream patch provenance must exactly match ${expectedFfmpegUpstreamPatch.repository}@${expectedFfmpegUpstreamPatch.commit}`,
    );
  }
  return contract;
}

export function loadFfmpegUpstreamPatchContract() {
  const config = JSON.parse(readFileSync(releaseConfigUrl, "utf8"));
  return validateFfmpegUpstreamPatchContract(config.ffmpegUpstreamPatch);
}

function assertPostconditions(textByFile) {
  for (const transformation of ffmpegUpstreamTransformations) {
    const text = textByFile.get(transformation.file);
    if (countOccurrences(text, transformation.postimage) !== 1) {
      throw new Error(`FFmpeg upstream patch postcondition failed: ${transformation.label}`);
    }
    if (countOccurrences(text, transformation.preimage) !== 0) {
      throw new Error(`FFmpeg upstream patch left its pristine preimage behind: ${transformation.label}`);
    }
  }

  const configure = textByFile.get("configure");
  for (const selector of ["cbs_apv_lavf", "cbs_av1_lavf"]) {
    if (!configure.includes(`    ${selector}\n`)) throw new Error(`FFmpeg configure is missing ${selector}`);
  }

  const makefile = textByFile.get("libavformat/Makefile");
  for (const line of [
    "OBJS-$(CONFIG_CBS_APV_LAVF)              += cbs_apv.o cbs.o",
    "OBJS-$(CONFIG_CBS_AV1_LAVF)              += cbs_av1.o cbs.o",
  ]) {
    if (!makefile.includes(line)) throw new Error(`FFmpeg libavformat Makefile is missing: ${line}`);
  }

  const cbsHeader = textByFile.get("libavformat/cbs.h");
  for (const line of [
    '#include "config.h"',
    "#define CBS_APV CONFIG_CBS_APV_LAVF",
    "#define CBS_AV1 CONFIG_CBS_AV1_LAVF",
    "#define CBS_H264 0",
    "#define CBS_H265 0",
    "#define CBS_H266 0",
    "#define CBS_JPEG 0",
    "#define CBS_MPEG2 0",
    "#define CBS_VP8 0",
    "#define CBS_VP9 0",
  ]) {
    if (!cbsHeader.includes(line)) throw new Error(`FFmpeg libavformat/cbs.h postcondition is missing: ${line}`);
  }
}

export function verifyFfmpegUpstreamPatchSource(sourceDir) {
  const contract = loadFfmpegUpstreamPatchContract();
  const root = resolve(sourceDir);
  const textByFile = new Map(contract.files.map((file) => [file, readUtf8Lf(join(root, ...file.split("/")))]));
  assertPostconditions(textByFile);
  return true;
}

export function applyFfmpegUpstreamPatches(sourceDir) {
  const contract = loadFfmpegUpstreamPatchContract();
  const root = resolve(sourceDir);
  const textByFile = new Map(contract.files.map((file) => [file, readUtf8Lf(join(root, ...file.split("/")))]));

  for (const transformation of ffmpegUpstreamTransformations) {
    const text = textByFile.get(transformation.file);
    const preimageCount = countOccurrences(text, transformation.preimage);
    const postimageCount = countOccurrences(text, transformation.postimage);
    if (preimageCount !== 1 || postimageCount !== 0) {
      throw new Error(
        `FFmpeg upstream patch preimage must occur exactly once and postimage must be absent before application: ${transformation.label} (preimage=${preimageCount}, postimage=${postimageCount})`,
      );
    }
  }

  for (const transformation of ffmpegUpstreamTransformations) {
    textByFile.set(
      transformation.file,
      textByFile.get(transformation.file).replace(transformation.preimage, transformation.postimage),
    );
  }
  assertPostconditions(textByFile);

  for (const file of contract.files) {
    writeFileSync(join(root, ...file.split("/")), textByFile.get(file), "utf8");
  }
  verifyFfmpegUpstreamPatchSource(root);
  return [...contract.files];
}

function parseArgs(argv) {
  if (argv.length === 2 && argv[0] === "--source-dir" && argv[1]) return argv[1];
  throw new Error("usage: apply-ffmpeg-upstream-patches.mjs --source-dir <extracted-ffmpeg-source>");
}

if (process.argv[1]?.endsWith("apply-ffmpeg-upstream-patches.mjs")) {
  try {
    const sourceDir = parseArgs(process.argv.slice(2));
    const modifiedFiles = applyFfmpegUpstreamPatches(sourceDir);
    process.stdout.write(
      `Applied FFmpeg upstream patch ${expectedFfmpegUpstreamPatch.repository}@${expectedFfmpegUpstreamPatch.commit}: ${expectedFfmpegUpstreamPatch.subject}\n`,
    );
    process.stdout.write(`FFmpeg upstream patch modified files: ${modifiedFiles.join(", ")}\n`);
  } catch (error) {
    console.error(`FFmpeg upstream patch application failed: ${error.message}`);
    process.exitCode = 1;
  }
}
