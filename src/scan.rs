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
        return Err("请选择真实文件夹；不扫描符号链接。".into());
    }
    let mut files = Vec::new();
    let mut errors = Vec::new();
    let mut state = ScanProgress::default();
    *progress.lock().unwrap_or_else(|e| e.into_inner()) = state;
    let mut pending = vec![root.clone()];
    let mut published = Instant::now();
    let mut processed = 0usize;
    while let Some(directory) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err("扫描已取消。".into());
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(e) if directory == root => {
                return Err(root_error(&e));
            }
            Err(e) => {
                record_error(&mut state, &mut errors, &directory, &e);
                continue;
            }
        };
        for entry in entries {
            if cancel.load(Ordering::Relaxed) {
                return Err("扫描已取消。".into());
            }
            processed += 1;
            match entry {
                Err(e) => record_error(&mut state, &mut errors, &directory, &e),
                Ok(entry) => {
                    let path = entry.path();
                    match fs::symlink_metadata(&path) {
                        Err(e) => record_error(&mut state, &mut errors, &path, &e),
                        Ok(meta) if meta.file_type().is_symlink() => state.skipped += 1,
                        Ok(meta) if meta.is_dir() => pending.push(path),
                        Ok(meta) if meta.is_file() => {
                            let size = meta.len();
                            state.files += 1;
                            state.bytes = state.bytes.saturating_add(size);
                            files.push(FileEntry {
                                path,
                                size,
                                parent: directory.clone(),
                            });
                        }
                        Ok(_) => state.skipped += 1,
                    }
                }
            }
            if processed % 128 == 0 || published.elapsed() >= Duration::from_millis(100) {
                *progress.lock().unwrap_or_else(|e| e.into_inner()) = state;
                published = Instant::now();
            }
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("扫描已取消。".into());
    }
    // Sorting once makes repeat scans stable without sorting every directory.
    files.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    if cancel.load(Ordering::Relaxed) {
        return Err("扫描已取消。".into());
    }
    *progress.lock().unwrap_or_else(|e| e.into_inner()) = state;
    Ok(ScanResult {
        root,
        files,
        skipped: state.skipped,
        errors,
        elapsed: started.elapsed(),
    })
}

fn root_error(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => {
            "macOS 拒绝访问此文件夹。请重新选择文件夹，或前往系统设置 > 隐私与安全性 > 完全磁盘访问权限，允许此应用访问。".into()
        }
        std::io::ErrorKind::NotFound => "所选文件夹已不存在。请重新选择文件夹。".into(),
        _ => "无法读取所选文件夹。请确认磁盘已连接，并重新选择文件夹。".into(),
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
}
