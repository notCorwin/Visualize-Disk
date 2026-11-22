use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows_ntfs;

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
    /// Shared by entries produced by the same directory scan.
    pub parent: Arc<Path>,
    pub identity: FileIdentity,
    pub parent_identity: FileIdentity,
    /// True only when the parent was fully enumerated and had no subdirectories.
    pub parent_is_leaf: bool,
    pub can_trash: bool,
}

#[derive(Debug)]
pub struct ScanResult {
    pub root: PathBuf,
    pub files: Vec<FileEntry>,
    /// Directory facts from this scan, including empty and skipped directories.
    pub directories: Arc<[DirectoryFact]>,
    pub skipped: u64,
    pub errors: Vec<String>,
    pub elapsed: Duration,
}

/// Facts established for a single directory entry. A directory whose children
/// could not be enumerated must never be assumed to be a leaf.
#[derive(Clone, Debug)]
pub struct DirectoryFact {
    pub path: PathBuf,
    pub identity: FileIdentity,
    pub enumeration_complete: bool,
    pub has_subdirectories: bool,
}

impl DirectoryFact {
    fn skipped(directory: &PendingDirectory) -> Self {
        Self {
            path: directory.path.clone(),
            identity: directory.identity,
            enumeration_complete: false,
            has_subdirectories: false,
        }
    }
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
    scan_impl(root, cancel, progress, None)
}

pub(crate) fn scan_restricted(
    root: PathBuf,
    expected_real: &Path,
    expected_identity: FileIdentity,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<ScanProgress>>,
) -> Result<ScanResult, String> {
    scan_impl(
        root,
        cancel,
        progress,
        Some((expected_real, expected_identity)),
    )
}

fn scan_impl(
    root: PathBuf,
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<ScanProgress>>,
    restriction: Option<(&Path, FileIdentity)>,
) -> Result<ScanResult, String> {
    let started = Instant::now();
    let metadata = fs::symlink_metadata(&root).map_err(|e| root_error(&e))?;
    if !metadata.is_dir() || is_link_or_reparse(&metadata) {
        return Err("扫描位置不是可读取的真实目录；不扫描符号链接。".into());
    }
    *progress.lock().unwrap_or_else(|e| e.into_inner()) = ScanProgress::default();
    let real_root = fs::canonicalize(&root).map_err(|e| root_error(&e))?;
    let root_identity = FileIdentity::from_path(&root, &metadata).map_err(|e| root_error(&e))?;
    if restriction.is_some_and(|(expected_real, expected_identity)| {
        expected_real != real_root || expected_identity != root_identity
    }) {
        return Err("演示目录已更改，请重置演示。".into());
    }
    let excluded_trash = excluded_trash_path();
    if excluded_trash.as_ref().is_some_and(|trash| {
        is_excluded_trash(&root, trash) || is_excluded_trash(&real_root, trash)
    }) {
        return Ok(ScanResult {
            root: root.clone(),
            files: Vec::new(),
            directories: vec![DirectoryFact::skipped(&PendingDirectory {
                path: root.clone(),
                identity: root_identity,
            })]
            .into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: started.elapsed(),
        });
    }
    #[cfg(target_os = "macos")]
    match macos::trash_relationship(&real_root) {
        Ok(true) => {
            return Ok(ScanResult {
                root: root.clone(),
                files: Vec::new(),
                directories: vec![DirectoryFact::skipped(&PendingDirectory {
                    path: root.clone(),
                    identity: root_identity,
                })]
                .into(),
                skipped: 0,
                errors: Vec::new(),
                elapsed: started.elapsed(),
            });
        }
        Ok(false) => {}
        Err(()) => return Err("无法确认扫描位置是否位于废纸篓，请检查磁盘和访问权限。".into()),
    }
    let traversal = Traversal {
        root: &root,
        real_root: &real_root,
        root_device: root_identity.device,
        startup_disk: cfg!(target_os = "macos") && root == Path::new("/"),
        excluded_trash,
        cancel: &cancel,
        progress: &progress,
        state: Mutex::new(TraversalState::default()),
    };
    let mut next = Some(PendingDirectory {
        identity: root_identity,
        path: root.clone(),
    });
    while let Some(directory) = next {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let new_identity = traversal
            .state
            .lock()
            .unwrap()
            .visited
            .insert(directory.identity);
        if !new_identity {
            publish(
                &progress,
                ScanProgress {
                    skipped: 1,
                    ..Default::default()
                },
            );
            break;
        }
        // Schedule descendants immediately, giving earlier discoveries priority
        // on each worker. Finish this phase before the Data aliases can claim identities.
        rayon::scope_fifo(|scope| traversal.spawn(scope, directory));
        next = traversal.state.lock().unwrap().deferred_data.take();
    }
    let TraversalState {
        mut files,
        directories,
        errors,
        root_failure,
        ..
    } = traversal.state.into_inner().unwrap();
    if let Some(error) = root_failure {
        return Err(error);
    }
    if cancel.load(Ordering::Relaxed) {
        return Err("扫描已取消。".into());
    }
    sort_scanned_files(&mut files);
    if cancel.load(Ordering::Relaxed) {
        return Err("扫描已取消。".into());
    }
    let state = *progress.lock().unwrap_or_else(|e| e.into_inner());
    Ok(ScanResult {
        root,
        files,
        directories: directories.into(),
        skipped: state.skipped,
        errors,
        elapsed: started.elapsed(),
    })
}

fn sort_scanned_files(files: &mut [FileEntry]) {
    files.par_sort_unstable_by(|a, b| {
        b.size.cmp(&a.size).then_with(|| {
            #[cfg(unix)]
            {
                compare_scanned_paths(&a.path, &b.path)
            }
            #[cfg(not(unix))]
            {
                a.path.cmp(&b.path)
            }
        })
    });
}

#[cfg(unix)]
fn compare_scanned_paths(a: &Path, b: &Path) -> std::cmp::Ordering {
    let a = a.as_os_str().as_encoded_bytes();
    let b = b.as_os_str().as_encoded_bytes();
    // Every path starts with the same root, followed by single separators and
    // ordinary directory entries. Skip common bytes in fixed-size chunks.
    let mut offset = 0;
    for (left, right) in a.as_chunks::<16>().0.iter().zip(b.as_chunks::<16>().0) {
        if left != right {
            break;
        }
        offset += 16;
    }
    for (&left, &right) in a[offset..].iter().zip(&b[offset..]) {
        if left != right {
            // Path ordering compares components. An ended component sorts
            // before any continuation of that component, including a hyphen.
            let component_byte = |byte| if byte == b'/' { 0 } else { byte };
            return component_byte(left).cmp(&component_byte(right));
        }
    }
    a.len().cmp(&b.len())
}

#[derive(Default)]
struct TraversalState {
    visited: HashSet<DirectoryIdentity>,
    files: Vec<FileEntry>,
    directories: Vec<DirectoryFact>,
    errors: Vec<String>,
    deferred_data: Option<PendingDirectory>,
    root_failure: Option<String>,
}

struct Traversal<'a> {
    root: &'a Path,
    real_root: &'a Path,
    root_device: u64,
    startup_disk: bool,
    excluded_trash: Option<PathBuf>,
    cancel: &'a AtomicBool,
    progress: &'a Mutex<ScanProgress>,
    state: Mutex<TraversalState>,
}

impl Traversal<'_> {
    fn spawn<'scope>(&'scope self, scope: &rayon::ScopeFifo<'scope>, directory: PendingDirectory) {
        scope.spawn_fifo(move |scope| {
            if self.cancel.load(Ordering::Relaxed) {
                return;
            }
            #[cfg(target_os = "macos")]
            if directory.path != self.root {
                let real_path = if self.root == self.real_root {
                    directory.path.clone()
                } else {
                    self.real_path(&directory.path)
                };
                if self
                    .skip_trash_or_unknown(&directory.path, macos::trash_relationship(&real_path))
                {
                    self.state
                        .lock()
                        .unwrap()
                        .directories
                        .push(DirectoryFact::skipped(&directory));
                    return;
                }
            }
            let mut report = scan_directory(&directory, self.cancel, self.progress);
            let enumeration_complete = report.errors.is_empty()
                && !report.uncertain_entries
                && report.unreadable.is_none();
            let has_subdirectories = !report.directories.is_empty();
            let parent_is_leaf = enumeration_complete && !has_subdirectories;
            // All entries share this validated directory. Checking its path once
            // avoids walking the same ancestor components for every file. Keep
            // the exact path check for special names and protected parents.
            #[cfg(target_os = "macos")]
            let parent_can_trash = crate::file_actions::can_move_to_trash(&directory.path)
                && (self.root == self.real_root
                    || crate::file_actions::can_move_to_trash(&self.real_path(&directory.path)));
            for file in &mut report.files {
                file.parent_identity = directory.identity;
                file.parent_is_leaf = parent_is_leaf;
                #[cfg(target_os = "macos")]
                let ordinary_child = ordinary_trash_child(&file.path, parent_can_trash);
                #[cfg(not(target_os = "macos"))]
                let ordinary_child = false;
                file.can_trash = ordinary_child
                    || (crate::file_actions::can_move_to_trash(&file.path)
                        && (self.root == self.real_root
                            || crate::file_actions::can_move_to_trash(
                                &self.real_path(&file.path),
                            )));
            }
            let mut skipped = 0;
            let children: Vec<_> = {
                let mut state = self.state.lock().unwrap();
                if let Some(error) = report.unreadable
                    && directory.path == self.root
                {
                    state.root_failure = Some(root_error(&error));
                    return;
                }
                state.directories.push(DirectoryFact {
                    path: directory.path.clone(),
                    identity: directory.identity,
                    enumeration_complete,
                    has_subdirectories,
                });
                if state.files.is_empty() {
                    state.files = report.files;
                } else {
                    state.files.append(&mut report.files);
                }
                let retain_errors = 24usize.saturating_sub(state.errors.len());
                state
                    .errors
                    .extend(report.errors.into_iter().take(retain_errors));
                report
                    .directories
                    .into_iter()
                    .filter_map(|child| self.queue_child(&mut state, child, &mut skipped))
                    .collect()
            };
            if skipped != 0 {
                publish(
                    self.progress,
                    ScanProgress {
                        skipped,
                        ..Default::default()
                    },
                );
            }
            for child in children {
                if self.cancel.load(Ordering::Relaxed) {
                    break;
                }
                self.spawn(scope, child);
            }
        });
    }

    fn real_path(&self, path: &Path) -> PathBuf {
        mapped_real_path(path, self.root, self.real_root)
    }

    #[cfg(target_os = "macos")]
    fn skip_trash_or_unknown(&self, path: &Path, outcome: Result<bool, ()>) -> bool {
        if outcome == Ok(false) {
            return false;
        }
        if outcome.is_err() {
            let mut state = self.state.lock().unwrap();
            if state.errors.len() < 24 {
                state.errors.push(format!(
                    "{}：无法确认是否位于废纸篓，已跳过；请检查磁盘和访问权限。",
                    path.display()
                ));
            }
        }
        publish(
            self.progress,
            ScanProgress {
                skipped: 1,
                ..Default::default()
            },
        );
        true
    }

    fn queue_child(
        &self,
        state: &mut TraversalState,
        child: PendingDirectory,
        skipped: &mut u64,
    ) -> Option<PendingDirectory> {
        if self.excluded_trash.as_ref().is_some_and(|trash| {
            is_excluded_trash(&child.path, trash)
                || (self.root != self.real_root
                    && is_excluded_trash(&self.real_path(&child.path), trash))
        }) || (cfg!(windows) && child.identity.device != self.root_device)
            || (self.startup_disk && excluded_from_startup_disk(&child.path))
        {
            state.directories.push(DirectoryFact::skipped(&child));
            *skipped += 1;
        } else if self.startup_disk && child.path == Path::new("/System/Volumes/Data") {
            state.deferred_data = Some(child);
        } else if state.visited.insert(child.identity) {
            return Some(child);
        } else {
            state.directories.push(DirectoryFact::skipped(&child));
            *skipped += 1;
        }
        None
    }
}

#[cfg(target_os = "macos")]
fn ordinary_trash_child(path: &Path, parent_can_trash: bool) -> bool {
    parent_can_trash
        && path
            .file_name()
            .is_some_and(|name| name != ".Trash" && name != ".Trashes" && name != "System")
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
    // Reparse points, links and special entries may conceal a directory.
    uncertain_entries: bool,
}

enum EntryReport {
    File(FileEntry),
    Directory(PendingDirectory),
    Skipped,
    Error(PathBuf, std::io::Error),
    Cancelled,
}

fn scan_directory(
    directory: &PendingDirectory,
    cancel: &AtomicBool,
    progress: &Mutex<ScanProgress>,
) -> DirectoryReport {
    #[cfg(target_os = "macos")]
    if let Some(report) = macos::scan_directory(directory, cancel, progress) {
        return report;
    }
    scan_directory_batches(directory, cancel, progress, || {})
}

/// Refresh just one directory after a child was removed. Identity is checked
/// on both sides of enumeration so a replacement cannot supply new facts.
pub(crate) fn directory_fact_after_change(
    path: &Path,
    expected: FileIdentity,
) -> Result<DirectoryFact, String> {
    let identity = || -> Result<FileIdentity, String> {
        let metadata = fs::symlink_metadata(path).map_err(|e| root_error(&e))?;
        if !metadata.is_dir() || is_link_or_reparse(&metadata) {
            return Err("文件夹已更改，请重新扫描。".into());
        }
        FileIdentity::from_path(path, &metadata).map_err(|e| root_error(&e))
    };
    if identity()? != expected {
        return Err("文件夹已更改，请重新扫描。".into());
    }
    let pending = PendingDirectory {
        path: path.into(),
        identity: expected,
    };
    let report = scan_directory(
        &pending,
        &AtomicBool::new(false),
        &Mutex::new(ScanProgress::default()),
    );
    if report.unreadable.is_some()
        || !report.errors.is_empty()
        || report.uncertain_entries
        || identity()? != expected
    {
        return Err("无法确认文件夹状态，请重新扫描。".into());
    }
    Ok(DirectoryFact {
        path: path.into(),
        identity: expected,
        enumeration_complete: true,
        has_subdirectories: !report.directories.is_empty(),
    })
}

fn scan_directory_batches(
    pending: &PendingDirectory,
    cancel: &AtomicBool,
    progress: &Mutex<ScanProgress>,
    after_batch: impl Fn(),
) -> DirectoryReport {
    let directory = pending.path.as_path();
    let mut report = DirectoryReport::default();
    if cancel.load(Ordering::Relaxed) {
        return report;
    }
    // Revalidate queued directories before enumeration: a junction introduced
    // after its parent was scanned must not redirect traversal to another tree.
    match fs::symlink_metadata(directory) {
        Ok(metadata) if !is_link_or_reparse(&metadata) && metadata.is_dir() => {}
        Ok(_) => {
            report.uncertain_entries = true;
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
    // Preserve Win32's directory access decision before opening the separate
    // metadata anchor. The original iterator supplies every name.
    #[cfg(windows)]
    let query = windows_ntfs::DirectoryQuery::open(pending);
    let parent: Arc<Path> = directory.into();
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
                #[cfg(windows)]
                if let Some(entry) = query
                    .as_ref()
                    .and_then(|query| query.entry(&entry, &parent))
                {
                    return entry;
                }
                scan_entry(entry.path(), &parent)
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
                EntryReport::Skipped => {
                    delta.skipped += 1;
                    report.uncertain_entries = true;
                }
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

fn scan_entry(path: PathBuf, parent: &Arc<Path>) -> EntryReport {
    // Windows directory records can carry stale attributes and authorize
    // listing independently of per-file FILE_READ_ATTRIBUTES. Read identity
    // and current metadata from the same no-follow handle.
    #[cfg(windows)]
    if let Ok(file) = open_windows_metadata(&path)
        && let Ok(metadata) = file.metadata()
    {
        return report_entry(path, parent, &metadata, |_, _| {
            windows_handle_identity(&file)
        });
    }
    // Preserve reparse skips and diagnostics when the Win32 metadata open
    // fails, including entries visible by name but denied on an attribute open.
    match fs::symlink_metadata(&path) {
        Ok(metadata) => report_entry(path, parent, &metadata, FileIdentity::from_path),
        Err(error) => EntryReport::Error(path, error),
    }
}

fn report_entry(
    path: PathBuf,
    parent: &Arc<Path>,
    metadata: &fs::Metadata,
    identity: impl FnOnce(&Path, &fs::Metadata) -> std::io::Result<FileIdentity>,
) -> EntryReport {
    if is_link_or_reparse(metadata) || (!metadata.is_dir() && !metadata.is_file()) {
        return EntryReport::Skipped;
    }
    let identity = match identity(&path, metadata) {
        Ok(identity) => identity,
        Err(error) => return EntryReport::Error(path, error),
    };
    if metadata.is_dir() {
        EntryReport::Directory(PendingDirectory { identity, path })
    } else {
        EntryReport::File(FileEntry {
            parent_identity: FileIdentity::default(),
            parent_is_leaf: false,
            identity,
            can_trash: true,
            size: metadata.len(),
            path,
            parent: Arc::clone(parent),
        })
    }
}

fn publish(progress: &Mutex<ScanProgress>, delta: ScanProgress) {
    if delta.files == 0 && delta.bytes == 0 && delta.skipped == 0 {
        return;
    }
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
fn open_windows_metadata(path: &Path) -> std::io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    // FILE_READ_ATTRIBUTES only: scanning never requires reading file contents.
    // Share deletion as well as reads/writes so the scan cannot block Explorer.
    fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS).0)
        .open(path)
}

#[cfg(windows)]
fn windows_identity(path: &Path, expected: &fs::Metadata) -> std::io::Result<FileIdentity> {
    use std::os::windows::fs::MetadataExt;
    if is_link_or_reparse(expected) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    let file = open_windows_metadata(path)?;
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
    windows_handle_identity(&file)
}

#[cfg(windows)]
fn windows_handle_identity(file: &fs::File) -> std::io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_ID_INFO, FileIdInfo, GetFileInformationByHandle,
            GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
        },
    };
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

/// The Data volume is also traversed through its visible startup-disk paths.
pub(crate) fn startup_data_relative(path: &Path) -> &Path {
    path.strip_prefix("/System/Volumes/Data")
        .or_else(|_| path.strip_prefix("/"))
        .unwrap_or(path)
}

fn mapped_real_path(path: &Path, root: &Path, real_root: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(|suffix| real_root.join(suffix))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn is_excluded_trash(path: &Path, trash: &Path) -> bool {
    startup_data_relative(path).starts_with(startup_data_relative(trash))
}

fn excluded_trash_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        crate::file_actions::home_directory()
            .ok()
            .map(|home| fs::canonicalize(&home).unwrap_or(home).join(".Trash"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
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

    #[cfg(target_os = "macos")]
    #[test]
    fn shared_parent_trash_guard_matches_full_path_checks() {
        for (root, real_root, parent) in [
            ("/", "/", "/"),
            ("/", "/", "/System"),
            ("/", "/", "/System/Volumes/Data"),
            ("/", "/", "/Users/me/.Trash"),
            ("/", "/", "/Users/me/Documents"),
            ("/alias", "/System", "/alias/Library"),
            ("/alias", "/Users/me", "/alias/Documents"),
        ] {
            let root = Path::new(root);
            let real_root = Path::new(real_root);
            let parent = Path::new(parent);
            let parent_allowed = crate::file_actions::can_move_to_trash(parent)
                && (root == real_root
                    || crate::file_actions::can_move_to_trash(&mapped_real_path(
                        parent, root, real_root,
                    )));
            for name in ["file", "System", ".Trash", ".Trashes", "文件"] {
                let path = parent.join(name);
                let exact = crate::file_actions::can_move_to_trash(&path)
                    && (root == real_root
                        || crate::file_actions::can_move_to_trash(&mapped_real_path(
                            &path, root, real_root,
                        )));
                assert_eq!(
                    ordinary_trash_child(&path, parent_allowed) || exact,
                    exact,
                    "{} mapped from {} to {}",
                    path.display(),
                    root.display(),
                    real_root.display()
                );
            }
        }
    }
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
    fn refreshed_parent_fact_detects_children_and_rejects_wrong_identity() {
        let temp = Temp::new();
        fs::create_dir(temp.0.join("empty-child")).unwrap();
        let metadata = fs::symlink_metadata(&temp.0).unwrap();
        let identity = FileIdentity::from_path(&temp.0, &metadata).unwrap();
        let fact = directory_fact_after_change(&temp.0, identity).unwrap();
        assert!(fact.enumeration_complete && fact.has_subdirectories);
        assert!(directory_fact_after_change(&temp.0, FileIdentity::default()).is_err());
    }

    #[test]
    fn scan_retains_empty_and_non_leaf_directory_facts() {
        let temp = Temp::new();
        let empty = temp.0.join("empty");
        let parent = temp.0.join("parent");
        fs::create_dir(&empty).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::create_dir(parent.join("child")).unwrap();
        fs::write(parent.join("file"), b"data").unwrap();
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(ScanProgress::default())),
        )
        .unwrap();
        let fact = |path: &Path| result.directories.iter().find(|d| d.path == path).unwrap();
        assert!(fact(&temp.0).enumeration_complete);
        assert!(fact(&temp.0).has_subdirectories);
        assert!(fact(&parent).enumeration_complete);
        assert!(fact(&parent).has_subdirectories);
        assert!(fact(&empty).enumeration_complete);
        assert!(!fact(&empty).has_subdirectories);
        assert_eq!(fact(&empty).identity, directory_identity(&empty).unwrap());
        assert!(fact(&parent.join("child")).enumeration_complete);
    }

    #[cfg(unix)]
    #[test]
    fn unclassified_entry_makes_directory_fact_incomplete() {
        let temp = Temp::new();
        std::os::unix::fs::symlink("missing", temp.0.join("link")).unwrap();
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(ScanProgress::default())),
        )
        .unwrap();
        let fact = result
            .directories
            .iter()
            .find(|d| d.path == temp.0)
            .unwrap();
        assert!(!fact.enumeration_complete);
        assert!(!fact.has_subdirectories);
    }

    #[test]
    fn skipped_child_is_retained_as_an_incomplete_directory_fact() {
        let cancel = AtomicBool::new(false);
        let progress = Mutex::new(ScanProgress::default());
        let traversal = Traversal {
            root: Path::new("/"),
            real_root: Path::new("/"),
            root_device: 1,
            startup_disk: true,
            excluded_trash: None,
            cancel: &cancel,
            progress: &progress,
            state: Mutex::default(),
        };
        let mut state = TraversalState::default();
        let mut skipped = 0;
        let child = PendingDirectory {
            path: "/Volumes".into(),
            identity: FileIdentity {
                device: 1,
                inode: 42,
            },
        };
        assert!(
            traversal
                .queue_child(&mut state, child, &mut skipped)
                .is_none()
        );
        assert_eq!(skipped, 1);
        assert_eq!(state.directories.len(), 1);
        assert_eq!(state.directories[0].path, Path::new("/Volumes"));
        assert!(!state.directories[0].enumeration_complete);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn real_root_mapping_handles_symlink_ancestors_without_changing_display_paths() {
        let temp = Temp::new();
        std::os::unix::fs::symlink("/System", temp.0.join("system-link")).unwrap();
        let root = temp.0.join("system-link/Library");
        let real_root = fs::canonicalize(&root).unwrap();
        let displayed = root.join("Fonts/example");
        let actual = mapped_real_path(&displayed, &root, &real_root);
        assert_eq!(displayed, temp.0.join("system-link/Library/Fonts/example"));
        assert!(!crate::file_actions::can_move_to_trash(&actual));

        fs::create_dir_all(temp.0.join("home/.Trash/inside")).unwrap();
        std::os::unix::fs::symlink(temp.0.join("home"), temp.0.join("home-link")).unwrap();
        let root = temp.0.join("home-link/.Trash/inside");
        let real_root = fs::canonicalize(&root).unwrap();
        let trash = fs::canonicalize(temp.0.join("home/.Trash")).unwrap();
        assert!(is_excluded_trash(&real_root, &trash));
        assert!(is_excluded_trash(
            &mapped_real_path(&root.join("file"), &root, &real_root),
            &trash
        ));
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
    fn trash_exclusion_matches_home_and_data_alias_only() {
        let trash = Path::new("/Users/me/.Trash");
        for path in [
            "/Users/me/.Trash",
            "/Users/me/.Trash/file",
            "/System/Volumes/Data/Users/me/.Trash/file",
        ] {
            assert!(is_excluded_trash(Path::new(path), trash), "{path}");
        }
        for path in ["/Users/me/.Trash-other/file", "/Users/other/.Trash/file"] {
            assert!(!is_excluded_trash(Path::new(path), trash), "{path}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn foundation_trash_check_excludes_real_trash_but_not_same_named_directory() {
        let home = crate::file_actions::home_directory().unwrap();
        let trash = home.join(".Trash");
        if trash.is_dir() {
            assert_eq!(macos::trash_relationship(&trash), Ok(true));
            let result = scan(
                trash,
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(ScanProgress::default())),
            )
            .unwrap();
            assert!(result.files.is_empty());
        }

        let temp = Temp::new();
        let ordinary = temp.0.join(".Trash");
        fs::create_dir(&ordinary).unwrap();
        fs::write(ordinary.join("file"), b"data").unwrap();
        assert_eq!(macos::trash_relationship(&ordinary), Ok(false));
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(ScanProgress::default())),
        )
        .unwrap();
        assert!(
            result
                .files
                .iter()
                .any(|entry| entry.path == ordinary.join("file"))
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn unknown_trash_relationship_skips_directory_and_reports_reason() {
        let cancel = AtomicBool::new(false);
        let progress = Mutex::new(ScanProgress::default());
        let traversal = Traversal {
            root: Path::new("/"),
            real_root: Path::new("/"),
            root_device: 1,
            startup_disk: true,
            excluded_trash: None,
            cancel: &cancel,
            progress: &progress,
            state: Mutex::new(TraversalState::default()),
        };
        assert!(!traversal.skip_trash_or_unknown(Path::new("/ordinary"), Ok(false)));
        assert!(traversal.skip_trash_or_unknown(Path::new("/trash"), Ok(true)));
        assert!(traversal.skip_trash_or_unknown(Path::new("/unavailable"), Err(())));
        assert_eq!(progress.lock().unwrap().skipped, 2);
        let state = traversal.state.lock().unwrap();
        assert_eq!(state.errors.len(), 1);
        assert!(state.errors[0].contains("/unavailable"));
        assert!(state.errors[0].contains("已跳过"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn startup_data_root_does_not_claim_identity_before_canonical_phase() {
        let cancel = AtomicBool::new(false);
        let progress = Mutex::new(ScanProgress::default());
        let traversal = Traversal {
            root: Path::new("/"),
            real_root: Path::new("/"),
            root_device: 1,
            startup_disk: true,
            excluded_trash: None,
            cancel: &cancel,
            progress: &progress,
            state: Mutex::default(),
        };
        let identity = FileIdentity {
            device: 1,
            inode: 42,
        };
        let child = |path: &str| PendingDirectory {
            path: path.into(),
            identity,
        };
        let mut state = traversal.state.lock().unwrap();
        let mut skipped = 0;
        assert!(
            traversal
                .queue_child(&mut state, child("/System/Volumes/Data"), &mut skipped)
                .is_none()
        );
        assert!(
            traversal
                .queue_child(&mut state, child("/canonical"), &mut skipped)
                .is_some()
        );
        assert_eq!(skipped, 0);
        let deferred = state.deferred_data.take().unwrap();
        assert!(!state.visited.insert(deferred.identity));
    }

    #[cfg(windows)]
    #[test]
    fn traversal_skips_other_windows_volumes_before_claiming_identity() {
        let cancel = AtomicBool::new(false);
        let progress = Mutex::new(ScanProgress::default());
        let traversal = Traversal {
            root: Path::new(r"C:\"),
            real_root: Path::new(r"C:\"),
            root_device: 1,
            startup_disk: false,
            excluded_trash: None,
            cancel: &cancel,
            progress: &progress,
            state: Mutex::default(),
        };
        let identity = FileIdentity {
            device: 2,
            inode: 42,
        };
        let child = PendingDirectory {
            path: r"C:\mount".into(),
            identity,
        };
        let mut state = traversal.state.lock().unwrap();
        let mut skipped = 0;
        assert!(
            traversal
                .queue_child(&mut state, child, &mut skipped)
                .is_none()
        );
        assert_eq!(skipped, 1);
        assert!(!state.visited.contains(&identity));
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
        assert_eq!(result.files[0].parent.as_ref(), temp.0.join("child"));
        assert!(Arc::ptr_eq(
            &result.files[1].parent,
            &result.files[2].parent
        ));
        assert_eq!(result.skipped, 0);
        assert_eq!(progress.lock().unwrap().bytes, 13);
        assert_eq!(progress.lock().unwrap().files, 3);
    }

    #[test]
    fn leaf_detection_counts_empty_child_directories() {
        let temp = Temp::new();
        fs::create_dir_all(temp.0.join("branch/empty")).unwrap();
        fs::create_dir_all(temp.0.join("leaf")).unwrap();
        fs::write(temp.0.join("branch/a"), b"a").unwrap();
        fs::write(temp.0.join("leaf/b"), b"b").unwrap();
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(ScanProgress::default())),
        )
        .unwrap();
        assert!(
            !result
                .files
                .iter()
                .find(|f| f.path.ends_with("branch/a"))
                .unwrap()
                .parent_is_leaf
        );
        assert!(
            result
                .files
                .iter()
                .find(|f| f.path.ends_with("leaf/b"))
                .unwrap()
                .parent_is_leaf
        );
    }

    #[cfg(unix)]
    #[test]
    fn leaf_detection_is_conservative_about_directory_symlinks() {
        use std::os::unix::fs::symlink;
        let temp = Temp::new();
        fs::create_dir(temp.0.join("directory")).unwrap();
        fs::write(temp.0.join("file"), b"a").unwrap();
        symlink(temp.0.join("directory"), temp.0.join("link")).unwrap();
        let result = scan(
            temp.0.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(ScanProgress::default())),
        )
        .unwrap();
        assert!(
            !result
                .files
                .iter()
                .find(|f| f.path.ends_with("file"))
                .unwrap()
                .parent_is_leaf
        );
    }

    #[cfg(unix)]
    #[test]
    fn scanned_path_order_matches_component_order() {
        use std::os::unix::ffi::OsStrExt;

        let root = Path::new("./parent//./long-common-directory-prefix");
        let paths = [
            b"a/file".as_slice(),
            b"a-b/file",
            b"a.b/file",
            b"a0/file",
            b"ancestor/child/file-a",
            b"ancestor/child/file",
            b"ancestor/file",
            b"ancestor/\xff",
        ]
        .map(|bytes| root.join(std::ffi::OsStr::from_bytes(bytes)));
        for a in &paths {
            for b in &paths {
                assert_eq!(compare_scanned_paths(a, b), a.cmp(b), "{a:?} and {b:?}");
            }
        }
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

    #[cfg(target_os = "macos")]
    #[test]
    fn bulk_metadata_matches_portable_scan_for_sparse_files_and_hard_links() {
        let temp = Temp::new();
        let sparse = temp.0.join("稀疏文件");
        fs::File::create(&sparse).unwrap().set_len(1 << 24).unwrap();
        fs::hard_link(&sparse, temp.0.join("hard-link")).unwrap();
        std::os::unix::fs::symlink(&sparse, temp.0.join("symlink")).unwrap();
        fs::create_dir(temp.0.join("child")).unwrap();

        let pending = PendingDirectory {
            identity: directory_identity(&temp.0).unwrap(),
            path: temp.0.clone(),
        };
        let cancel = AtomicBool::new(false);
        let native_progress = Mutex::new(ScanProgress::default());
        let portable_progress = Mutex::new(ScanProgress::default());
        let bulk = macos::scan_directory(&pending, &cancel, &native_progress)
            .expect("native enumeration should work on the temporary filesystem");
        let portable = scan_directory_batches(&pending, &cancel, &portable_progress, || {});
        let files = |report: DirectoryReport| {
            let mut files: Vec<_> = report
                .files
                .into_iter()
                .map(|file| (file.path, file.parent, file.size, file.identity))
                .collect();
            files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            files
        };
        assert!(bulk.errors.is_empty());
        assert_eq!(bulk.directories.len(), 1);
        assert_eq!(bulk.directories[0].path, portable.directories[0].path);
        assert_eq!(
            bulk.directories[0].identity,
            portable.directories[0].identity
        );
        assert_eq!(files(bulk), files(portable));
        let progress = native_progress.into_inner().unwrap();
        assert_eq!(
            (progress.files, progress.bytes, progress.skipped),
            (2, 1 << 25, 1)
        );
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
    fn windows_scan_reads_current_size_from_both_hard_links() {
        let temp = Temp::new();
        let first = temp.0.join("original");
        let directory = temp.0.join("child");
        fs::create_dir(&directory).unwrap();
        let link = directory.join("hard-link");
        fs::write(&first, b"old size").unwrap();
        fs::hard_link(&first, &link).unwrap();
        // NTFS can keep the old size in the other name's directory record.
        let writer = fs::OpenOptions::new().write(true).open(&first).unwrap();
        writer.set_len(4096).unwrap();

        // Verify the native query itself on supported NTFS, so fallback alone
        // cannot make this test pass if it returns stale identity or size.
        if let Some(file) = windows_ntfs::query_fixture_file(&link) {
            let metadata = fs::symlink_metadata(&link).unwrap();
            assert_eq!(file.size, 4096);
            assert_eq!(
                file.identity,
                FileIdentity::from_path(&link, &metadata).unwrap()
            );
        }
        let result = scan(temp.0.clone(), Arc::default(), Arc::default()).unwrap();
        assert_eq!(result.skipped, 0);
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].path, link);
        assert_eq!(result.files[1].path, first);
        assert_eq!(result.files[0].identity, result.files[1].identity);
        assert!(result.files.iter().all(|file| file.size == 4096));
    }

    #[cfg(windows)]
    #[test]
    fn windows_scan_preserves_win32_name_resolution() {
        let mut temp = Temp::new();
        let ordinary = temp.0.clone();
        // Keep the cleanup path verbatim even if an assertion fails.
        temp.0 = fs::canonicalize(&ordinary).unwrap();
        let verbatim = &temp.0;
        fs::write(ordinary.join("file"), b"normal").unwrap();
        fs::write(verbatim.join("file. "), b"verbatim").unwrap();

        for name in [
            "file. ",
            "file.",
            "NUL.txt",
            "CoM¹.txt",
            "LPT² .log",
            "CONIN$",
        ] {
            let wide: Vec<_> = name.encode_utf16().collect();
            assert!(windows_ntfs::requires_win32_normalization(&wide));
        }
        for name in ["ordinary.txt", "COM10.txt", "auxiliary.log"] {
            let wide: Vec<_> = name.encode_utf16().collect();
            assert!(!windows_ntfs::requires_win32_normalization(&wide));
        }

        let parent: Arc<Path> = ordinary.as_path().into();
        let alias = ordinary.join("file. ");
        let EntryReport::File(expected) = scan_entry(alias.clone(), &parent) else {
            panic!("existing Windows lookup must resolve one of the two fixture files");
        };
        let normal = scan(ordinary, Arc::default(), Arc::default()).unwrap();
        let actual = normal.files.iter().find(|file| file.path == alias).unwrap();
        assert_eq!(
            (actual.size, actual.identity),
            (expected.size, expected.identity)
        );

        let exact = scan(verbatim.clone(), Arc::default(), Arc::default()).unwrap();
        let literal = exact
            .files
            .iter()
            .find(|file| file.path == verbatim.join("file. "))
            .unwrap();
        assert_eq!(literal.size, 8);
    }

    #[cfg(windows)]
    #[test]
    fn windows_scan_matches_legacy_attribute_access_for_restricted_files() {
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        fn icacls(path: &Path, args: [&str; 2]) -> std::io::Result<std::process::Output> {
            Command::new("icacls.exe")
                .arg(path)
                .args(args)
                .creation_flags(0x0800_0000)
                .output()
        }

        struct ClearReadDeny<'a>(&'a Path);
        impl Drop for ClearReadDeny<'_> {
            fn drop(&mut self) {
                let _ = icacls(self.0, ["/remove:d", "*S-1-1-0"]);
            }
        }

        let temp = Temp::new();
        let path = temp.0.join("restricted-file");
        fs::write(&path, b"accessible directory entry").unwrap();
        let _clear_deny = ClearReadDeny(&path);
        let deny = icacls(&path, ["/deny", "*S-1-1-0:(RD,RA)"]).unwrap();
        assert!(deny.status.success(), "could not set the file's deny ACE");

        assert!(
            fs::read_dir(&temp.0)
                .unwrap()
                .any(|entry| entry.unwrap().path() == path)
        );
        assert_eq!(
            fs::File::open(&path).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );

        // An attributes-only handle can still succeed under some Windows
        // tokens. The independent content-read check above verifies the fixture.
        let legacy = fs::symlink_metadata(&path).and_then(|metadata| {
            FileIdentity::from_path(&path, &metadata).map(|identity| (metadata.len(), identity))
        });
        let result = scan(temp.0.clone(), Arc::default(), Arc::default()).unwrap();
        match legacy {
            Ok((size, identity)) => {
                assert_eq!(result.files.len(), 1);
                assert_eq!(result.files[0].path, path);
                assert_eq!(result.files[0].size, size);
                assert_eq!(result.files[0].identity, identity);
                assert_eq!(result.skipped, 0);
                assert!(result.errors.is_empty());
            }
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                assert!(result.files.is_empty());
                assert_eq!(result.skipped, 1);
                assert_eq!(result.errors, [format!("{}：无访问权限", path.display())]);
            }
        }
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
        let pending = PendingDirectory {
            identity: directory_identity(&temp.0).unwrap(),
            path: temp.0.clone(),
        };
        let report = scan_directory_batches(&pending, &cancel, &progress, || {
            cancel.store(true, Ordering::Relaxed);
        });
        assert_eq!(report.files.len(), 512);
        assert_eq!(progress.lock().unwrap().files, 512);
        assert!(cancel.load(Ordering::Relaxed));
    }
}
