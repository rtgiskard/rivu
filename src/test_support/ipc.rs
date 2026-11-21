/// Keeps the directory inode alive in forked children until exec or exit.
/// CLOEXEC listeners are inherited across fork too; without this pin, removing
/// a directory can recycle its IPC identity while a child still owns its socket.
pub struct SocketTestDirectory {
    directory: tempfile::TempDir,
    _identity: std::fs::File,
}

impl SocketTestDirectory {
    pub fn path(&self) -> &std::path::Path {
        self.directory.path()
    }
}

pub fn socket_test_directory() -> SocketTestDirectory {
    let cache = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".cache");
    std::fs::create_dir_all(&cache).unwrap();
    let directory = tempfile::tempdir_in(cache).unwrap();
    let identity = std::fs::File::open(directory.path()).unwrap();
    SocketTestDirectory {
        directory,
        _identity: identity,
    }
}
