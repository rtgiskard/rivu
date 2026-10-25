//! Cross-process ownership shared by the host and offline migration tool.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::{linux::net::SocketAddrExt, unix::fs::MetadataExt},
    path::Path,
};

/// Directory identity, not spelling: aliases and bind mounts share one address.
/// Keep these bytes stable across protocol and application versions.
pub fn data_directory_address(data_dir: &Path) -> Result<std::os::unix::net::SocketAddr> {
    let metadata = fs::metadata(data_dir).context("Reading IPC data directory identity")?;
    ensure!(metadata.is_dir(), "IPC data path is not a directory");
    let mut name = [0u8; 21];
    name[..5].copy_from_slice(b"rivu.");
    name[5..13].copy_from_slice(&metadata.dev().to_be_bytes());
    name[13..21].copy_from_slice(&metadata.ino().to_be_bytes());
    Ok(std::os::unix::net::SocketAddr::from_abstract_name(name)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::symlink, net::UnixListener};

    #[test]
    fn directory_aliases_share_existing_abstract_socket_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let alias = directory.path().join("alias");
        symlink(directory.path(), &alias).unwrap();
        let _owner =
            UnixListener::bind_addr(&data_directory_address(directory.path()).unwrap()).unwrap();
        assert!(UnixListener::bind_addr(&data_directory_address(&alias).unwrap()).is_err());
    }
}
