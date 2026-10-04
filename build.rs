use std::{collections::BTreeSet, env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");
    let target = env::var("TARGET").expect("Cargo TARGET");
    assert!(
        target.starts_with("x86_64-") && target.contains("linux"),
        "FFmpeg ABI support is currently validated only for x86_64 Linux"
    );

    let mut includes = BTreeSet::new();
    for (name, major) in [("libavutil", 61), ("libavcodec", 63), ("libavformat", 63)] {
        // Locate development headers only; do not emit linker instructions.
        let package = pkg_config::Config::new()
            .cargo_metadata(false)
            .range_version(format!("{major}").as_str()..format!("{}", major + 1).as_str())
            .probe(name)
            .unwrap_or_else(|error| {
                panic!("FFmpeg {name} {major}.x development files required: {error}")
            });
        includes.extend(package.include_paths);
    }
    let mut builder = bindgen::Builder::default()
        .header_contents("rivu_ffmpeg.h", "\
#include <libavutil/avutil.h>\n\
#include <libavutil/frame.h>\n\
#include <libavutil/channel_layout.h>\n\
#include <libavcodec/avcodec.h>\n\
#include <libavformat/avformat.h>\n\
#if LIBAVUTIL_VERSION_MAJOR != 61 || LIBAVCODEC_VERSION_MAJOR != 63 || LIBAVFORMAT_VERSION_MAJOR != 63\n\
#error Unsupported FFmpeg header ABI: expected 61/63/63\n\
#endif\n")
        .clang_arg(format!("--target={target}"))
        // Recursively retain complete layouts for structures we actually read.
        // Pointer-only handles do not need generated field accessors.
        .allowlist_type("AV(FormatContext|InputFormat|Dictionary|DictionaryEntry|Packet|Codec|CodecID|CodecContext|CodecParameters|Frame|Rational|ChannelLayout|Channel|ChannelOrder|MediaType|Discard|SampleFormat)")
        .allowlist_type("AVStreamGroup")
        .opaque_type("AVCodecContext|AVInputFormat|AVStreamGroup")
        .allowlist_var("AV_TIME_BASE")
        .allowlist_var("LIBAV(UTIL|CODEC|FORMAT)_VERSION_MAJOR")
        .allowlist_function("^$")
        .layout_tests(false)
        .derive_debug(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()));
    for include in includes {
        builder = builder.clang_arg(format!("-I{}", include.display()));
    }
    builder
        .generate()
        .expect("Generating FFmpeg ABI bindings")
        .write_to_file(
            PathBuf::from(env::var_os("OUT_DIR").expect("Cargo OUT_DIR"))
                .join("ffmpeg_bindings.rs"),
        )
        .expect("Writing FFmpeg ABI bindings");
}
