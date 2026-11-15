//! R1 (wave H): durability of the session file.
use oxa_proto::OxaError;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// `fdatasync` the file at `path` (opened read-only, `O_NOFOLLOW`): every append written before
/// this call is on the disk when it returns.
pub(crate) fn sync_file(path: &Path) -> Result<(), OxaError> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| OxaError::io(&format!("cannot open {} to sync it", path.display()), &e))?;
    f.sync_data()
        .map_err(|e| OxaError::io(&format!("cannot sync {}", path.display()), &e))
}
