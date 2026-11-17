use crate::scan::{FileEntry, FileIdentity};
use std::{fs, io::ErrorKind, path::Path};

const REPLACED: &str = "文件已被替换，请重新扫描后重试。";

/// Call only from a background worker. The system Trash API has no permanent
/// deletion fallback; filesystem identity is revalidated against the scan.
pub fn move_to_trash(path: &Path, expected: FileIdentity) -> Result<Option<FileEntry>, String> {
    move_with(path, expected, native_trash)
}

fn move_with(
    path: &Path,
    expected: FileIdentity,
    operation: impl FnOnce(&Path, FileIdentity) -> Result<Option<FileEntry>, String>,
) -> Result<Option<FileEntry>, String> {
    verify_file(path, expected)?;
    operation(path, expected)
}

fn verify_file(path: &Path, expected: FileIdentity) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("无法确认文件位置，请重新扫描后重试。".into());
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| -> String {
        match error.kind() {
            ErrorKind::NotFound => "文件已移动或删除，请重新扫描。".into(),
            ErrorKind::PermissionDenied => {
                "macOS 拒绝读取文件信息，请检查文件权限和完全磁盘访问权限。".into()
            }
            _ => "无法读取文件信息，请确认磁盘仍已连接后重试。".into(),
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("扫描位置已不再是普通文件，请重新扫描后重试。".into());
    }
    if FileIdentity::from_metadata(&metadata) != expected {
        return Err(REPLACED.into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn native_trash(path: &Path, expected: FileIdentity) -> Result<Option<FileEntry>, String> {
    use objc2::rc::autoreleasepool;
    use objc2_foundation::{NSFileManager, NSURL};
    use std::{
        ffi::{CStr, CString, OsStr},
        os::unix::ffi::OsStrExt,
        ptr::NonNull,
    };

    autoreleasepool(|_| {
        let path_bytes = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| "文件路径无效，请重新扫描后重试。".to_string())?;
        // NSURL copies the filesystem representation during this synchronous
        // call. The CString remains alive throughout the native operation.
        let url = unsafe {
            NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
                NonNull::new(path_bytes.as_ptr().cast_mut()).unwrap(),
                false,
                None,
            )
        };
        // Minimize the path-lookup race after URL construction. As with Finder,
        // NSFileManager operates on a directory entry; there is no public API
        // for an atomic conditional trash-by-inode operation.
        verify_file(path, expected)?;
        let mut destination = None;
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut destination))
            .map_err(|error| native_error(&error.domain().to_string(), error.code()))?;
        // A successful move is committed even when its destination cannot be
        // resolved. Return None for a refresh, never an error that invites an
        // unsafe retry. Trash files still occupy space on the scanned disk.
        let Some(destination) = destination.filter(|url| url.isFileURL()) else {
            return Ok(None);
        };
        // The resulting NSURL remains alive while its borrowed C string is
        // copied into PathBuf, preserving non-UTF8 filenames exactly.
        let destination_bytes =
            unsafe { CStr::from_ptr(destination.fileSystemRepresentation().as_ptr()) };
        let path = Path::new(OsStr::from_bytes(destination_bytes.to_bytes())).to_path_buf();
        Ok(destination_entry(path))
    })
}

#[cfg(not(target_os = "macos"))]
fn native_trash(_path: &Path, _expected: FileIdentity) -> Result<Option<FileEntry>, String> {
    Err("当前系统不支持 macOS 废纸篓操作。".into())
}

fn destination_entry(path: std::path::PathBuf) -> Option<FileEntry> {
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return None;
    }
    Some(FileEntry {
        parent: path.parent()?.to_path_buf(),
        size: metadata.len(),
        identity: FileIdentity::from_metadata(&metadata),
        path,
    })
}

fn native_error(domain: &str, code: isize) -> String {
    match (domain, code) {
        ("NSCocoaErrorDomain", 4 | 260) | ("NSPOSIXErrorDomain", 2) => {
            "文件已移动或删除，请重新扫描。".into()
        }
        ("NSCocoaErrorDomain", 257 | 513) | ("NSPOSIXErrorDomain", 1 | 13) => {
            "macOS 拒绝移动文件，请检查文件及所在目录的写入权限；系统保护的文件无法移到废纸篓。"
                .into()
        }
        ("NSCocoaErrorDomain", 642) | ("NSPOSIXErrorDomain", 30) => {
            "文件所在磁盘为只读，无法移到废纸篓。".into()
        }
        ("NSCocoaErrorDomain", 640) | ("NSPOSIXErrorDomain", 28) => {
            "磁盘可用空间不足，无法移到废纸篓。请释放空间后重试。".into()
        }
        _ => "macOS 未能将文件移到废纸篓。请确认磁盘可写，并在 Finder 中检查该文件后重试。".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            loop {
                let path = std::env::temp_dir().join(format!(
                    "visualize-trash-test-{}-{}",
                    std::process::id(),
                    SEQUENCE.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("cannot create test fixture: {error}"),
                }
            }
        }
        fn file(&self, name: &str) -> (PathBuf, FileIdentity) {
            let path = self.0.join(name);
            fs::write(&path, b"temporary fixture").unwrap();
            let identity = FileIdentity::from_metadata(&fs::symlink_metadata(&path).unwrap());
            (path, identity)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_current_regular_file_reaches_native_operation() {
        let temp = Temp::new();
        let (path, identity) = temp.file("file");
        let mut called = false;
        move_with(&path, identity, |received, fingerprint| {
            assert_eq!(received, path);
            assert_eq!(fingerprint, identity);
            called = true;
            Ok(None)
        })
        .unwrap();
        assert!(called);
        assert!(path.exists(), "mock must never remove any file");
    }

    #[test]
    fn replaced_file_is_rejected_even_with_same_size() {
        let temp = Temp::new();
        let (path, identity) = temp.file("file");
        fs::rename(&path, temp.0.join("original")).unwrap();
        fs::write(&path, b"temporary fixture").unwrap();
        let error =
            move_with(&path, identity, |_, _| panic!("must not trash replacement")).unwrap_err();
        assert_eq!(error, REPLACED);
        assert!(path.exists());
        assert!(temp.0.join("original").exists());
    }

    #[test]
    fn missing_directory_relative_and_symlink_targets_are_rejected() {
        let temp = Temp::new();
        assert!(
            move_with(
                &temp.0.join("missing"),
                FileIdentity::default(),
                |_, _| panic!()
            )
            .is_err()
        );
        let identity = FileIdentity::from_metadata(&fs::symlink_metadata(&temp.0).unwrap());
        assert!(move_with(&temp.0, identity, |_, _| panic!()).is_err());
        assert!(move_with(Path::new("relative"), identity, |_, _| panic!()).is_err());
        #[cfg(unix)]
        {
            let (file, file_identity) = temp.file("file");
            let link = temp.0.join("link");
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert!(move_with(&link, file_identity, |_, _| panic!()).is_err());
            assert!(file.exists());
        }
    }

    #[test]
    fn native_errors_are_concise_actionable_and_never_raw() {
        assert!(native_error("NSCocoaErrorDomain", 513).contains("写入权限"));
        assert!(native_error("NSPOSIXErrorDomain", 30).contains("只读"));
        assert!(native_error("NSCocoaErrorDomain", 640).contains("空间不足"));
        let unknown = native_error("untrusted native exception text", 9999);
        assert!(!unknown.contains("untrusted"));
        assert!(unknown.contains("Finder"));
    }

    #[test]
    fn destination_entry_preserves_file_identity_and_logical_bytes() {
        let temp = Temp::new();
        let (path, identity) = temp.file("source");
        let destination = temp.0.join("renamed");
        fs::rename(&path, &destination).unwrap();
        let entry = destination_entry(destination.clone()).unwrap();
        assert_eq!(entry.path, destination);
        assert_eq!(entry.parent, temp.0);
        assert_eq!(entry.size, b"temporary fixture".len() as u64);
        assert_eq!(entry.identity, identity);
        assert!(destination_entry(temp.0.join("missing")).is_none());
        assert!(destination_entry(temp.0.clone()).is_none());
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "moves one unique temporary fixture to native Trash, then restores it"]
    fn native_trash_round_trip_of_temporary_fixture() {
        let temp = Temp::new();
        let name = format!(
            "visualize-ssd-native-trash-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let (path, identity) = temp.file(&name);
        let destination = move_to_trash(&path, identity)
            .unwrap()
            .expect("native Trash must return actual destination");
        struct Restore(PathBuf, PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::rename(&self.0, &self.1);
            }
        }
        let restore = Restore(destination.path.clone(), path.clone());
        assert!(!path.exists());
        assert!(destination.path.exists());
        assert_eq!(destination.size, b"temporary fixture".len() as u64);
        assert_eq!(
            FileIdentity::from_metadata(&fs::symlink_metadata(&destination.path).unwrap()),
            destination.identity
        );
        fs::rename(&destination.path, &path).unwrap();
        drop(restore);
        assert!(path.exists());
        assert!(!destination.path.exists());
    }
}
