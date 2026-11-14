use std::fs;
use std::path::Path;

#[cfg(unix)]
pub(super) fn secure(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
pub(super) fn secure(_path: &Path) {}

/// Takes an exclusive advisory lock on the data directory.
///
/// Two workers against one database would each keep their own pending-order
/// state and could submit the same purchase twice, so the second process is
/// refused instead of being allowed to trade. The lock is held by the returned
/// handle and released when `Store` is dropped; the OS releases it even if the
/// process crashes.
pub fn lock_data_dir(data_dir: &Path) -> Result<fs::File, String> {
    fs::create_dir_all(data_dir)
        .map_err(|error| format!("could not create {}: {error}", data_dir.display()))?;
    let path = data_dir.join(".worker.lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(fs::TryLockError::WouldBlock) => Err(format!(
            "another worker already owns {}; only one process may use a data directory",
            data_dir.display()
        )),
        Err(fs::TryLockError::Error(error)) => {
            Err(format!("could not lock {}: {error}", path.display()))
        }
    }
}
