//! Generates `crates/nian-ffmpeg-sys/src/bindings.rs` from the vendored
//! FFmpeg headers under `thirdparty/`.
//!
//! Run explicitly (requires libclang):
//!
//! ```text
//! cargo run -p bindgen-gen
//! ```
//!
//! Normal builds never invoke bindgen; they compile the committed output.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const HEADER_FILES: &[&str] = &[
    "libavutil/avutil.h",
    "libavutil/dict.h",
    "libavutil/error.h",
    "libavutil/rational.h",
    "libavcodec/avcodec.h",
    "libavcodec/defs.h",
    "libavcodec/packet.h",
    "libavcodec/codec_par.h",
    "libavcodec/codec_id.h",
    "libavformat/avformat.h",
];

const ALLOWLISTED_FUNCTIONS: &[&str] = &[
    "avformat_version",
    "avformat_configuration",
    "avformat_license",
    "avformat_network_init",
    "avformat_network_deinit",
    "avformat_alloc_context",
    "avformat_free_context",
    "avformat_open_input",
    "avformat_close_input",
    "avformat_find_stream_info",
    "avformat_alloc_output_context2",
    "avformat_free_context",
    "avformat_write_header",
    "avformat_new_stream",
    "av_write_trailer",
    "av_interleaved_write_frame",
    "av_guess_format",
    "av_read_frame",
    "avio_open",
    "avio_open2",
    "avio_closep",
    "av_log_set_level",
    "av_log_get_level",
    "av_packet_alloc",
    "av_packet_free",
    "av_packet_unref",
    "av_packet_rescale_ts",
    "avcodec_parameters_copy",
    "avcodec_get_name",
    "avcodec_version",
    "avutil_version",
    "av_strerror",
    "av_dict_get",
    "av_dict_set",
    "av_dict_free",
    "av_dict_count",
    "av_get_media_type_string",
    "av_rescale_q",
    "av_rescale_q_rnd",
    "av_reduce",
];

const ALLOWLISTED_TYPES: &[&str] = &[
    "AVFormatContext",
    "AVStream",
    "AVInputFormat",
    "AVCodecParameters",
    "AVPacket",
    "AVDictionary",
    "AVDictionaryEntry",
    "AVRational",
    "AVIOInterruptCB",
    "AVIOContext",
    "AVClass",
    "AVMediaType",
    "AVCodecID",
    "AVColorRange",
    "AVRounding",
];

const ALLOWLISTED_VARS: &[&str] = &[
    "NIAN_AV_NOPTS_VALUE",
    "NIAN_AVERROR_EOF",
    "LIBAVFORMAT_VERSION_MAJOR",
    "LIBAVCODEC_VERSION_MAJOR",
    "LIBAVUTIL_VERSION_MAJOR",
    "AV_TIME_BASE",
    "AV_ERROR_MAX_STRING_SIZE",
    "AV_PKT_FLAG_KEY",
    "AV_PKT_FLAG_CORRUPT",
    "AV_PKT_FLAG_DISCARD",
    "AVFMT_NOFILE",
    "AVFMT_FLAG_NONBLOCK",
    "AVFMT_GLOBALHEADER",
    "AVIO_FLAG_READ",
    "AVIO_FLAG_WRITE",
    "AVIO_FLAG_READ_WRITE",
    "AV_LOG_QUIET",
    "AV_DICT_MATCH_CASE",
    "AV_DICT_APPEND",
    "AV_ROUND_NEAR_INF",
    "AV_ROUND_PASS_MINMAX",
];

fn main() -> ExitCode {
    let repo_root: PathBuf = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .map(|bin_dir| {
            // target/<profile>/deps/... walk up until Cargo.lock is found.
            let mut dir = bin_dir;
            loop {
                if dir.join("Cargo.lock").exists() {
                    return dir;
                }
                match dir.parent() {
                    Some(parent) => dir = parent.to_path_buf(),
                    None => break,
                }
            }
            std::env::current_dir().expect("current directory")
        })
        .unwrap_or_else(|| std::env::current_dir().expect("current directory"));

    let include_root = repo_root.join("thirdparty").join("ffmpeg");
    let output_path = repo_root
        .join("crates")
        .join("nian-ffmpeg-sys")
        .join("src")
        .join("bindings.rs");

    if !include_root.join("libavformat").join("avformat.h").exists() {
        eprintln!(
            "FFmpeg headers not found under {}.\nRun scripts/fetch-ffmpeg-headers.sh first.",
            include_root.display()
        );
        return ExitCode::FAILURE;
    }

    let mut builder = bindgen::Builder::default()
        .clang_arg(format!("-I{}", include_root.display()))
        .clang_arg(format!(
            "-I{}",
            repo_root.join("tools").join("bindgen-gen").display()
        ))
        .header("shim.h")
        .opaque_type("AVIOContext")
        .opaque_type("AVOutputFormat")
        .opaque_type("AVCodec")
        .opaque_type("AVClass")
        .constified_enum("AVMediaType")
        .constified_enum("AVCodecID")
        .constified_enum("AVRounding")
        .constified_enum("AVColorRange")
        .layout_tests(false)
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false);

    for header in HEADER_FILES {
        builder = builder.header(include_root.join(header).to_string_lossy().as_ref());
    }
    for function in ALLOWLISTED_FUNCTIONS {
        builder = builder.allowlist_function(function);
    }
    for ty in ALLOWLISTED_TYPES {
        builder = builder.allowlist_type(ty);
    }
    for variable in ALLOWLISTED_VARS {
        builder = builder.allowlist_var(variable);
    }

    let bindings = match builder.generate() {
        Ok(bindings) => bindings,
        Err(error) => {
            eprintln!("bindgen failed: {error}");
            return ExitCode::FAILURE;
        }
    };

    if let Err(error) = bindings.write_to_file(&output_path) {
        eprintln!("cannot write {}: {error}", output_path.display());
        return ExitCode::FAILURE;
    }

    println!("wrote {}", output_path.display());
    ExitCode::SUCCESS
}
