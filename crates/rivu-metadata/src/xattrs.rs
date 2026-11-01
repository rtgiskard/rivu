use anyhow::{Context, Result, ensure};
use std::{ffi::CStr, fs::File, io, os::fd::AsRawFd};

const MAX_LIST: usize = 64 * 1024;
const MAX_VALUE: usize = 64 * 1024;
const MAX_TOTAL: usize = 1024 * 1024;

fn list(file: libc::c_int) -> Result<Vec<u8>> {
    let size = unsafe { libc::flistxattr(file, std::ptr::null_mut(), 0) };
    if size < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOTSUP) {
            return Ok(Vec::new());
        }
        return Err(error).context("Listing extended permissions");
    }
    let size = usize::try_from(size)?;
    ensure!(
        size <= MAX_LIST,
        "Extended attribute list exceeds preservation budget"
    );
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut names = vec![0u8; size];
    let read = unsafe { libc::flistxattr(file, names.as_mut_ptr().cast(), names.len()) };
    ensure!(
        read >= 0,
        "Reading extended attribute list: {}",
        io::Error::last_os_error()
    );
    ensure!(
        read as usize == size,
        "Extended attributes changed during edit"
    );
    Ok(names)
}

/// Copy descriptor-scoped extended attributes, including POSIX access ACLs.
/// Refuse rather than discard attributes that cannot be restored. Source
/// identity/ctime is validated by the transaction after copying.
pub(crate) fn copy(source: &File, destination: &File) -> Result<()> {
    let source = source.as_raw_fd();
    let destination = destination.as_raw_fd();
    let names = list(source)?;
    // A newly created inode can inherit a directory ACL that the original did
    // not have. Remove destination-only attributes instead of broadening access.
    for (index, name) in list(destination)?
        .split_inclusive(|byte| *byte == 0)
        .enumerate()
    {
        ensure!(
            index < 4096,
            "Replacement has too many inherited extended attributes"
        );
        if names
            .split_inclusive(|byte| *byte == 0)
            .any(|original| original == name)
        {
            continue;
        }
        let name = CStr::from_bytes_with_nul(name)?;
        let result = unsafe { libc::fremovexattr(destination, name.as_ptr()) };
        ensure!(
            result == 0,
            "Removing inherited extended attribute {}: {}",
            name.to_string_lossy(),
            io::Error::last_os_error()
        );
    }
    let mut total = names.len();
    for (index, name) in names.split_inclusive(|byte| *byte == 0).enumerate() {
        ensure!(
            index < 4096,
            "Source has too many extended attributes to preserve"
        );
        let name =
            CStr::from_bytes_with_nul(name).context("Invalid source extended attribute name")?;
        let size = unsafe { libc::fgetxattr(source, name.as_ptr(), std::ptr::null_mut(), 0) };
        ensure!(
            size >= 0,
            "Sizing source extended attribute: {}",
            io::Error::last_os_error()
        );
        let size = usize::try_from(size)?;
        total = total
            .checked_add(size)
            .context("Extended attribute preservation budget overflow")?;
        ensure!(
            size <= MAX_VALUE && total <= MAX_TOTAL,
            "Source extended attributes exceed preservation budget"
        );
        let mut value = vec![0u8; size];
        let read = unsafe {
            libc::fgetxattr(
                source,
                name.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        ensure!(
            read >= 0,
            "Reading source extended attribute: {}",
            io::Error::last_os_error()
        );
        ensure!(
            read as usize == size,
            "Source extended attribute changed during edit"
        );
        // A temp file may already have the desired security label; do not
        // demand permission to rewrite an unchanged mandatory attribute.
        let current_size =
            unsafe { libc::fgetxattr(destination, name.as_ptr(), std::ptr::null_mut(), 0) };
        if current_size == size as isize {
            let mut current = vec![0u8; size];
            let read = unsafe {
                libc::fgetxattr(
                    destination,
                    name.as_ptr(),
                    current.as_mut_ptr().cast(),
                    current.len(),
                )
            };
            if read == size as isize && current == value {
                continue;
            }
        }
        let result = unsafe {
            libc::fsetxattr(
                destination,
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        ensure!(
            result == 0,
            "Restoring extended attribute {}: {}",
            name.to_string_lossy(),
            io::Error::last_os_error()
        );
    }
    Ok(())
}
