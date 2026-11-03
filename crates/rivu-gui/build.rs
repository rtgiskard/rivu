fn main() {
    println!("cargo:rerun-if-changed=build.rs");
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
