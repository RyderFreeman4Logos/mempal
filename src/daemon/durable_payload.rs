use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

#[cfg(test)]
thread_local! {
    static FILE_SYNC_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PARENT_SYNC_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn sync_file(file: &fs::File, path: &Path) -> Result<()> {
    file.sync_all()
        .with_context(|| format!("failed to sync {}", path.display()))?;
    #[cfg(test)]
    FILE_SYNC_CALLS.with(|calls| calls.set(calls.get() + 1));
    Ok(())
}

fn sync_parent(parent: &Path) -> Result<()> {
    fs::File::open(parent)
        .with_context(|| format!("failed to open payload directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("failed to sync payload directory {}", parent.display()))?;
    #[cfg(test)]
    PARENT_SYNC_CALLS.with(|calls| calls.set(calls.get() + 1));
    Ok(())
}

fn sync_existing(path: &Path, parent: &Path) -> Result<()> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    sync_file(&file, path)?;
    sync_parent(parent)
}

fn persist<F>(path: &Path, write: F) -> Result<()>
where
    F: FnOnce(&mut fs::File) -> Result<()>,
{
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!("raw hook payload path has no parent: {}", path.display())
    })?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    if path.exists() {
        return sync_existing(path, parent);
    }

    let mut staged = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("failed to create private payload beside {}", path.display()))?;
    write(staged.as_file_mut())?;
    sync_file(staged.as_file(), staged.path())?;
    let staged_path = staged.into_temp_path();
    fs::rename(&staged_path, path)
        .with_context(|| format!("failed to publish raw hook payload {}", path.display()))?;
    sync_parent(parent)
}

pub(super) fn persist_raw_payload_from_path(source: &Path, target: &Path) -> Result<()> {
    let mut spool = fs::File::open(source)
        .with_context(|| format!("failed to open hook spool {}", source.display()))?;
    persist(target, |file| {
        std::io::copy(&mut spool, file)
            .with_context(|| {
                format!(
                    "failed to copy hook spool {} to private payload for {}",
                    source.display(),
                    target.display()
                )
            })
            .map(|_| ())
    })?;
    match fs::remove_file(source) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove hook spool {}", source.display()))
        }
    }
}

pub(super) fn persist_raw_payload_at(raw_payload: &str, path: &Path) -> Result<()> {
    persist(path, |file| {
        file.write_all(raw_payload.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))
    })
}

#[cfg(test)]
fn reset_sync_calls() {
    FILE_SYNC_CALLS.with(|calls| calls.set(0));
    PARENT_SYNC_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
fn sync_calls() -> (usize, usize) {
    (
        FILE_SYNC_CALLS.with(std::cell::Cell::get),
        PARENT_SYNC_CALLS.with(std::cell::Cell::get),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_payload_publication_syncs_file_and_parent_for_inline_and_spooled_payloads() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let inline_target = tmp.path().join("hook-payloads").join("inline.json");
        let spool = tmp.path().join("hook-spool.json");
        let promoted_target = tmp.path().join("hook-payloads").join("spooled.json");
        fs::write(&spool, "spooled payload").expect("write spool");

        reset_sync_calls();
        persist_raw_payload_at("inline payload", &inline_target).expect("persist inline");
        persist_raw_payload_from_path(&spool, &promoted_target).expect("promote spool");

        assert_eq!(
            fs::read_to_string(&inline_target).expect("read inline"),
            "inline payload"
        );
        assert_eq!(
            fs::read_to_string(&promoted_target).expect("read promoted"),
            "spooled payload"
        );
        assert!(!spool.exists(), "promoted spool must be removed");
        assert_eq!(
            sync_calls(),
            (2, 2),
            "every payload must sync its private file and parent namespace"
        );
    }
}
