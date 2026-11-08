use std::env;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-env-changed=RIVU_GIT_VERSION");
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!(
        "cargo:rustc-env=RIVU_DEVELOPMENT_RESOURCES={}",
        development_resources().display()
    );

    let version = env::var("RIVU_GIT_VERSION")
        .ok()
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| env::var("CARGO_PKG_VERSION").expect("Cargo sets CARGO_PKG_VERSION"));
    println!("cargo:rustc-env=RIVU_GIT_VERSION={version}");

    println!("cargo:rustc-env=RIVU_BUILD_TIMESTAMP={}", build_time());

    let features = [
        (cfg!(feature = "gui"), "gui"),
        (cfg!(feature = "tui"), "tui"),
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
}

// OUT_DIR is <target>/<profile>/build/<package>/out, also with --target.
// Store only the relative relationship, never the build machine's checkout path.
fn development_resources() -> std::path::PathBuf {
    let output = std::path::PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let profile = output.ancestors().nth(3).expect("Cargo build directory");
    let source = std::path::PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("Cargo sets CARGO_MANIFEST_DIR"),
    )
    .join("tools/audio_processing");
    let shared = profile
        .components()
        .zip(source.components())
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = std::path::PathBuf::new();
    for _ in profile.components().skip(shared) {
        relative.push("..");
    }
    for component in source.components().skip(shared) {
        relative.push(component.as_os_str());
    }
    relative
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
