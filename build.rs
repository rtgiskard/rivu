#[cfg(feature = "ffmpeg")]
use std::collections::BTreeSet;
use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");

    let git_dir = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|path| PathBuf::from(path.trim()))
        .unwrap_or_else(|| PathBuf::from(".git"));
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD"))
        && let Some(reference) = head.strip_prefix("ref: ").map(str::trim)
    {
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join(reference).display()
        );
    }
    println!("cargo:rerun-if-env-changed=RIVU_GIT_VERSION");
    let git_version = env::var("RIVU_GIT_VERSION").ok().or_else(|| {
        Command::new("git")
            .args(["describe", "--long", "--tags", "--dirty", "--always"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|version| version.trim().to_owned())
    });
    println!(
        "cargo:rustc-env=RIVU_GIT_VERSION={}",
        git_version.as_deref().unwrap_or("unknown")
    );
    #[cfg(feature = "ffmpeg")]
    if env::var_os("CARGO_FEATURE_FFMPEG").is_some() {
        generate_ffmpeg_bindings();
    }
}

#[cfg(feature = "ffmpeg")]
fn generate_ffmpeg_bindings() {
    let target = env::var("TARGET").expect("Cargo TARGET");
    let supported = (target.starts_with("x86_64-") || target.starts_with("aarch64-"))
        && target.contains("-linux");
    assert!(
        supported,
        "FFmpeg extension is validated only for little-endian x86_64/aarch64 Linux"
    );

    let mut includes = BTreeSet::new();
    for (name, major) in [("libavutil", 61), ("libavcodec", 63), ("libavformat", 63)] {
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
