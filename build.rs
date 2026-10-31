use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-env-changed=RIVU_GIT_VERSION");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let version = env::var("RIVU_GIT_VERSION")
        .ok()
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION"));
    println!("cargo:rustc-env=RIVU_GIT_VERSION={version}");

    println!("cargo:rustc-env=RIVU_BUILD_TIMESTAMP={}", build_time());

    let features = [
        (cfg!(feature = "ffmpeg"), "ffmpeg"),
        (cfg!(feature = "onnx"), "onnx"),
        (cfg!(feature = "embed-nerd-font"), "embed-nerd-font"),
    ]
    .into_iter()
    .filter_map(|(enabled, name)| enabled.then_some(name))
    .collect::<Vec<_>>()
    .join(", ");
    println!(
        "cargo:rustc-env=RIVU_BUILD_FEATURES={}",
        if features.is_empty() {
            "none"
        } else {
            &features
        }
    );

    #[cfg(feature = "embed-nerd-font")]
    embed_nerd_font();
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
