#[cfg(feature = "ffmpeg")]
use std::collections::BTreeSet;
use std::env;
#[cfg(feature = "ffmpeg")]
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");
    println!("cargo:rerun-if-env-changed=RIVU_GIT_VERSION");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        pkg_config::Config::new()
            .atleast_version("1.2")
            .probe("libpipewire-0.3")
            .expect("PipeWire 1.2 or newer development files required");
    }

    let version = env::var("RIVU_GIT_VERSION")
        .ok()
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION"));
    println!("cargo:rustc-env=RIVU_GIT_VERSION={version}");

    println!("cargo:rustc-env=RIVU_BUILD_TIMESTAMP={}", build_time());

    let features = [
        (cfg!(feature = "ffmpeg"), "ffmpeg"),
        (cfg!(feature = "embed-nerd-font"), "embed-nerd-font"),
    ]
    .into_iter()
    .filter_map(|(enabled, name)| enabled.then_some(name))
    .collect::<Vec<_>>()
    .join(", ");
    println!(
        "cargo:rustc-env=RIVU_BUILD_FEATURES={}",
        if features.is_empty() { "none" } else { &features }
    );

    #[cfg(feature = "embed-nerd-font")]
    embed_nerd_font();

    #[cfg(feature = "ffmpeg")]
    if env::var_os("CARGO_FEATURE_FFMPEG").is_some() {
        generate_ffmpeg_bindings();
    }
}

#[cfg(feature = "embed-nerd-font")]
fn embed_nerd_font() {
    println!("cargo:rerun-if-env-changed=FONTCONFIG_FILE");
    println!("cargo:rerun-if-env-changed=FONTCONFIG_PATH");
    let output = std::process::Command::new("fc-list")
        .args(["--format", "%{family}\t%{file}\n"])
        .output()
        .expect("embed-nerd-font requires Fontconfig's fc-list on the build host");
    assert!(
        output.status.success(),
        "fc-list failed while finding a Nerd Font"
    );
    let fonts = String::from_utf8(output.stdout).expect("fc-list returned non-UTF-8 font names");
    let (_, family, path) = fonts
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .flat_map(|(families, path)| {
            families.split(',').map(move |family| (family.trim(), path))
        })
        .filter_map(|(family, path)| {
            let rank = match family {
                "Symbols Nerd Font Mono" => 0,
                "Symbols Nerd Font" => 1,
                _ if family.contains("Nerd Font")
                    || family.contains("NerdFont")
                    || family.ends_with(" NF")
                    || family.ends_with(" NFM") => 2,
                _ => return None,
            };
            Some((rank, family, path))
        })
        .min()
        .expect("embed-nerd-font requires an installed Nerd Font on the build host; install Symbols Nerd Font Mono and rebuild");
    println!("cargo:rerun-if-changed={path}");
    println!("cargo:rustc-env=RIVU_EMBEDDED_SYMBOLS_FONT={path}");
    println!("cargo:rustc-env=RIVU_EMBEDDED_SYMBOLS_FAMILY={family}");
}

fn build_time() -> i64 {
    let timestamp = match env::var_os("SOURCE_DATE_EPOCH") {
        Some(value) => {
            let value = value
                .to_str()
                .expect("SOURCE_DATE_EPOCH must be a nonnegative integer Unix timestamp");
            assert!(
                !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
                "SOURCE_DATE_EPOCH must be a nonnegative integer Unix timestamp"
            );
            let seconds = value
                .parse::<i64>()
                .expect("SOURCE_DATE_EPOCH is outside the supported timestamp range");
            chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)
                .expect("SOURCE_DATE_EPOCH is outside the supported timestamp range")
        }
        None => chrono::Utc::now(),
    };
    timestamp.timestamp()
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
