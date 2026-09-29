//! Private temporary files that leave nothing behind (NFR-13).

use std::fs::File;
use std::io;

/// A file in `$TMPDIR`, created with mode 0600 and unlinked at once: no exit path (normal,
/// Ctrl-C, panic or SIGKILL) can leave it behind, so no guard or signal hook is needed.
///
/// # Errors
/// When the file cannot be created.
pub fn file() -> io::Result<File> {
    tempfile::tempfile()
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
