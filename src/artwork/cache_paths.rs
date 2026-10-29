use std::path::{Path, PathBuf};

/// Originals use the first 128 bits of the content hash, split into one bucket.
pub(crate) fn original_path(cache_root: &Path, hash: blake3::Hash, extension: &str) -> PathBuf {
    let hex = hash.to_hex();
    cache_root
        .join(&hex[..2])
        .join(format!("{}.{extension}", &hex[2..32]))
}

/// A thumbnail lives beside its source and retains only its pixel-size suffix.
pub(crate) fn thumbnail_path(source: &Path, hash: blake3::Hash, tier: u32) -> Option<PathBuf> {
    let hex = hash.to_hex();
    Some(
        source
            .parent()?
            .join(format!("{}-{tier}.webp", &hex[2..32])),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_names_use_only_the_first_128_hash_bits() {
        let mut bytes = [0xab; 32];
        bytes[16..].fill(0xcd);
        let hash = blake3::Hash::from_bytes(bytes);
        let root = Path::new("artwork/cache");
        let source = original_path(root, hash, "png");
        assert_eq!(
            source,
            root.join("ab").join("ababababababababababababababab.png")
        );
        for tier in [256, 512, 1024] {
            assert_eq!(
                thumbnail_path(&source, hash, tier),
                Some(
                    root.join("ab")
                        .join(format!("ababababababababababababababab-{tier}.webp"))
                )
            );
        }
        assert_eq!(thumbnail_path(Path::new("/"), hash, 256), None);
    }
}
