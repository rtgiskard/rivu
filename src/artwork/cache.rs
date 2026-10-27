use std::path::{Path, PathBuf};

/// Originals use the first 128 bits of the content hash, split into one bucket.
pub(crate) fn original_path(cache_root: &Path, hash: blake3::Hash, extension: &str) -> PathBuf {
    let hex = hash.to_hex();
    cache_root
        .join(&hex[..2])
        .join(format!("{}.{extension}", &hex[2..32]))
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
    }
}
