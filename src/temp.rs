//! Private temporary files that leave nothing behind.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

/// A file in `$TMPDIR`, created with mode 0600 and unlinked at once: no exit path (normal,
/// Ctrl-C, panic or SIGKILL) can leave it behind, so no guard or signal hook is needed.
///
/// # Errors
/// When the file cannot be created.
pub fn file() -> io::Result<File> {
    tempfile::tempfile()
}

/// Replaces `dest` with `bytes` atomically: a uniquely named file beside it is written,
/// synced and renamed into place, so readers see the old or the new content and concurrent
/// writers never share a temporary name.
///
/// # Errors
/// When the file cannot be written or renamed.
pub fn replace(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = dest.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(dest).map_err(|err| err.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::*;

    #[test]
    fn temp_files_have_no_name_and_only_the_owner_can_read_them() {
        let meta = file().unwrap().metadata().unwrap();
        assert_eq!(meta.nlink(), 0, "still linked into a directory");
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }
}
