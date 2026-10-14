use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::{linux::net::SocketAddrExt, unix::fs::MetadataExt},
    path::Path,
};
use tokio::net::UnixStream;

/// Directory identity, not spelling: aliases and bind mounts share one address.
/// No UID or protocol version in the name: neither may split directory ownership.
pub(crate) fn address(data_dir: &Path) -> Result<std::os::unix::net::SocketAddr> {
    let metadata = fs::metadata(data_dir).context("Reading IPC data directory identity")?;
    ensure!(metadata.is_dir(), "IPC data path is not a directory");
    let mut name = [0u8; 21];
    name[..5].copy_from_slice(b"rivu.");
    name[5..13].copy_from_slice(&metadata.dev().to_be_bytes());
    name[13..21].copy_from_slice(&metadata.ino().to_be_bytes());
    Ok(std::os::unix::net::SocketAddr::from_abstract_name(name)?)
}

pub(crate) fn verify_peer(stream: &UnixStream) -> Result<()> {
    let peer = stream.peer_cred().context("Reading IPC peer credentials")?;
    // SAFETY: geteuid takes no arguments and cannot fail.
    let uid = unsafe { libc::geteuid() };
    ensure!(peer.uid() == uid, "IPC peer belongs to a different user");
    Ok(())
}

pub(crate) async fn connect(data_dir: &Path) -> Result<UnixStream> {
    let stream = UnixStream::connect_addr(&address(data_dir)?.into()).await?;
    verify_peer(&stream)?;
    Ok(stream)
}
