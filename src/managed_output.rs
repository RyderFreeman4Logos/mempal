use std::fs::{self, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail};

pub(crate) fn overwrite_existing_managed_file(
    path: &Path,
    expected_metadata: &fs::Metadata,
    content: &[u8],
    artifact_label: &str,
) -> Result<()> {
    if expected_metadata.file_type().is_symlink() {
        bail!(
            "refusing to overwrite symlinked {artifact_label} {}",
            path.display()
        );
    }
    if !expected_metadata.file_type().is_file() {
        bail!(
            "refusing to overwrite non-regular {artifact_label} {}",
            path.display()
        );
    }

    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);

    let mut file = options
        .open(path)
        .with_context(|| format!("failed to open {artifact_label} {}", path.display()))?;
    let actual_metadata = file.metadata().with_context(|| {
        format!(
            "failed to inspect opened {artifact_label} {}",
            path.display()
        )
    })?;
    if !actual_metadata.file_type().is_file() {
        bail!(
            "refusing to overwrite non-regular {artifact_label} {}",
            path.display()
        );
    }
    #[cfg(unix)]
    if !matches_validated_unix_file(expected_metadata, &actual_metadata) {
        bail!(
            "refusing to overwrite replaced {artifact_label} {}",
            path.display()
        );
    }

    file.set_len(0)
        .with_context(|| format!("failed to truncate {artifact_label} {}", path.display()))?;
    file.write_all(content)
        .with_context(|| format!("failed to write {artifact_label} {}", path.display()))
}

#[cfg(unix)]
fn matches_validated_unix_file(expected: &fs::Metadata, actual: &fs::Metadata) -> bool {
    expected.dev() == actual.dev()
        && expected.ino() == actual.ino()
        && expected.mode() == actual.mode()
        && expected.nlink() == actual.nlink()
        && expected.uid() == actual.uid()
        && expected.gid() == actual.gid()
        && expected.len() == actual.len()
        && expected.mtime() == actual.mtime()
        && expected.mtime_nsec() == actual.mtime_nsec()
        && expected.ctime() == actual.ctime()
        && expected.ctime_nsec() == actual.ctime_nsec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    #[cfg(unix)]
    #[test]
    fn overwrite_existing_managed_file_refuses_symlink_swap_after_validation() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let managed_path = tmp.path().join("managed.md");
        let outside_path = tmp.path().join("outside.md");
        fs::write(&managed_path, "old managed").expect("write managed");
        fs::write(&outside_path, "outside").expect("write outside");
        let expected = fs::symlink_metadata(&managed_path).expect("managed metadata");

        fs::remove_file(&managed_path).expect("remove managed");
        unix_fs::symlink(&outside_path, &managed_path).expect("replace with symlink");

        let error = overwrite_existing_managed_file(
            &managed_path,
            &expected,
            b"new managed",
            "test artifact",
        )
        .expect_err("symlink swap must be refused");

        assert!(
            error.to_string().contains("failed to open test artifact"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(outside_path).expect("read outside"),
            "outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_existing_managed_file_refuses_regular_file_swap_after_validation() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let managed_path = tmp.path().join("managed.md");
        fs::write(&managed_path, "old managed").expect("write managed");
        let expected = fs::symlink_metadata(&managed_path).expect("managed metadata");

        fs::remove_file(&managed_path).expect("remove managed");
        fs::write(&managed_path, "evilmanaged").expect("write replacement");

        let error = overwrite_existing_managed_file(
            &managed_path,
            &expected,
            b"new managed",
            "test artifact",
        )
        .expect_err("regular file swap must be refused");

        assert!(
            error
                .to_string()
                .contains("refusing to overwrite replaced test artifact"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(managed_path).expect("read replacement"),
            "evilmanaged"
        );
    }
}
