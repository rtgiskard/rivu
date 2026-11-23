//! Descriptor-based admission for local regular-file readers.

use std::{
    fs::{File, Metadata, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

/// Convert a filesystem timestamp to exact signed Unix nanoseconds, if representable.
/// Sentinel values and cache/error policy belong to the caller.
pub fn modified_unix_ns(modified: std::time::SystemTime) -> Option<i64> {
    let nanoseconds = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).ok()?,
        Err(error) => -i128::try_from(error.duration().as_nanos()).ok()?,
    };
    i64::try_from(nanoseconds).ok()
}

/// An owned descriptor whose opened object has been checked to be a regular file.
#[derive(Debug)]
pub struct RegularFile(File);

impl RegularFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        Self::open_with_metadata(path).map(|(file, _)| file)
    }

    pub fn open_with_metadata(path: &Path) -> io::Result<(Self, Metadata)> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)?;
        Self::admit(file)
    }

    pub fn from_file(file: File) -> io::Result<Self> {
        Self::admit(file).map(|(file, _)| file)
    }

    fn admit(file: File) -> io::Result<(Self, Metadata)> {
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "input is not an ordinary file",
            ));
        }
        Ok((Self(file), metadata))
    }

    /// Duplicate the descriptor, sharing its file offset.
    pub fn try_clone(&self) -> io::Result<Self> {
        self.0.try_clone().map(Self)
    }

    pub fn as_file(&self) -> &File {
        &self.0
    }

    pub fn into_file(self) -> File {
        self.0
    }
}

/// Follow ordinary symlinks, but reject non-regular opened objects. Nonblocking
/// open ensures a FIFO substituted at the path cannot wait for a writer.
pub fn open_regular_file(path: &Path) -> io::Result<File> {
    RegularFile::open(path).map(RegularFile::into_file)
}

/// Return the descriptor and the metadata used to admit it.
pub fn open_regular_file_with_metadata(path: &Path) -> io::Result<(File, Metadata)> {
    RegularFile::open_with_metadata(path).map(|(file, metadata)| (file.into_file(), metadata))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::CString, os::unix::fs::symlink};

    #[test]
    fn signed_nanoseconds_preserve_epoch_negative_and_full_integer_range() {
        use std::time::{Duration, UNIX_EPOCH};
        assert_eq!(modified_unix_ns(UNIX_EPOCH), Some(0));
        assert_eq!(
            modified_unix_ns(UNIX_EPOCH - Duration::from_nanos(1)),
            Some(-1)
        );
        let maximum = i64::MAX as u64;
        assert_eq!(
            modified_unix_ns(UNIX_EPOCH + Duration::from_nanos(maximum)),
            Some(i64::MAX)
        );
        assert_eq!(
            modified_unix_ns(UNIX_EPOCH - Duration::from_nanos(maximum + 1)),
            Some(i64::MIN)
        );
        assert_eq!(
            modified_unix_ns(UNIX_EPOCH + Duration::from_nanos(maximum + 1)),
            None
        );
        assert_eq!(
            modified_unix_ns(UNIX_EPOCH - Duration::from_nanos(maximum + 2)),
            None
        );
    }

    #[test]
    fn admission_uses_opened_regular_object_and_follows_symlinks() {
        // A blocking-open regression waits forever for a FIFO writer.
        if crate::test_support::run_in_subprocess(std::time::Duration::from_secs(10)) {
            return;
        }
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
