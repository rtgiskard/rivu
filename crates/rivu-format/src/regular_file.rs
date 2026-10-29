//! Descriptor-based admission for local regular-file readers.

use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

/// Follow ordinary symlinks, but reject non-regular opened objects. Nonblocking
/// open ensures a FIFO substituted at the path cannot wait for a writer.
pub fn open_regular_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input is not an ordinary file",
        ));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::CString, os::unix::fs::symlink};

    #[test]
    fn admission_uses_opened_regular_object_and_follows_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        let link = directory.path().join("link");
        symlink(&path, &link).unwrap();
        let file = open_regular_file(&link).unwrap();
        std::fs::remove_file(&path).unwrap();
        let name = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: name is NUL terminated and mkfifo retains no pointer.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(file.metadata().unwrap().is_file());
        assert_eq!(
            open_regular_file(&link).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            open_regular_file(directory.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
