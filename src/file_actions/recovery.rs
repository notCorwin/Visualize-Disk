use super::{destination_entry, verify_file};
use crate::scan::{FileEntry, FileIdentity};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Debug)]
pub struct RecoveryRecord {
    pub id: String,
    pub original_path: PathBuf,
    pub trash_path: Option<PathBuf>,
    pub identity: FileIdentity,
    pub original_links: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    version: u8,
    id: String,
    original: Vec<u8>,
    trash: Option<Vec<u8>>,
    device: u64,
    inode: u64,
    original_links: u64,
}

pub(super) struct Store {
    root: PathBuf,
    trash: PathBuf,
    _lock: File,
}

fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
fn bytes_path(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes).into()
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() < 100 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}
fn storage_error() -> String {
    "无法保存文件恢复记录，请检查应用数据目录的权限和磁盘空间；尚未移动文件。".into()
}

impl Store {
    #[cfg(test)]
    pub(super) fn open_for_test(root: PathBuf, trash: PathBuf) -> Result<Self, String> {
        Self::open(root, trash)
    }
    #[cfg(test)]
    pub(super) fn restore_for_test(&self, id: &str) -> Result<Option<FileEntry>, String> {
        self.restore(id)
    }
    pub(super) fn production() -> Result<Self, String> {
        let home = super::home_directory()?;
        Self::open(
            home.join("Library/Application Support/com.visualizessd.app/Recovery"),
            home.join(".Trash"),
        )
    }

    fn open(root: PathBuf, trash: PathBuf) -> Result<Self, String> {
        use std::os::unix::{
            fs::{DirBuilderExt, OpenOptionsExt},
            io::AsRawFd,
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&root)
            .map_err(|_| storage_error())?;
        let meta = fs::symlink_metadata(&root).map_err(|_| storage_error())?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(storage_error());
        }
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
                .map_err(|_| storage_error())?;
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(0x100)
            .open(root.join(".lock"))
            .map_err(|_| storage_error())?;
        unsafe extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        // Separate opens, even within one process, serialize journal mutations.
        if unsafe { flock(lock.as_raw_fd(), 2) } != 0 {
            return Err(storage_error());
        }
        Ok(Self {
            root,
            trash,
            _lock: lock,
        })
    }

    pub(super) fn pending(
        &self,
        path: &Path,
        identity: FileIdentity,
    ) -> Result<(RecoveryRecord, bool), String> {
        // Reuse an unresolved record for this exact object. An interrupted
        // Finder reply must never erase the already-durable original location.
        for record in self.records()? {
            if record.original_path == path
                && record.identity == identity
                && record.trash_path.is_none()
            {
                return Ok((record, false));
            }
        }
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| storage_error())?
            .as_nanos();
        let record = RecoveryRecord {
            id: format!(
                "{}-{nanos}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            original_path: path.to_path_buf(),
            trash_path: None,
            identity,
            original_links: {
                use std::os::unix::fs::MetadataExt;
                fs::symlink_metadata(path)
                    .map_err(|_| storage_error())?
                    .nlink()
            },
        };
        self.save(&record)?;
        Ok((record, true))
    }

    pub(super) fn save(&self, record: &RecoveryRecord) -> Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt;
        if !valid_id(&record.id) {
            return Err(storage_error());
        }
        let saved = Saved {
            version: 1,
            id: record.id.clone(),
            original: path_bytes(&record.original_path),
            trash: record.trash_path.as_deref().map(path_bytes),
            device: record.identity.device,
            inode: record.identity.inode,
            original_links: record.original_links,
        };
        let bytes = serde_json::to_vec(&saved).map_err(|_| storage_error())?;
        let temporary = self.root.join(format!(".{}.tmp", record.id));
        // The journal lock makes this transaction's temporary name exclusive.
        if temporary.exists() || fs::symlink_metadata(&temporary).is_ok() {
            fs::remove_file(&temporary).map_err(|_| storage_error())?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(0x100)
            .open(&temporary)
            .map_err(|_| storage_error())?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| storage_error())?;
        fs::rename(&temporary, self.root.join(format!("{}.json", record.id)))
            .map_err(|_| storage_error())?;
        File::open(&self.root)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| storage_error())
    }

    pub(super) fn remove(&self, id: &str) -> Result<(), String> {
        if !valid_id(id) {
            return Err(storage_error());
        }
        fs::remove_file(self.root.join(format!("{id}.json"))).map_err(|_| storage_error())?;
        File::open(&self.root)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| storage_error())
    }

    fn records(&self) -> Result<Vec<RecoveryRecord>, String> {
        let mut result = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|_| storage_error())? {
            let entry = entry.map_err(|_| storage_error())?;
            if entry.path().extension().is_none_or(|e| e != "json") {
                continue;
            }
            let meta = entry.file_type().map_err(|_| storage_error())?;
            if !meta.is_file() || entry.metadata().map_err(|_| storage_error())?.len() > 131072 {
                return Err(storage_error());
            }
            let saved: Saved =
                serde_json::from_slice(&fs::read(entry.path()).map_err(|_| storage_error())?)
                    .map_err(|_| storage_error())?;
            if saved.version != 1
                || !valid_id(&saved.id)
                || entry.file_name() != std::ffi::OsString::from(format!("{}.json", saved.id))
            {
                return Err(storage_error());
            }
            let record = RecoveryRecord {
                id: saved.id,
                original_path: bytes_path(saved.original),
                trash_path: saved.trash.map(bytes_path),
                identity: FileIdentity {
                    device: saved.device,
                    inode: saved.inode,
                },
                original_links: saved.original_links,
            };
            if !record.original_path.is_absolute()
                || record.trash_path.as_ref().is_some_and(|p| !p.is_absolute())
            {
                return Err(storage_error());
            }
            result.push(record);
        }
        result.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        Ok(result)
    }

    pub(super) fn is_trash_destination(&self, path: &Path) -> bool {
        path.parent() == Some(self.trash.as_path())
            || fs::canonicalize(path.parent().unwrap_or(Path::new("")))
                .ok()
                .zip(fs::canonicalize(&self.trash).ok())
                .is_some_and(|(p, root)| p == root)
    }

    pub(super) fn locate(&self, identity: FileIdentity) -> Option<FileEntry> {
        // The trash backend accepts only the current user's startup-data device,
        // so its Finder destination is the user's Trash, not another volume.
        let mut found = None;
        for entry in fs::read_dir(&self.trash).ok()? {
            let Ok(entry) = entry else {
                continue;
            };
            let Some(file) = destination_entry(entry.path()) else {
                continue;
            };
            if file.identity == identity {
                if found.is_some() {
                    return None;
                }
                found = Some(file);
            }
        }
        found
    }

    pub(super) fn reconciled(&self) -> Result<Vec<RecoveryRecord>, String> {
        let mut result = Vec::new();
        for mut record in self.records()? {
            if record.trash_path.as_ref().is_some_and(|path| {
                self.is_trash_destination(path) && verify_file(path, record.identity).is_ok()
            }) {
                result.push(record);
                continue;
            }
            let original_exists = verify_file(&record.original_path, record.identity).is_ok();
            if record.trash_path.is_some() && original_exists {
                self.remove(&record.id)?;
                continue;
            }
            if !original_exists && record.original_links == 1 {
                if let Some(found) = self.locate(record.identity) {
                    record.trash_path = Some(found.path);
                    self.save(&record)?;
                } else {
                    record.trash_path = None;
                }
            } else {
                // Never infer a moved directory entry from another hard link.
                // Keep pending originals durable if a Finder reply was lost.
                record.trash_path = None;
            }
            result.push(record);
        }
        Ok(result)
    }

    fn restore(&self, id: &str) -> Result<Option<FileEntry>, String> {
        let records = self.reconciled()?;
        let record = records
            .into_iter()
            .find(|r| r.id == id)
            .ok_or("找不到该文件的恢复记录，请重新检查恢复列表。")?;
        let trash = record
            .trash_path
            .as_ref()
            .ok_or("尚未确认文件在废纸篓中的位置，请重新检查；不要清空废纸篓。")?;
        if !self.is_trash_destination(trash) {
            return Err("恢复记录中的废纸篓位置无效，未移动任何文件。".into());
        }
        verify_file(trash, record.identity)?;
        for parent in record
            .original_path
            .parent()
            .ok_or("原始路径无效，无法放回原处。")?
            .ancestors()
        {
            let meta = fs::symlink_metadata(parent)
                .map_err(|_| "原文件夹不存在或无法访问，请先恢复该文件夹。")?;
            if !meta.is_dir() || meta.file_type().is_symlink() {
                return Err("原文件夹已变更，请在 Finder 中选择恢复位置。".into());
            }
        }
        verify_file(trash, record.identity)?;
        rename_exclusive(trash, &record.original_path)?;
        // A failed cleanup cannot turn a committed restore into an apparent
        // failure. The next load reconciles the original inode and removes it.
        let _ = self.remove(&record.id);
        Ok(destination_entry(record.original_path))
    }
}

#[cfg_attr(test, allow(dead_code))]
pub fn recoveries() -> Result<Vec<RecoveryRecord>, String> {
    Store::production()?.reconciled()
}
pub fn restore(id: &str) -> Result<Option<FileEntry>, String> {
    Store::production()?.restore(id)
}

fn rename_exclusive(source: &Path, destination: &Path) -> Result<(), String> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    unsafe extern "C" {
        fn renamex_np(
            from: *const std::ffi::c_char,
            to: *const std::ffi::c_char,
            flags: u32,
        ) -> i32;
    }
    let source =
        CString::new(source.as_os_str().as_bytes()).map_err(|_| "文件路径无效，无法放回原处。")?;
    let destination = CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| "原始路径无效，无法放回原处。")?;
    // RENAME_EXCL atomically refuses existing files, directories and symlinks.
    if unsafe { renamex_np(source.as_ptr(), destination.as_ptr(), 4) } == 0 {
        return Ok(());
    }
    Err(match std::io::Error::last_os_error().kind() {
        std::io::ErrorKind::AlreadyExists => {
            "原位置已有同名项目，未覆盖任何内容。请先在 Finder 中重命名或移走该项目。"
        }
        std::io::ErrorKind::PermissionDenied => "macOS 拒绝恢复文件，请检查原文件夹的写入权限。",
        std::io::ErrorKind::NotFound => "原文件夹或废纸篓中的文件已不存在，请重新检查。",
        _ => "无法安全放回原处。文件仍在废纸篓中，请在 Finder 中检查原位置和磁盘状态。",
    }
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            loop {
                let p = std::env::temp_dir().join(format!(
                    "visualize-recovery-{}-{}",
                    std::process::id(),
                    N.fetch_add(1, Ordering::Relaxed)
                ));
                if fs::create_dir(&p).is_ok() {
                    return Self(fs::canonicalize(p).unwrap());
                }
            }
        }
        fn store(&self) -> Store {
            fs::create_dir_all(self.0.join("trash")).unwrap();
            Store::open(self.0.join("records"), self.0.join("trash")).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn durable_pending_reconciles_and_restore_survives_reopen() {
        let f = Fixture::new();
        let original = f.0.join("source");
        fs::write(&original, b"recover me").unwrap();
        let identity = destination_entry(original.clone()).unwrap().identity;
        let store = f.store();
        let record = store.pending(&original, identity).unwrap().0;
        let trash = f.0.join("trash/renamed");
        fs::rename(&original, &trash).unwrap();
        drop(store);
        let store = f.store();
        let records = store.reconciled().unwrap();
        assert_eq!(records[0].trash_path.as_ref(), Some(&trash));
        let restored = store.restore(&record.id).unwrap().unwrap();
        assert_eq!(restored.path, original);
        assert_eq!(fs::read(&original).unwrap(), b"recover me");
        assert!(store.reconciled().unwrap().is_empty());
    }
    #[test]
    fn existing_origin_and_replaced_trash_are_never_overwritten() {
        let f = Fixture::new();
        let source = f.0.join("source");
        fs::write(&source, b"original").unwrap();
        let identity = destination_entry(source.clone()).unwrap().identity;
        let store = f.store();
        let mut record = store.pending(&source, identity).unwrap().0;
        let trash = f.0.join("trash/file");
        fs::rename(&source, &trash).unwrap();
        record.trash_path = Some(trash.clone());
        store.save(&record).unwrap();
        fs::write(&source, b"new content").unwrap();
        assert!(store.restore(&record.id).unwrap_err().contains("同名"));
        assert_eq!(fs::read(&source).unwrap(), b"new content");
        assert_eq!(fs::read(&trash).unwrap(), b"original");
        fs::rename(&trash, f.0.join("outside-trash")).unwrap();
        fs::write(&trash, b"replacement").unwrap();
        assert!(store.restore(&record.id).is_err());
        assert_eq!(fs::read(&trash).unwrap(), b"replacement");
    }
    #[test]
    fn non_utf8_paths_round_trip_and_corrupt_store_fails_closed() {
        use std::os::unix::ffi::OsStringExt;
        let f = Fixture::new();
        let source = f.0.join(std::ffi::OsString::from_vec(vec![b'f', 255]));
        let store = f.store();
        let r = RecoveryRecord {
            id: "raw-bytes".into(),
            original_path: source.clone(),
            trash_path: None,
            identity: FileIdentity::default(),
            original_links: 1,
        };
        store.save(&r).unwrap();
        assert_eq!(store.records().unwrap()[0].original_path, source);
        fs::write(store.root.join(format!("{}.json", r.id)), b"invalid").unwrap();
        assert!(store.records().is_err());
        assert_eq!(store.root.parent(), Some(f.0.as_path()));
    }
    #[test]
    fn hardlink_pending_never_uses_another_trash_directory_entry() {
        let f = Fixture::new();
        let source = f.0.join("source");
        fs::write(&source, b"same inode").unwrap();
        let store = f.store();
        let existing = f.0.join("trash/older-link");
        fs::hard_link(&source, &existing).unwrap();
        let identity = destination_entry(source.clone()).unwrap().identity;
        let record = store.pending(&source, identity).unwrap().0;
        assert_eq!(record.original_links, 2);
        assert!(store.reconciled().unwrap()[0].trash_path.is_none());
        let moved = f.0.join("trash/newer-link");
        fs::rename(&source, &moved).unwrap();
        assert!(store.locate(identity).is_none());
        assert!(store.reconciled().unwrap()[0].trash_path.is_none());
        assert!(store.restore(&record.id).is_err());
        assert!(existing.exists() && moved.exists());
    }
}
