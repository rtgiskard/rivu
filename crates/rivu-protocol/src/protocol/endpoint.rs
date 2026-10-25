use anyhow::{Context, Result, ensure};
use async_io::Async;
use std::{
    os::{
        fd::{AsFd, AsRawFd},
        linux::net::SocketAddrExt,
        unix::{ffi::OsStrExt, net::UnixStream},
    },
    path::Path,
};

/// Directory identity, not spelling: aliases and bind mounts share one address.
/// No UID or protocol version in the name: neither may split directory ownership.
pub fn address(data_dir: &Path) -> Result<std::os::unix::net::SocketAddr> {
    rivu_format::ownership::data_directory_address(data_dir)
}

pub fn verify_peer(stream: &impl AsFd) -> Result<()> {
    let mut peer = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: the fd remains borrowed for the call, and peer/length point to
    // writable storage of the size passed to getsockopt.
    let result = unsafe {
        libc::getsockopt(
            stream.as_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            peer.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result == -1 {
        return Err(std::io::Error::last_os_error()).context("Reading IPC peer credentials");
    }
    ensure!(
        length as usize == std::mem::size_of::<libc::ucred>(),
        "IPC peer credentials are truncated"
    );
    // SAFETY: getsockopt succeeded and returned the complete ucred structure.
    let peer = unsafe { peer.assume_init() };
    // SAFETY: geteuid takes no arguments and cannot fail.
    let uid = unsafe { libc::geteuid() };
    ensure!(peer.uid == uid, "IPC peer belongs to a different user");
    Ok(())
}

pub async fn connect(data_dir: &Path) -> Result<Async<UnixStream>> {
    let address = address(data_dir)?;
    let name = address
        .as_abstract_name()
        .context("IPC address is not an abstract Unix socket")?;
    // async-io accepts a leading NUL as an abstract namespace address. Preserve
    // every name byte (including embedded NULs) without allocating a pathname.
    let mut path = [0; 108];
    ensure!(name.len() < path.len(), "IPC abstract address is too long");
    path[1..1 + name.len()].copy_from_slice(name);
    let path = Path::new(std::ffi::OsStr::from_bytes(&path[..1 + name.len()]));
    let stream = Async::<UnixStream>::connect(path).await?;
    verify_peer(&stream)?;
    Ok(stream)
}
