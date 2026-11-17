use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct FileEntry {
    pub path: PathBuf,
    /// Logical file length, including sparse regions; not allocated disk blocks.
    pub size: u64,
    pub parent: PathBuf,
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
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("扫描位置不是可读取的真实目录；不扫描符号链接。".into());
    }
    let mut files = Vec::new();
    let mut errors = Vec::new();
    *progress.lock().unwrap_or_else(|e| e.into_inner()) = ScanProgress::default();
    let startup_disk = root == std::path::Path::new("/");
    let mut visited = HashSet::new();
    let mut pending = vec![PendingDirectory {
        identity: identity_from_metadata(&metadata, &root),
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
                if startup_disk && excluded_from_startup_disk(&child.path) {
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

#[cfg(unix)]
type DirectoryIdentity = (u64, u64);
#[cfg(not(unix))]
type DirectoryIdentity = PathBuf;

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
                    Ok(meta) if meta.file_type().is_symlink() => EntryReport::Skipped,
                    Ok(meta) if meta.is_dir() => EntryReport::Directory(PendingDirectory {
                        identity: identity_from_metadata(&meta, &path),
                        path,
                    }),
                    Ok(meta) if meta.is_file() => EntryReport::File(FileEntry {
                        size: meta.len(),
                        path,
                        parent: directory.to_path_buf(),
                    }),
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

#[cfg(unix)]
fn identity_from_metadata(metadata: &fs::Metadata, _path: &std::path::Path) -> DirectoryIdentity {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn identity_from_metadata(_metadata: &fs::Metadata, path: &std::path::Path) -> DirectoryIdentity {
    path.to_path_buf()
}

#[cfg(all(test, unix))]
fn directory_identity(path: &std::path::Path) -> std::io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    Ok((metadata.dev(), metadata.ino()))
}

// This application targets macOS; keeping a fallback makes headless core tests
// portable without introducing filesystem traversal dependencies.
#[cfg(all(test, not(unix)))]
fn directory_identity(path: &std::path::Path) -> std::io::Result<PathBuf> {
    fs::canonicalize(path)
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
            "macOS 拒绝访问扫描位置。请前往系统设置 > 隐私与安全性 > 完全磁盘访问权限，允许此应用访问后重新扫描。".into()
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
        assert!(permission.contains("完全磁盘访问权限"));
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
