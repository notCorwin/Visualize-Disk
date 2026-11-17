use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FileIdentity {
    pub device: u64,
    #[cfg(not(windows))]
    pub inode: u64,
    #[cfg(windows)]
    pub inode: u128,
}

impl FileIdentity {
    #[cfg(unix)]
    pub fn from_metadata(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }

    /// Read an identity from the same no-follow object used for validation.
    /// Identity failures must never turn into a shared, all-zero identity.
    pub fn from_path(path: &std::path::Path, metadata: &fs::Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let _ = path;
            Ok(Self::from_metadata(metadata))
        }
        #[cfg(windows)]
        {
            windows_identity(path, metadata)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (path, metadata);
            Err(std::io::ErrorKind::Unsupported.into())
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    /// Logical file length, including sparse regions; not allocated disk blocks.
    pub size: u64,
    pub parent: PathBuf,
    pub identity: FileIdentity,
}

#[derive(Debug)]
pub struct ScanResult {
    pub root: PathBuf,
    pub files: Vec<FileEntry>,
    pub skipped: u64,
    pub errors: Vec<String>,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanProgress {
    pub files: u64,
    pub bytes: u64,
    pub skipped: u64,
}

/// Scan on a worker thread. Symlinks and special files are never followed.
/// Hard links are counted by directory entry, matching the file-based display.
pub fn scan(
    root: PathBuf,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<ScanProgress>>,
) -> Result<ScanResult, String> {
    let started = Instant::now();
    let metadata = fs::symlink_metadata(&root).map_err(|e| root_error(&e))?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err("扫描位置不是可读取的真实目录；不扫描符号链接。".into());
    }
    let mut files = Vec::new();
    let mut errors = Vec::new();
    *progress.lock().unwrap_or_else(|e| e.into_inner()) = ScanProgress::default();
    let startup_disk = cfg!(target_os = "macos") && root == std::path::Path::new("/");
    let root_identity = FileIdentity::from_path(&root, &metadata).map_err(|e| root_error(&e))?;
    let mut visited = HashSet::new();
    let mut pending = vec![PendingDirectory {
        identity: root_identity,
        path: root.clone(),
    }];
    let mut deferred_data = None;
    // Only this many directory jobs can be active, regardless of tree size.
    // All jobs and metadata batches share Rayon's process-wide worker pool.
    let batch_limit = rayon::current_num_threads().max(1).saturating_mul(4);
    while !pending.is_empty() || deferred_data.is_some() {
        if cancel.load(Ordering::Relaxed) {
            return Err("扫描已取消。".into());
        }
        if pending.is_empty() {
            pending.push(deferred_data.take().unwrap());
        }
        let mut batch = Vec::with_capacity(batch_limit);
        while batch.len() < batch_limit {
            let Some(directory) = pending.pop() else {
                break;
            };
            if visited.insert(directory.identity) {
                batch.push(directory.path);
            } else {
                let mut state = progress.lock().unwrap_or_else(|e| e.into_inner());
                state.skipped = state.skipped.saturating_add(1);
            }
        }
        let reports: Vec<_> = batch
            .into_par_iter()
            .map(|directory| {
                let report = scan_directory(&directory, &cancel, &progress);
                (directory, report)
            })
            .collect();
        for (directory, mut report) in reports {
            if let Some(error) = report.unreadable {
                if directory == root {
                    return Err(root_error(&error));
                }
            }
            if files.is_empty() {
                files = report.files;
            } else {
                files.append(&mut report.files);
            }
            for error in report.errors {
                if errors.len() < 24 {
                    errors.push(error);
                }
            }
            for child in report.directories {
                if (cfg!(windows) && child.identity.device != root_identity.device)
                    || (startup_disk && excluded_from_startup_disk(&child.path))
                {
                    let mut state = progress.lock().unwrap_or_else(|e| e.into_inner());
                    state.skipped = state.skipped.saturating_add(1);
                } else if startup_disk && child.path == std::path::Path::new("/System/Volumes/Data")
                {
                    // The complete canonical tree finishes before Data aliases
                    // enter the same per-scan identity cache.
                    deferred_data = Some(child);
                } else {
                    pending.push(child);
                }
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("扫描已取消。".into());
    }
    files.par_sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    if cancel.load(Ordering::Relaxed) {
        return Err("扫描已取消。".into());
    }
    let state = *progress.lock().unwrap_or_else(|e| e.into_inner());
    Ok(ScanResult {
        root,
        files,
        skipped: state.skipped,
        errors,
        elapsed: started.elapsed(),
    })
}

type DirectoryIdentity = FileIdentity;

struct PendingDirectory {
    path: PathBuf,
    identity: DirectoryIdentity,
}

#[derive(Default)]
struct DirectoryReport {
    files: Vec<FileEntry>,
    directories: Vec<PendingDirectory>,
    errors: Vec<String>,
    unreadable: Option<std::io::Error>,
}

enum EntryReport {
    File(FileEntry),
    Directory(PendingDirectory),
    Skipped,
    Error(PathBuf, std::io::Error),
    Cancelled,
}

fn scan_directory(
    directory: &std::path::Path,
    cancel: &AtomicBool,
    progress: &Mutex<ScanProgress>,
) -> DirectoryReport {
    scan_directory_batches(directory, cancel, progress, || {})
}

fn scan_directory_batches(
    directory: &std::path::Path,
    cancel: &AtomicBool,
    progress: &Mutex<ScanProgress>,
    after_batch: impl Fn(),
) -> DirectoryReport {
    let mut report = DirectoryReport::default();
    if cancel.load(Ordering::Relaxed) {
        return report;
    }
    // Revalidate queued directories before enumeration: a junction introduced
    // after its parent was scanned must not redirect traversal to another tree.
    match fs::symlink_metadata(directory) {
        Ok(metadata) if !is_link_or_reparse(&metadata) && metadata.is_dir() => {}
        Ok(_) => {
            publish(
                progress,
                ScanProgress {
                    skipped: 1,
                    ..Default::default()
                },
            );
            return report;
        }
        Err(error) => {
            let mut delta = ScanProgress::default();
            record_error(&mut delta, &mut report.errors, directory, &error);
            publish(progress, delta);
            report.unreadable = Some(error);
            return report;
        }
    }
    let mut entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            let mut delta = ScanProgress::default();
            record_error(&mut delta, &mut report.errors, directory, &error);
            publish(progress, delta);
            report.unreadable = Some(error);
            return report;
        }
    };
    loop {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // Enumeration stays streaming and bounded; metadata work in even a
        // single large directory can use every available worker.
        let entries: Vec<_> = entries.by_ref().take(512).collect();
        if entries.is_empty() {
            break;
        }
        let batch: Vec<_> = entries
            .into_par_iter()
            .with_min_len(32)
            .map(|entry| {
                if cancel.load(Ordering::Relaxed) {
                    return EntryReport::Cancelled;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => return EntryReport::Error(directory.to_path_buf(), error),
                };
                let path = entry.path();
                match fs::symlink_metadata(&path) {
                    Err(error) => EntryReport::Error(path, error),
                    Ok(meta) if is_link_or_reparse(&meta) => EntryReport::Skipped,
                    Ok(meta) if meta.is_dir() || meta.is_file() => {
                        let identity = match FileIdentity::from_path(&path, &meta) {
                            Ok(identity) => identity,
                            Err(error) => return EntryReport::Error(path, error),
                        };
                        if meta.is_dir() {
                            EntryReport::Directory(PendingDirectory { identity, path })
                        } else {
                            EntryReport::File(FileEntry {
                                identity,
                                size: meta.len(),
                                path,
                                parent: directory.to_path_buf(),
                            })
                        }
                    }
                    Ok(_) => EntryReport::Skipped,
                }
            })
            .collect();
        let mut delta = ScanProgress::default();
        for entry in batch {
            match entry {
                EntryReport::File(file) => {
                    delta.files += 1;
                    delta.bytes = delta.bytes.saturating_add(file.size);
                    report.files.push(file);
                }
                EntryReport::Directory(directory) => report.directories.push(directory),
                EntryReport::Skipped => delta.skipped += 1,
                EntryReport::Error(path, error) => {
                    record_error(&mut delta, &mut report.errors, &path, &error)
                }
                EntryReport::Cancelled => {}
            }
        }
        publish(progress, delta);
        after_batch();
    }
    report
}

fn publish(progress: &Mutex<ScanProgress>, delta: ScanProgress) {
    let mut state = progress.lock().unwrap_or_else(|e| e.into_inner());
    state.files = state.files.saturating_add(delta.files);
    state.bytes = state.bytes.saturating_add(delta.bytes);
    state.skipped = state.skipped.saturating_add(delta.skipped);
}

/// Windows junctions, volume mount points and cloud placeholders are all
/// reparse points. Do not traverse or hydrate any of them during a disk scan.
pub fn is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

#[cfg(windows)]
fn windows_identity(
    path: &std::path::Path,
    expected: &fs::Metadata,
) -> std::io::Result<FileIdentity> {
    use std::os::windows::{
        fs::{MetadataExt, OpenOptionsExt},
        io::AsRawHandle,
    };
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
        },
    };
    if is_link_or_reparse(expected) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    // FILE_READ_ATTRIBUTES only: scanning never requires reading file contents.
    // Share deletion as well as reads/writes so the scan cannot block Explorer.
    let file = fs::OpenOptions::new()
        .access_mode(0x80)
        .share_mode(0x1 | 0x2 | 0x4)
        .custom_flags(0x0020_0000 | 0x0200_0000)
        .open(path)?;
    let actual = file.metadata()?;
    if is_link_or_reparse(&actual)
        || expected.file_attributes() != actual.file_attributes()
        || expected.creation_time() != actual.creation_time()
        || (expected.is_file()
            && (expected.last_write_time() != actual.last_write_time()
                || expected.file_size() != actual.file_size()))
    {
        return Err(std::io::ErrorKind::Interrupted.into());
    }
    let mut info = FILE_ID_INFO::default();
    let result = unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if let Err(error) = result {
        let error = windows_io_error(error);
        // Some FAT/exFAT drivers do not expose FileIdInfo. Use the documented
        // 64-bit index only after confirming a filesystem that uses that width;
        // ReFS and unknown drivers must never silently lose identity bits.
        if !matches!(error.raw_os_error(), Some(1 | 50 | 87)) {
            return Err(error);
        }
        let mut filesystem = [0u16; 32];
        unsafe {
            GetVolumeInformationByHandleW(
                HANDLE(file.as_raw_handle()),
                None,
                None,
                None,
                None,
                Some(&mut filesystem),
            )
        }
        .map_err(windows_io_error)?;
        let end = filesystem
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(filesystem.len());
        let name = String::from_utf16_lossy(&filesystem[..end]);
        if !matches!(
            name.to_ascii_uppercase().as_str(),
            "FAT" | "FAT32" | "EXFAT" | "NTFS"
        ) {
            return Err(error);
        }
        let mut legacy = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut legacy) }
            .map_err(windows_io_error)?;
        let inode = ((legacy.nFileIndexHigh as u64) << 32) | legacy.nFileIndexLow as u64;
        if inode == 0 {
            return Err(std::io::ErrorKind::Unsupported.into());
        }
        return Ok(FileIdentity {
            device: legacy.dwVolumeSerialNumber as u64,
            inode: inode as u128,
        });
    }
    if info.FileId.Identifier == [0; 16] {
        return Err(std::io::ErrorKind::Unsupported.into());
    }
    Ok(FileIdentity {
        device: info.VolumeSerialNumber,
        // ReFS uses all 128 bits; truncating them can alias unrelated objects.
        inode: u128::from_le_bytes(info.FileId.Identifier),
    })
}

#[cfg(windows)]
fn windows_io_error(error: windows::core::Error) -> std::io::Error {
    let code = error.code().0 as u32;
    // windows-core's generic conversion preserves HRESULT, whereas io::Error
    // needs the underlying Win32 code to classify permission/not-found errors.
    if code & 0xffff_0000 == 0x8007_0000 {
        std::io::Error::from_raw_os_error((code & 0xffff) as i32)
    } else {
        std::io::Error::other(error)
    }
}

#[cfg(test)]
fn directory_identity(path: &std::path::Path) -> std::io::Result<DirectoryIdentity> {
    FileIdentity::from_path(path, &fs::symlink_metadata(path)?)
}

fn excluded_from_startup_disk(path: &std::path::Path) -> bool {
    // These are device or other-disk/network mount namespaces, not files on the
    // startup disk. Do not apply this policy when explicitly scanning a folder.
    let path = path.strip_prefix("/System/Volumes/Data").unwrap_or(path);
    matches!(
        path.to_str(),
        Some("/dev" | "/Volumes" | "/Network" | "dev" | "Volumes" | "Network")
    )
}

fn root_error(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => {
            if cfg!(target_os = "macos") {
                "macOS 拒绝访问扫描位置。请前往系统设置 > 隐私与安全性 > 完全磁盘访问权限，允许此应用访问后重新扫描。".into()
            } else {
                "系统拒绝访问扫描位置。请检查当前账户的文件夹访问权限后重新扫描。".into()
            }
        }
        std::io::ErrorKind::NotFound => "扫描位置已不存在。请确认磁盘已连接后重新扫描。".into(),
        _ => "无法读取扫描位置。请确认磁盘已连接且可访问后重新扫描。".into(),
    }
}

fn record_error(
    state: &mut ScanProgress,
    errors: &mut Vec<String>,
    path: &std::path::Path,
    error: &std::io::Error,
) {
    state.skipped += 1;
    // Bound diagnostic memory even when scanning inaccessible system trees.
    if errors.len() < 24 {
        let reason = match error.kind() {
            std::io::ErrorKind::PermissionDenied => "无访问权限",
            std::io::ErrorKind::NotFound => "扫描期间已移除",
            _ => "无法读取",
        };
        errors.push(format!("{}：{reason}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            loop {
                let nonce = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir()
                    .join(format!("storage-scan-{}-{nonce}", std::process::id()));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("cannot create isolated test directory: {error}"),
                }
            }
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn startup_scope_excludes_only_other_mount_namespaces() {
        for path in [
            "/dev",
            "/Volumes",
            "/Network",
            "/System/Volumes/Data/Volumes",
            "/System/Volumes/Data/Network",
            "/System/Volumes/Data/dev",
        ] {
            assert!(excluded_from_startup_disk(std::path::Path::new(path)));
        }
        for path in [
            "/Users",
            "/Applications",
            "/System/Volumes/Data",
            "/Users/a/Volumes",
        ] {
            assert!(!excluded_from_startup_disk(std::path::Path::new(path)));
        }
    }

    #[test]
    fn directory_identity_cache_distinguishes_directories_and_is_per_scan() {
        let temp = Temp::new();
        let child = temp.0.join("child");
        fs::create_dir(&child).unwrap();
        let mut visited = HashSet::new();
        assert!(visited.insert(directory_identity(&temp.0).unwrap()));
        assert!(!visited.insert(directory_identity(&temp.0).unwrap()));
        assert!(visited.insert(directory_identity(&child).unwrap()));
        let mut next_scan = HashSet::new();
        assert!(next_scan.insert(directory_identity(&temp.0).unwrap()));
    }

    #[test]
    fn scans_nested_files_and_zero_length_with_exact_progress() {
        let temp = Temp::new();
        fs::create_dir(temp.0.join("child")).unwrap();
        fs::write(temp.0.join("a"), [0u8; 4]).unwrap();
        fs::write(temp.0.join("child/b"), [0u8; 9]).unwrap();
        fs::write(temp.0.join("empty"), []).unwrap();
        let progress = Arc::new(Mutex::new(ScanProgress::default()));
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(false)),
            progress.clone(),
        )
        .unwrap();
        assert_eq!(
            result.files.iter().map(|f| f.size).collect::<Vec<_>>(),
            vec![9, 4, 0]
        );
        for file in &result.files {
            assert_eq!(
                file.identity,
                FileIdentity::from_path(&file.path, &fs::symlink_metadata(&file.path).unwrap())
                    .unwrap()
            );
        }
        assert_eq!(result.files[0].parent, temp.0.join("child"));
        assert_eq!(result.skipped, 0);
        assert_eq!(progress.lock().unwrap().bytes, 13);
        assert_eq!(progress.lock().unwrap().files, 3);
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_symlink_cycles_or_linked_files() {
        let temp = Temp::new();
        fs::write(temp.0.join("file"), [1u8; 3]).unwrap();
        std::os::unix::fs::symlink(&temp.0, temp.0.join("cycle")).unwrap();
        std::os::unix::fs::symlink(temp.0.join("file"), temp.0.join("link")).unwrap();
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::default(),
        )
        .unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.skipped, 2);
    }

    #[cfg(windows)]
    #[test]
    fn windows_identity_distinguishes_files_and_tracks_hard_links() {
        let temp = Temp::new();
        let first = temp.0.join("原始文件.txt");
        let second = temp.0.join("other.txt");
        let link = temp.0.join("hard-link.txt");
        fs::write(&first, b"same").unwrap();
        fs::write(&second, b"same").unwrap();
        fs::hard_link(&first, &link).unwrap();
        let identity = |path: &std::path::Path| {
            FileIdentity::from_path(path, &fs::symlink_metadata(path).unwrap()).unwrap()
        };
        assert_ne!(identity(&first), FileIdentity::default());
        assert_ne!(identity(&first), identity(&second));
        assert_eq!(identity(&first), identity(&link));
        let original = identity(&first);
        fs::remove_file(&first).unwrap();
        fs::rename(&second, &first).unwrap();
        assert_ne!(identity(&first), original);
    }

    #[cfg(windows)]
    #[test]
    fn windows_scans_unicode_paths_longer_than_max_path() {
        use std::os::windows::ffi::OsStrExt;
        let temp = Temp::new();
        let mut directory = temp.0.clone();
        while directory.as_os_str().encode_wide().count() < 300 {
            directory.push("较长的目录名称-long-directory");
        }
        fs::create_dir_all(&directory).unwrap();
        let file = directory.join("文件.txt");
        assert!(file.as_os_str().encode_wide().count() > 260);
        fs::write(&file, b"content").unwrap();
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::default(),
        )
        .unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, file);
        assert_eq!(result.files[0].size, 7);
    }

    #[cfg(windows)]
    #[test]
    fn windows_does_not_follow_junction_cycles_or_accept_junction_root() {
        use std::os::windows::process::CommandExt;
        let temp = Temp::new();
        fs::write(temp.0.join("file"), b"data").unwrap();
        let junction = temp.0.join("cycle");
        let result = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(&junction)
            .arg(&temp.0)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(result.status.success(), "junction fixture creation failed");
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::default(),
        )
        .unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.skipped, 1);
        assert!(
            scan(
                junction.clone(),
                Arc::new(AtomicBool::new(false)),
                Arc::default()
            )
            .is_err()
        );
        // Remove the junction itself before recursive fixture cleanup.
        fs::remove_dir(junction).unwrap();
    }

    #[test]
    fn cancellation_is_observed_before_traversal() {
        let temp = Temp::new();
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(true)),
            Arc::default(),
        );
        assert!(result.unwrap_err().contains("取消"));
    }

    #[test]
    fn rejects_a_file_as_root() {
        let temp = Temp::new();
        let path = temp.0.join("file");
        fs::write(&path, []).unwrap();
        assert!(scan(path, Arc::new(AtomicBool::new(false)), Arc::default()).is_err());
    }

    #[test]
    fn missing_root_has_actionable_message_without_os_error() {
        let temp = Temp::new();
        let error = scan(
            temp.0.join("missing"),
            Arc::new(AtomicBool::new(false)),
            Arc::default(),
        )
        .unwrap_err();
        assert!(error.contains("不存在"));
        assert!(!error.contains("os error"));
        let permission = root_error(&std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        if cfg!(target_os = "macos") {
            assert!(permission.contains("完全磁盘访问权限"));
        } else {
            assert!(permission.contains("访问权限"));
            assert!(!permission.contains("macOS"));
        }
    }
    #[test]
    fn parallel_and_single_worker_scans_match_with_exact_progress() {
        let temp = Temp::new();
        for directory in 0..64 {
            let path = temp.0.join(format!("dir-{directory}"));
            fs::create_dir(&path).unwrap();
            for file in 0..32 {
                fs::write(path.join(format!("file-{file}")), vec![0; file]).unwrap();
            }
        }
        // Include a large flat directory so metadata batching is exercised too.
        for file in 0..1024 {
            fs::write(temp.0.join(format!("flat-{file}")), []).unwrap();
        }
        let run = |threads| {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let progress = Arc::new(Mutex::new(ScanProgress::default()));
            let start = Instant::now();
            let result = pool
                .install(|| {
                    scan(
                        temp.0.clone(),
                        Arc::new(AtomicBool::new(false)),
                        progress.clone(),
                    )
                })
                .unwrap();
            eprintln!(
                "synthetic cached scan: {threads} workers, {} files, {:?}",
                result.files.len(),
                start.elapsed()
            );
            let actual = *progress.lock().unwrap();
            assert_eq!(actual.files, result.files.len() as u64);
            assert_eq!(
                actual.bytes,
                result.files.iter().map(|file| file.size).sum::<u64>()
            );
            (
                result
                    .files
                    .into_iter()
                    .map(|file| (file.path, file.size))
                    .collect::<Vec<_>>(),
                result.skipped,
            )
        };
        assert_eq!(run(1), run(4));
    }
    #[test]
    fn cancellation_stops_after_the_current_bounded_metadata_batch() {
        let temp = Temp::new();
        for file in 0..1024 {
            fs::write(temp.0.join(format!("file-{file}")), []).unwrap();
        }
        let cancel = AtomicBool::new(false);
        let progress = Mutex::new(ScanProgress::default());
        let report = scan_directory_batches(&temp.0, &cancel, &progress, || {
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!(report.files.len(), 512);
        assert_eq!(progress.lock().unwrap().files, 512);
        assert!(cancel.load(Ordering::Relaxed));
    }
}
