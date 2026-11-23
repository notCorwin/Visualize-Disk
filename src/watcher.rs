//! Native macOS FSEvents filesystem watcher for incremental directory updates.
//! Emits coalesced dirty directory paths off the UI thread and wakes egui.
use std::path::PathBuf;
#[cfg(not(target_os = "macos"))]
use std::sync::mpsc;

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Clone, Debug)]
pub enum WatchEvent {
    /// Specific directories whose direct entries may have changed.
    DirtyDirs(Vec<PathBuf>),
    /// Kernel coalesced or dropped events under this subtree; rescan subtree.
    RescanSubtree(PathBuf),
    /// The watched root itself was renamed, unmounted, or replaced.
    RootChanged,
}

#[cfg(target_os = "macos")]
mod macos {
    use super::WatchEvent;
    use std::ffi::{CStr, CString, OsStr, c_char, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;
    use std::sync::{Arc, mpsc};

    type CFIndex = isize;
    type CFAllocatorRef = *const c_void;
    type CFArrayRef = *const c_void;
    type CFStringRef = *const c_void;
    type FSEventStreamRef = *mut c_void;
    type ConstFSEventStreamRef = *const c_void;
    type DispatchQueueRef = *mut c_void;
    type FSEventStreamEventFlags = u32;
    type FSEventStreamEventId = u64;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW: u64 = 0xFFFF_FFFF_FFFF_FFFF;
    const K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER: u32 = 0x0000_0002;
    const K_FS_EVENT_STREAM_CREATE_FLAG_WATCH_ROOT: u32 = 0x0000_0004;
    const K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS: u32 = 0x0000_0010;

    const K_FLAG_MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
    const K_FLAG_USER_DROPPED: u32 = 0x0000_0002;
    const K_FLAG_KERNEL_DROPPED: u32 = 0x0000_0004;
    const K_FLAG_ROOT_CHANGED: u32 = 0x0000_0020;
    const K_FLAG_ITEM_IS_DIR: u32 = 0x0002_0000;

    #[repr(C)]
    struct FSEventStreamContext {
        version: CFIndex,
        info: *mut c_void,
        retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
        release: Option<extern "C" fn(*const c_void)>,
        copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
    }

    type FSEventStreamCallback = extern "C" fn(
        stream: ConstFSEventStreamRef,
        client_info: *mut c_void,
        num_events: usize,
        event_paths: *mut c_void,
        event_flags: *const FSEventStreamEventFlags,
        event_ids: *const FSEventStreamEventId,
    );

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeArrayCallBacks: c_void;
        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: CFIndex,
            encoding: u32,
            is_external_representation: u8,
        ) -> CFStringRef;
        fn CFArrayCreate(
            alloc: CFAllocatorRef,
            values: *const *const c_void,
            num_values: CFIndex,
            callbacks: *const c_void,
        ) -> CFArrayRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn FSEventStreamCreate(
            allocator: CFAllocatorRef,
            callback: FSEventStreamCallback,
            context: *mut FSEventStreamContext,
            paths_to_watch: CFArrayRef,
            since_when: FSEventStreamEventId,
            latency: f64,
            flags: u32,
        ) -> FSEventStreamRef;
        fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, q: DispatchQueueRef);
        fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
        fn FSEventStreamStop(stream: FSEventStreamRef);
        fn FSEventStreamInvalidate(stream: FSEventStreamRef);
        fn FSEventStreamRelease(stream: FSEventStreamRef);
    }

    unsafe extern "C" {
        fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueueRef;
        fn dispatch_sync_f(
            queue: DispatchQueueRef,
            context: *mut c_void,
            work: extern "C" fn(*mut c_void),
        );
        fn dispatch_release(object: *mut c_void);
    }

    extern "C" fn retain_context(info: *const c_void) -> *const c_void {
        if !info.is_null() {
            unsafe { Arc::increment_strong_count(info as *const CallbackState) };
        }
        info
    }

    extern "C" fn release_context(info: *const c_void) {
        if !info.is_null() {
            unsafe { drop(Arc::from_raw(info as *const CallbackState)) };
        }
    }

    extern "C" fn noop_barrier(_ctx: *mut c_void) {}

    struct CallbackState {
        root: PathBuf,
        real_root: PathBuf,
        startup_disk: bool,
        tx: mpsc::Sender<WatchEvent>,
        wake: Arc<dyn Fn() + Send + Sync>,
    }

    fn is_ignored_path(path: &Path, startup_disk: bool) -> bool {
        if startup_disk
            && (crate::scan::excluded_from_startup_disk(path)
                || path.starts_with("/System/Volumes/Data/Volumes")
                || path.starts_with("/System/Volumes/Data/dev")
                || path.starts_with("/System/Volumes/Data/Network")
                || path.starts_with("/Volumes")
                || path.starts_with("/dev")
                || path.starts_with("/Network"))
        {
            return true;
        }
        path.components().any(|c| {
            let bytes = c.as_os_str().as_bytes();
            bytes.eq_ignore_ascii_case(b".trash") || bytes.eq_ignore_ascii_case(b".trashes")
        })
    }

    fn map_to_display_root(
        path: &Path,
        root: &Path,
        real_root: &Path,
        startup_disk: bool,
    ) -> Option<PathBuf> {
        if startup_disk
            && let Ok(rel) = path.strip_prefix("/System/Volumes/Data")
            && !rel.as_os_str().is_empty()
        {
            let visible = Path::new("/").join(rel);
            if visible.exists() || visible.parent().is_some_and(Path::exists) {
                return Some(visible);
            }
        }
        if root == real_root {
            return path.starts_with(root).then(|| path.to_path_buf());
        }
        if let Ok(rel) = path.strip_prefix(real_root) {
            return Some(if rel.as_os_str().is_empty() {
                root.to_path_buf()
            } else {
                root.join(rel)
            });
        }
        path.starts_with(root).then(|| path.to_path_buf())
    }

    extern "C" fn on_fs_events(
        _stream: ConstFSEventStreamRef,
        client_info: *mut c_void,
        num_events: usize,
        event_paths: *mut c_void,
        event_flags: *const FSEventStreamEventFlags,
        _event_ids: *const FSEventStreamEventId,
    ) {
        if client_info.is_null()
            || event_paths.is_null()
            || event_flags.is_null()
            || num_events == 0
        {
            return;
        }
        // SAFETY: `client_info` is a live `CallbackState` owned until after
        // `FSEventStreamInvalidate` completes in `FsWatcher::drop`.
        let state = unsafe { &*(client_info as *const CallbackState) };
        let paths = event_paths as *const *const c_char;
        let mut dirty = std::collections::BTreeSet::new();
        let mut subtrees = Vec::new();
        let mut root_changed = false;

        for i in 0..num_events {
            // SAFETY: CoreServices guarantees `num_events` valid C strings and flags.
            let c_path = unsafe { *paths.add(i) };
            let flags = unsafe { *event_flags.add(i) };
            if c_path.is_null() {
                continue;
            }
            if flags & K_FLAG_ROOT_CHANGED != 0 {
                root_changed = true;
                break;
            }
            let raw = Path::new(OsStr::from_bytes(
                unsafe { CStr::from_ptr(c_path) }.to_bytes(),
            ));
            let Some(display) =
                map_to_display_root(raw, &state.root, &state.real_root, state.startup_disk)
            else {
                continue;
            };
            if is_ignored_path(&display, state.startup_disk) {
                continue;
            }
            if flags & (K_FLAG_MUST_SCAN_SUBDIRS | K_FLAG_USER_DROPPED | K_FLAG_KERNEL_DROPPED) != 0
            {
                subtrees.push(display);
                continue;
            }
            if flags & K_FLAG_ITEM_IS_DIR != 0 {
                dirty.insert(display.clone());
                if display != state.root
                    && let Some(parent) = display.parent()
                    && parent.starts_with(&state.root)
                {
                    dirty.insert(parent.to_path_buf());
                }
            } else if let Some(parent) = display.parent()
                && parent.starts_with(&state.root)
            {
                dirty.insert(parent.to_path_buf());
            } else if display == state.root {
                dirty.insert(display);
            }
        }

        let mut sent = false;
        if root_changed {
            sent |= state.tx.send(WatchEvent::RootChanged).is_ok();
        } else {
            for subtree in subtrees {
                sent |= state.tx.send(WatchEvent::RescanSubtree(subtree)).is_ok();
            }
            if !dirty.is_empty() {
                sent |= state
                    .tx
                    .send(WatchEvent::DirtyDirs(dirty.into_iter().collect()))
                    .is_ok();
            }
        }
        if sent {
            (state.wake)();
        }
    }

    pub struct FsWatcher {
        stream: FSEventStreamRef,
        queue: DispatchQueueRef,
    }

    // SAFETY: `FSEventStreamRef` and GCD queues are safe to stop/release from
    // the owning thread; `CallbackState` is `Arc`-refcounted by CoreServices.
    unsafe impl Send for FsWatcher {}

    impl FsWatcher {
        pub fn start(
            root: PathBuf,
            real_root: PathBuf,
            tx: mpsc::Sender<WatchEvent>,
            wake: Arc<dyn Fn() + Send + Sync>,
        ) -> Option<Self> {
            let watch_bytes = real_root.as_os_str().as_bytes().to_vec();
            let startup_disk = root == Path::new("/");
            let state = Arc::new(CallbackState {
                root,
                real_root,
                startup_disk,
                tx,
                wake,
            });
            let state_ptr = Arc::into_raw(state);
            unsafe {
                let cf_path = CFStringCreateWithBytes(
                    ptr::null(),
                    watch_bytes.as_ptr(),
                    watch_bytes.len() as CFIndex,
                    K_CF_STRING_ENCODING_UTF8,
                    0,
                );
                if cf_path.is_null() {
                    drop(Arc::from_raw(state_ptr));
                    return None;
                }
                let arr_items = [cf_path as *const c_void];
                let paths_to_watch = CFArrayCreate(
                    ptr::null(),
                    arr_items.as_ptr(),
                    1,
                    &raw const kCFTypeArrayCallBacks,
                );
                if paths_to_watch.is_null() {
                    CFRelease(cf_path);
                    drop(Arc::from_raw(state_ptr));
                    return None;
                }
                let mut ctx = FSEventStreamContext {
                    version: 0,
                    info: state_ptr.cast_mut().cast(),
                    retain: Some(retain_context),
                    release: Some(release_context),
                    copy_description: None,
                };
                let flags = K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER
                    | K_FS_EVENT_STREAM_CREATE_FLAG_WATCH_ROOT
                    | K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS;
                let stream = FSEventStreamCreate(
                    ptr::null(),
                    on_fs_events,
                    &mut ctx,
                    paths_to_watch,
                    K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
                    0.25,
                    flags,
                );
                CFRelease(paths_to_watch);
                CFRelease(cf_path);
                // CoreServices retained its own reference via `retain_context`;
                // release our initial creation reference now.
                drop(Arc::from_raw(state_ptr));
                if stream.is_null() {
                    return None;
                }
                let label = CString::new("com.visualizessd.fsevents").ok()?;
                let queue = dispatch_queue_create(label.as_ptr(), ptr::null());
                if queue.is_null() {
                    FSEventStreamRelease(stream);
                    return None;
                }
                FSEventStreamSetDispatchQueue(stream, queue);
                if FSEventStreamStart(stream) == 0 {
                    FSEventStreamInvalidate(stream);
                    FSEventStreamRelease(stream);
                    dispatch_release(queue);
                    return None;
                }
                Some(Self { stream, queue })
            }
        }
    }

    impl Drop for FsWatcher {
        fn drop(&mut self) {
            unsafe {
                FSEventStreamStop(self.stream);
                FSEventStreamInvalidate(self.stream);
                // Drain any callback already executing on the serial queue.
                dispatch_sync_f(self.queue, ptr::null_mut(), noop_barrier);
                FSEventStreamRelease(self.stream);
                dispatch_release(self.queue);
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::FsWatcher;

#[cfg(not(target_os = "macos"))]
pub struct FsWatcher;

#[cfg(not(target_os = "macos"))]
impl FsWatcher {
    pub fn start(
        _root: PathBuf,
        _real_root: PathBuf,
        _tx: mpsc::Sender<WatchEvent>,
        _wake: std::sync::Arc<dyn Fn() + Send + Sync>,
    ) -> Option<Self> {
        None
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn fsevents_detects_created_and_removed_file() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "visualize-ssd-watch-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        let real = std::fs::canonicalize(&dir).unwrap();
        let (tx, rx) = mpsc::channel();
        let watcher = FsWatcher::start(dir.clone(), real, tx, Arc::new(|| {})).unwrap();

        let file = dir.join("hello.txt");
        std::fs::write(&file, b"world").unwrap();
        let mut saw_dir = false;
        for _ in 0..20 {
            if let Ok(event) = rx.recv_timeout(Duration::from_millis(200)) {
                match event {
                    WatchEvent::DirtyDirs(dirs) if dirs.iter().any(|d| d == &dir) => {
                        saw_dir = true;
                        break;
                    }
                    WatchEvent::RescanSubtree(p) if p == dir => {
                        saw_dir = true;
                        break;
                    }
                    _ => {}
                }
            }
        }
        drop(watcher);
        let _ = std::process::Command::new("/usr/bin/trash")
            .arg(&dir)
            .status();
        assert!(saw_dir, "expected FSEvents notification for created file");
    }
}
