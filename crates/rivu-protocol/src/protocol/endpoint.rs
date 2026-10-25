use anyhow::{Context, Result, ensure};
use std::path::Path;
use tokio::net::UnixStream;

/// Directory identity, not spelling: aliases and bind mounts share one address.
/// No UID or protocol version in the name: neither may split directory ownership.
pub fn address(data_dir: &Path) -> Result<std::os::unix::net::SocketAddr> {
    rivu_format::ownership::data_directory_address(data_dir)
}

pub fn verify_peer(stream: &UnixStream) -> Result<()> {
    let peer = stream.peer_cred().context("Reading IPC peer credentials")?;
    // SAFETY: geteuid takes no arguments and cannot fail.
    let uid = unsafe { libc::geteuid() };
    ensure!(peer.uid() == uid, "IPC peer belongs to a different user");
    Ok(())
}

pub async fn connect(data_dir: &Path) -> Result<UnixStream> {
    let stream = UnixStream::connect_addr(&address(data_dir)?.into()).await?;
    verify_peer(&stream)?;
    Ok(stream)
}
