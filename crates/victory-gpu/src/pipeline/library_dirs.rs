//! Maintenance of the on-disk pipeline library layout. Old releases have no
//! separate ownership marker: a regular host-pipelines.bin in a build leaf
//! identifies one of their namespaces.
use super::{EARLY_FILE, LIBRARY_FILE, PREBUILT_FILE};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
const FILE_SHARE_READ_WRITE: u32 = 3;
const DELETE: u32 = 0x0001_0000;
const GENERIC_READ: u32 = 0x8000_0000;

const KEPT: usize = 2;

fn hex(name: &str, width: usize) -> bool {
    name.len() == width
        && name
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn adapter(name: &str) -> Option<&str> {
    let (device, driver) = name.rsplit_once('-')?;
    let mut parts = device.split('-');
    (hex(driver, 16)
        && matches!(parts.next(), Some("hw" | "sw"))
        && (0..4).all(|_| parts.next().is_some_and(|part| hex(part, 8)))
        && parts.next().is_none())
    .then_some(device)
}

fn plain(meta: &fs::Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

pub(super) fn regular(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| plain(&m) && m.is_file())
}

// Hold the directory itself without FILE_SHARE_DELETE. A cleaner takes it
// exclusively before touching its children. OPEN_REPARSE_POINT ensures a
// redirected directory cannot take the walk outside the configured tree.
pub(super) fn hold(path: &Path, exclusive: bool) -> io::Result<File> {
    let handle = OpenOptions::new()
        .access_mode(if exclusive {
            GENERIC_READ | DELETE
        } else {
            GENERIC_READ
        })
        .share_mode(FILE_SHARE_READ_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let meta = handle.metadata()?;
    if meta.is_dir() && plain(&meta) {
        Ok(handle)
    } else {
        Err(io::Error::other("expected a plain cache directory"))
    }
}

fn dirs(root: &Path, accepts: impl Fn(&str) -> bool) -> Vec<(SystemTime, PathBuf)> {
    let mut result = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().is_some_and(&accepts)
                && let Ok(m) = entry.metadata()
                && plain(&m)
                && m.is_dir()
                && let Ok(time) = m.modified()
            {
                result.push((time, entry.path()));
            }
        }
    }
    result.sort_unstable_by(|a, b| b.cmp(a));
    result
}

fn hashes(root: &Path) -> Vec<(SystemTime, PathBuf)> {
    dirs(root, |s| hex(s, 16))
}

fn adapters(root: &Path) -> Vec<(SystemTime, PathBuf)> {
    dirs(root, |s| adapter(s).is_some())
}

fn contains_library(dir: &Path) -> bool {
    hashes(dir)
        .into_iter()
        .any(|(_, leaf)| regular(&leaf.join(LIBRARY_FILE)))
}

// Only unlink recognized regular files. An unknown sibling, reparse point,
// or busy file keeps its containing directory in place for a later run.
fn clear_adapter(dir: &Path) -> io::Result<()> {
    let guard = hold(dir, true)?;
    for (_, leaf) in hashes(dir) {
        let leaf_guard = hold(&leaf, true)?;
        let marker = leaf.join(LIBRARY_FILE);
        if !regular(&marker) {
            continue;
        }
        // A still-unmigrated early list or an unknown sibling keeps the
        // whole leaf. Keeping the marker also makes sharing failures retryable.
        if fs::read_dir(&leaf)?.any(|entry| {
            !entry.is_ok_and(|entry| {
                matches!(
                    entry.file_name().to_str(),
                    Some(LIBRARY_FILE | PREBUILT_FILE)
                ) && regular(&entry.path())
            })
        }) {
            continue;
        }
        let sidecar = leaf.join(PREBUILT_FILE);
        if regular(&sidecar) {
            fs::remove_file(sidecar)?;
        }
        fs::remove_file(marker)?;
        drop(leaf_guard);
        fs::remove_dir(leaf)?;
    }
    drop(guard);
    fs::remove_dir(dir)
}

fn clear_source(dir: &Path) -> io::Result<()> {
    let guard = hold(dir, true)?;
    for (_, child) in adapters(dir) {
        if contains_library(&child) {
            clear_adapter(&child)?;
        }
    }
    drop(guard);
    fs::remove_dir(dir)
}

fn prune(
    current: &Path,
    candidates: impl Iterator<Item = PathBuf>,
    clear: fn(&Path) -> io::Result<()>,
) {
    for old in candidates.filter(|p| p != current).skip(KEPT - 1) {
        if let Err(error) = clear(&old) {
            tracing::debug!(path = %old.display(), %error, "could not retire pipeline cache");
        }
    }
}

pub(super) fn prune_sources(root: &Path, current: &Path) {
    let Ok(_root) = hold(root, false) else { return };
    let versions = hashes(root);
    if versions.len() < KEPT {
        return;
    }
    prune(
        current,
        versions
            .into_iter()
            .map(|(_, p)| p)
            .filter(|p| p == current || adapters(p).iter().any(|(_, d)| contains_library(d))),
        clear_source,
    );
}

pub(super) fn prune_drivers(source: &Path, current: &Path) {
    let Some(device) = current
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(adapter)
    else {
        return;
    };
    let candidates = dirs(source, |s| adapter(s) == Some(device));
    prune(
        current,
        candidates
            .into_iter()
            .map(|(_, p)| p)
            .filter(|p| contains_library(p)),
        clear_adapter,
    );
}

// Only needed for upgrades. Collect every legacy list, newest first, even
// if the old driver, build settings or translator can no longer be selected.
pub(super) fn early_lists(root: &Path) -> Vec<(SystemTime, PathBuf)> {
    let mut lists = Vec::new();
    for (_, source) in hashes(root) {
        for (_, namespace) in adapters(&source) {
            for (_, leaf) in hashes(&namespace) {
                let path = leaf.join(EARLY_FILE);
                if regular(&path)
                    && let Ok(time) = fs::metadata(&path).and_then(|m| m.modified())
                {
                    lists.push((time, path));
                }
            }
        }
    }
    lists.sort_unstable_by(|a, b| b.cmp(a));
    lists
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn seed(source: &Path, device: &str, driver: usize) -> PathBuf {
        let dir = source.join(format!("{device}-{driver:016x}"));
        let leaf = dir.join("abcde0123456789f");
        fs::create_dir_all(&leaf).unwrap();
        fs::write(leaf.join(LIBRARY_FILE), b"driver bytes").unwrap();
        dir
    }

    fn date(dir: &Path, n: u64) {
        OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1000000 + n))
            .unwrap();
    }

    const GPU: &str = "hw-000010de-00002b85-00000000-00000001";
    const WARP: &str = "sw-00001414-0000008c-00000000-00000001";

    #[test]
    fn retire_only_marked_translators_keep_current_newest_and_busy() {
        let root = tempfile::tempdir().unwrap();
        let mut versions = Vec::new();
        for i in 0..5 {
            let source = root.path().join(format!("{i:016x}"));
            seed(&source, GPU, 1);
            date(&source, i);
            versions.push(source);
        }
        let unknown = root.path().join("1234567890abcdef");
        fs::create_dir(&unknown).unwrap();
        fs::write(unknown.join("user-file"), b"keep").unwrap();
        seed(&root.path().join("my-backups"), GPU, 1);
        let guard = hold(&versions[1], false).unwrap();
        prune_sources(root.path(), &versions[0]);
        assert!(versions[0].exists() && versions[1].exists() && versions[4].exists());
        assert!(!versions[2].exists() && !versions[3].exists());
        drop(guard);
        prune_sources(root.path(), &versions[0]);
        assert!(!versions[1].exists());
        assert!(unknown.join("user-file").is_file() && root.path().join("my-backups").exists());
    }

    #[test]
    fn retire_drivers_per_device_skip_busy_blobs_and_unmarked_directories() {
        let source = tempfile::tempdir().unwrap();
        let versions: Vec<_> = (0..5)
            .map(|i| {
                let dir = seed(source.path(), GPU, i);
                date(&dir, i as u64);
                dir
            })
            .collect();
        let warp = seed(source.path(), WARP, 0);
        let igpu = seed(source.path(), "hw-00008086-00001234-00000000-00000001", 0);
        let unknown = source.path().join(format!("{GPU}-ffffffffffffffff"));
        fs::create_dir(&unknown).unwrap();
        let busy = versions[1].join("abcde0123456789f").join(LIBRARY_FILE);
        let guard = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&busy)
            .unwrap();
        prune_drivers(source.path(), &versions[0]);
        assert!(busy.exists() && versions[0].exists() && versions[4].exists());
        assert!(!versions[2].exists() && !versions[3].exists());
        assert!(warp.exists() && igpu.exists() && unknown.exists());
        drop(guard);
        prune_drivers(source.path(), &versions[0]);
        assert!(!versions[1].exists());
    }
}
