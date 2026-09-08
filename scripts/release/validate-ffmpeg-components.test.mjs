import assert from "node:assert/strict";
import test from "node:test";

import {
  requiredComponentMacros,
  requestedComponents,
  validateRequestedComponents,
} from "./validate-ffmpeg-components.mjs";

const correctedFlags = [
  "--enable-protocol=file,tcp,rtp,udp",
  "--enable-demuxer=matroska,mov,rtsp",
  "--enable-muxer=matroska,mov,mp4",
  "--enable-parser=h264,mpeg4video,mpegaudio,aac",
  "--enable-decoder=mpeg4,aac",
];

const resolvedComponentHeader = [
  "#define CONFIG_FILE_PROTOCOL 1",
  "#define CONFIG_TCP_PROTOCOL 1",
  "#define CONFIG_RTP_PROTOCOL 1",
  "#define CONFIG_UDP_PROTOCOL 1",
  "#define CONFIG_HTTP_PROTOCOL 1",
  "#define CONFIG_MATROSKA_DEMUXER 1",
  "#define CONFIG_MOV_DEMUXER 1",
  "#define CONFIG_RTSP_DEMUXER 1",
  "#define CONFIG_ASF_DEMUXER 1",
  "#define CONFIG_RM_DEMUXER 1",
  "#define CONFIG_MPEGTS_DEMUXER 1",
  "#define CONFIG_MATROSKA_MUXER 1",
  "#define CONFIG_MOV_MUXER 1",
  "#define CONFIG_MP4_MUXER 1",
  "#define CONFIG_H264_PARSER 1",
  "#define CONFIG_MPEG4VIDEO_PARSER 1",
  "#define CONFIG_MPEGAUDIO_PARSER 1",
  "#define CONFIG_AAC_PARSER 1",
  "#define CONFIG_MPEG4_DECODER 1",
  "#define CONFIG_AAC_DECODER 1",
  "",
].join("\n");

test("corrected RC15 direct component contract validates without CONFIG_RTSP_PROTOCOL", () => {
  const macros = new Set(requiredComponentMacros(correctedFlags));
  assert.equal(macros.has("CONFIG_RTSP_PROTOCOL"), false);
  assert.equal(macros.has("CONFIG_RTSP_DEMUXER"), true);
  assert.deepEqual(
    requestedComponents(["--enable-protocol=file,tcp,rtp,udp"]).map(({ macro }) => macro),
    ["CONFIG_FILE_PROTOCOL", "CONFIG_TCP_PROTOCOL", "CONFIG_RTP_PROTOCOL", "CONFIG_UDP_PROTOCOL"],
  );
  assert.equal(validateRequestedComponents(resolvedComponentHeader, correctedFlags), true);
});

test("transitive resolved components are allowed without becoming direct requirements", () => {
  const macros = new Set(requiredComponentMacros(correctedFlags));
  for (const transitive of [
    "CONFIG_HTTP_PROTOCOL",
    "CONFIG_ASF_DEMUXER",
    "CONFIG_RM_DEMUXER",
    "CONFIG_MPEGTS_DEMUXER",
  ]) {
    assert.equal(macros.has(transitive), false);
  }
  assert.equal(validateRequestedComponents(resolvedComponentHeader, correctedFlags), true);
});

test("synthetic nonexistent RTSP protocol request fails against resolved components", () => {
  assert.throws(
    () => validateRequestedComponents(resolvedComponentHeader, ["--enable-protocol=rtsp"]),
    /family=protocol name=rtsp expected=CONFIG_RTSP_PROTOCOL actual=<missing>/,
  );
});

test("missing real requested protocol macros fail closed", () => {
  for (const macro of ["CONFIG_TCP_PROTOCOL", "CONFIG_RTP_PROTOCOL", "CONFIG_UDP_PROTOCOL"]) {
    const header = resolvedComponentHeader.replace(`#define ${macro} 1\n`, "");
    assert.throws(() => validateRequestedComponents(header, correctedFlags), new RegExp(`expected=${macro} actual=<missing>`));
  }
});

test("disabled real requested protocol macro reports actual zero", () => {
  const header = resolvedComponentHeader.replace("CONFIG_TCP_PROTOCOL 1", "CONFIG_TCP_PROTOCOL 0");
  assert.throws(
    () => validateRequestedComponents(header, correctedFlags),
    /family=protocol name=tcp expected=CONFIG_TCP_PROTOCOL actual=0/,
  );
});

test("missing requested RTSP demuxer fails closed", () => {
  const header = resolvedComponentHeader.replace("#define CONFIG_RTSP_DEMUXER 1\n", "");
  assert.throws(
    () => validateRequestedComponents(header, correctedFlags),
    /family=demuxer name=rtsp expected=CONFIG_RTSP_DEMUXER actual=<missing>/,
  );
});

test("every requested component family remains enforced", () => {
  for (const [family, macro] of [
    ["protocol", "CONFIG_FILE_PROTOCOL"],
    ["demuxer", "CONFIG_MATROSKA_DEMUXER"],
    ["muxer", "CONFIG_MP4_MUXER"],
    ["parser", "CONFIG_H264_PARSER"],
    ["decoder", "CONFIG_AAC_DECODER"],
  ]) {
    const header = resolvedComponentHeader.replace(`#define ${macro} 1\n`, "");
    assert.throws(
      () => validateRequestedComponents(header, correctedFlags),
      new RegExp(`family=${family} .*expected=${macro} actual=<missing>`),
    );
  }
});
