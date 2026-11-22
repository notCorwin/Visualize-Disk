use crate::scan::{FileEntry, FileIdentity, is_link_or_reparse};
use std::{fs, io::ErrorKind, path::Path};

const REPLACED: &str = "文件已被替换，请重新扫描后重试。";

#[cfg(target_os = "macos")]
mod recovery;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "macos")]
#[cfg_attr(test, allow(unused_imports))]
pub use recovery::{recoveries, restore};

#[derive(Clone, Debug)]
pub struct RecoveryRecord {
    pub id: String,
    pub original_path: std::path::PathBuf,
    pub trash_path: Option<std::path::PathBuf>,
    pub identity: FileIdentity,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub original_links: u64,
}

#[cfg_attr(not(windows), allow(dead_code))]
pub const SYSTEM_RECOVERY: bool = cfg!(windows);

#[cfg(not(target_os = "macos"))]
#[cfg_attr(test, allow(dead_code))]
pub fn recoveries() -> Result<Vec<RecoveryRecord>, String> {
    Ok(Vec::new())
}

#[cfg(not(target_os = "macos"))]
pub fn restore(_id: &str) -> Result<Option<FileEntry>, String> {
    Err("请在系统回收站中选择文件并还原。".into())
}

#[cfg(windows)]
pub use windows::open_recycle_bin;

/// Use the native recoverable trash operation; never fall back to permanent deletion.
pub fn move_to_trash(path: &Path, expected: FileIdentity) -> Result<Option<FileEntry>, String> {
    if is_system_path(path) || fs::canonicalize(path).is_ok_and(|p| is_system_path(&p)) {
        return Err("系统文件夹中的内容不能移到废纸篓。".into());
    }
    if already_in_trash(path) {
        return Err(if cfg!(windows) {
            "文件已在回收站中，请在系统回收站中还原；不会再次删除。"
        } else {
            "文件已在废纸篓中，请使用“放回原处”；不会再次删除。"
        }
        .into());
    }
    verify_file(path, expected)?;
    native_trash(path, expected)
}

/// Whether a scanned file may be offered as a trash action.
pub fn can_move_to_trash(path: &Path) -> bool {
    !already_in_trash(path) && !is_system_path(path)
}

fn is_system_path(path: &Path) -> bool {
    // Refuse ambiguous paths rather than letting parent components bypass the guard.
    path.components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
        || crate::scan::startup_data_relative(path).starts_with("System")
}

pub fn already_in_trash(path: &Path) -> bool {
    path.components().any(|part| {
        #[cfg(windows)]
        {
            part.as_os_str()
                .to_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("$Recycle.Bin"))
        }
        #[cfg(not(windows))]
        {
            part.as_os_str() == ".Trash" || part.as_os_str() == ".Trashes"
        }
    })
}

#[cfg(test)]
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
                if cfg!(target_os = "macos") {
                    "macOS 拒绝读取文件信息，请检查文件权限和完全磁盘访问权限。".into()
                } else {
                    "Windows 拒绝读取文件信息，请检查当前账户的文件访问权限。".into()
                }
            }
            _ => "无法读取文件信息，请确认磁盘仍已连接后重试。".into(),
        }
    })?;
    if is_link_or_reparse(&metadata) || !metadata.is_file() {
        return Err("扫描位置已不再是普通文件，请重新扫描后重试。".into());
    }
    if FileIdentity::from_path(path, &metadata)
        .map_err(|_| "无法确认文件身份，请重新扫描后重试。")?
        != expected
    {
        return Err(REPLACED.into());
    }
    Ok(())
}

#[cfg(any(target_os = "macos", all(test, unix)))]
fn protected_recovery_location(path: &Path, support: &Path) -> bool {
    path.starts_with(support)
        || fs::canonicalize(path)
            .ok()
            .zip(fs::canonicalize(support).ok())
            .is_some_and(|(p, root)| p.starts_with(root))
}

#[cfg(target_os = "macos")]
fn native_trash(path: &Path, expected: FileIdentity) -> Result<Option<FileEntry>, String> {
    let home = home_directory()?;
    let home_meta =
        fs::metadata(&home).map_err(|_| "无法确认当前用户的废纸篓位置，请在 Finder 中处理。")?;
    if FileIdentity::from_metadata(&home_meta).device != expected.device {
        return Err("此位置不支持本应用的可恢复废纸篓操作，请在 Finder 中处理。".into());
    }
    let support = home.join("Library/Application Support/com.visualizessd.app");
    if protected_recovery_location(path, &support) {
        return Err("该文件保存本应用的恢复记录，不能移到废纸篓。".into());
    }
    // This is the only place allowed to request OS consent: a user-initiated
    // action. Native tests use the separate no-prompt authorization check.
    finder::permission(true)?;
    let store = recovery::Store::production()?;
    let (mut record, fresh_record) = store.pending(path, expected)?;
    if let Err(error) = verify_file(path, expected) {
        if fresh_record {
            let _ = store.remove(&record.id);
        }
        return Err(error);
    }
    let result = finder::trash(path);
    let moved = result
        .as_ref()
        .ok()
        .and_then(|p| p.as_ref())
        .and_then(|p| destination_entry(p.clone()))
        .filter(|f| {
            f.identity == expected
                && store.is_trash_destination(&f.path)
                && verify_file(path, expected).is_err()
        })
        .or_else(|| {
            if record.original_links == 1 && verify_file(path, expected).is_err() {
                store.locate(expected)
            } else {
                None
            }
        });
    if let Some(file) = moved {
        record.trash_path = Some(file.path.clone());
        record.identity = file.identity;
        // The synced pending record remains recoverable even if this second
        // write fails after Finder committed the move.
        let _ = store.save(&record);
        return Ok(Some(file));
    }
    match result {
        Ok(_) => Ok(None),
        Err(error) if verify_file(path, expected).is_ok() => {
            if error.confirmed_rejection && fresh_record {
                let _ = store.remove(&record.id);
            }
            Err(error.message)
        }
        // A lost/late reply does not mean the move failed. Keep the durable
        // pending record and request reconciliation instead of unsafe retry.
        Err(_) => Ok(None),
    }
}

#[cfg(windows)]
fn native_trash(path: &Path, expected: FileIdentity) -> Result<Option<FileEntry>, String> {
    windows::move_to_trash(path, expected)
}

#[cfg(not(any(target_os = "macos", windows)))]
fn native_trash(_path: &Path, _expected: FileIdentity) -> Result<Option<FileEntry>, String> {
    Err("当前系统不支持安全的回收站操作。".into())
}

#[cfg(target_os = "macos")]
pub(crate) fn home_directory() -> Result<std::path::PathBuf, String> {
    use std::{
        ffi::{CStr, OsStr},
        os::unix::ffi::OsStrExt,
    };
    let home = objc2_foundation::NSHomeDirectory();
    let bytes = unsafe { CStr::from_ptr(home.fileSystemRepresentation().as_ptr()) };
    Ok(Path::new(OsStr::from_bytes(bytes.to_bytes())).to_path_buf())
}

#[cfg(target_os = "macos")]
mod finder {
    use super::*;
    use objc2::rc::{Retained, autoreleasepool};
    use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventSendOptions, NSString, NSURL};
    use std::{
        ffi::{CStr, CString, OsStr},
        os::unix::ffi::OsStrExt,
        path::PathBuf,
        ptr::NonNull,
    };

    const CORE: u32 = u32::from_be_bytes(*b"core");
    const DELETE: u32 = u32::from_be_bytes(*b"delo");
    const DIRECT: u32 = u32::from_be_bytes(*b"----");

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AEDeterminePermissionToAutomateTarget(
            target: *const std::ffi::c_void,
            event_class: u32,
            event_id: u32,
            ask: u8,
        ) -> i32;
    }

    fn target() -> Retained<NSAppleEventDescriptor> {
        NSAppleEventDescriptor::descriptorWithBundleIdentifier(&NSString::from_str(
            "com.apple.finder",
        ))
    }

    pub(super) fn permission(ask: bool) -> Result<(), String> {
        autoreleasepool(|_| {
            let target = target();
            let raw: *const std::ffi::c_void = target.aeDesc().cast();
            let status =
                unsafe { AEDeterminePermissionToAutomateTarget(raw, CORE, DELETE, u8::from(ask)) };
            if status == 0 {
                Ok(())
            } else {
                Err("需要允许 Visualize SSD 控制 Finder，才能保留“放回原处”信息。请在系统设置 > 隐私与安全性 > 自动化中允许 Finder。文件尚未移动。".into())
            }
        })
    }

    fn event(id: u32, object: &NSAppleEventDescriptor) -> Retained<NSAppleEventDescriptor> {
        let target = target();
        let event = NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(CORE,id,Some(&target),-1,0);
        event.setParamDescriptor_forKeyword(object, DIRECT);
        event
    }
    #[derive(Debug)]
    pub(super) struct Failure {
        pub message: String,
        pub confirmed_rejection: bool,
    }
    fn failure(code: isize, reply: bool) -> Failure {
        Failure {
            message: error_message(code),
            confirmed_rejection: code != -1712
                && (reply
                    || matches!(
                        code,
                        -1743 | -1744 | -43 | -1728 | -5000 | -54 | -10004 | -1713 | -600
                    )),
        }
    }
    fn send(event: &NSAppleEventDescriptor) -> Result<Retained<NSAppleEventDescriptor>, Failure> {
        // Finder may neither switch applications nor present confirmation/admin
        // dialogs. Unsupported trash operations fail instead of deleting forever.
        let reply = event
            .sendEventWithOptions_timeout_error(
                NSAppleEventSendOptions::WaitForReply | NSAppleEventSendOptions::NeverInteract,
                30.0,
            )
            .map_err(|error| failure(error.code(), false))?;
        if let Some(error) = reply_error(&reply) {
            return Err(error);
        }
        Ok(reply)
    }
    fn error_message(code: isize) -> String {
        match code {
            -1743 | -1744 => {
                "macOS 拒绝控制 Finder。请在系统设置 > 隐私与安全性 > 自动化中允许 Finder。".into()
            }
            -1712 => "Finder 尚未确认操作结果，请稍后检查恢复列表，避免重复操作。".into(),
            -43 | -1728 => "Finder 找不到该文件，请重新扫描。".into(),
            -5000 | -54 | -10004 | -1713 => {
                "Finder 无法安全移动此文件，请检查权限或在 Finder 中处理；未尝试永久删除。".into()
            }
            _ => "Finder 未能确认移到废纸篓的结果，请检查恢复列表或重新扫描。".into(),
        }
    }
    pub(super) fn reply_error(reply: &NSAppleEventDescriptor) -> Option<Failure> {
        let error = reply.paramDescriptorForKeyword(u32::from_be_bytes(*b"errn"));
        error.and_then(|e| {
            let code = e.int32Value();
            (code != 0).then(|| failure(code as isize, true))
        })
    }

    fn reply_object(reply: &NSAppleEventDescriptor) -> Option<Retained<NSAppleEventDescriptor>> {
        reply.paramDescriptorForKeyword(DIRECT)
    }
    pub(super) fn single_object(
        object: Retained<NSAppleEventDescriptor>,
    ) -> Option<Retained<NSAppleEventDescriptor>> {
        let kind = object.descriptorType();
        if kind == u32::from_be_bytes(*b"list") {
            if object.numberOfItems() == 1 {
                object.descriptorAtIndex(1)
            } else {
                None
            }
        } else {
            Some(object)
        }
    }
    fn file_path(desc: &NSAppleEventDescriptor) -> Option<PathBuf> {
        let url = desc.fileURLValue()?;
        let raw = unsafe { CStr::from_ptr(url.fileSystemRepresentation().as_ptr()) };
        Some(Path::new(OsStr::from_bytes(raw.to_bytes())).to_path_buf())
    }
    fn file_object(path: &Path) -> Result<Retained<NSAppleEventDescriptor>, Failure> {
        let bytes = CString::new(path.as_os_str().as_bytes()).map_err(|_| Failure {
            message: "文件路径无效，请重新扫描。".into(),
            confirmed_rejection: true,
        })?;
        let url = unsafe {
            NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
                NonNull::new(bytes.as_ptr().cast_mut()).unwrap(),
                false,
                None,
            )
        };
        return Ok(NSAppleEventDescriptor::descriptorWithFileURL(&url));
    }
    #[cfg(test)]
    pub(super) fn file_codec_without_send(path: &Path) -> Option<PathBuf> {
        autoreleasepool(|_| {
            let object = file_object(path).ok()?;
            let event = event(DELETE, &object);
            let _ = event.aeDesc();
            let direct = event.paramDescriptorForKeyword(DIRECT)?;
            file_path(&direct)
        })
    }
    pub(super) fn trash(path: &Path) -> Result<Option<PathBuf>, Failure> {
        autoreleasepool(|_| {
            let object = file_object(path)?;
            let reply = send(&event(DELETE, &object))?;
            let Some(object) = reply_object(&reply).and_then(single_object) else {
                return Ok(None);
            };
            if let Some(path) = file_path(&object) {
                return Ok(Some(path));
            }
            // Finder returns an object specifier. Ask Finder for its file URL,
            // rather than parsing AppleScript text or interpolating filenames.
            let get = event(u32::from_be_bytes(*b"getd"), &object);
            let requested =
                NSAppleEventDescriptor::descriptorWithTypeCode(u32::from_be_bytes(*b"furl"));
            get.setParamDescriptor_forKeyword(&requested, u32::from_be_bytes(*b"rtyp"));
            Ok(send(&get)
                .ok()
                .and_then(|r| reply_object(&r))
                .and_then(single_object)
                .and_then(|d| file_path(&d)))
        })
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn destination_entry(path: std::path::PathBuf) -> Option<FileEntry> {
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || is_link_or_reparse(&metadata) {
        return None;
    }
    Some(FileEntry {
        can_trash: can_move_to_trash(&path)
            && fs::canonicalize(&path).is_ok_and(|p| can_move_to_trash(&p)),
        parent: path.parent()?.into(),
        size: metadata.len(),
        identity: FileIdentity::from_path(&path, &metadata).ok()?,
        path,
    })
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
                    Ok(()) => return Self(fs::canonicalize(path).unwrap()),
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("cannot create test fixture: {error}"),
                }
            }
        }
        fn file(&self, name: &str) -> (PathBuf, FileIdentity) {
            let path = self.0.join(name);
            fs::write(&path, b"temporary fixture").unwrap();
            let identity =
                FileIdentity::from_path(&path, &fs::symlink_metadata(&path).unwrap()).unwrap();
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
        let identity =
            FileIdentity::from_path(&temp.0, &fs::symlink_metadata(&temp.0).unwrap()).unwrap();
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
    fn destination_entry_preserves_file_identity_and_logical_bytes() {
        let temp = Temp::new();
        let (path, identity) = temp.file("source");
        let destination = temp.0.join("renamed");
        fs::rename(&path, &destination).unwrap();
        let entry = destination_entry(destination.clone()).unwrap();
        assert_eq!(entry.path, destination);
        assert_eq!(entry.parent.as_ref(), temp.0);
        assert_eq!(entry.size, b"temporary fixture".len() as u64);
        assert_eq!(entry.identity, identity);
        assert!(destination_entry(temp.0.join("missing")).is_none());
        assert!(destination_entry(temp.0.clone()).is_none());
    }

    #[test]
    fn system_files_cannot_be_trashed_but_data_volume_user_files_can() {
        for path in ["/System/Library/file", "/System/Volumes/Data/System/file"] {
            assert!(!can_move_to_trash(Path::new(path)), "{path}");
            assert!(
                move_to_trash(Path::new(path), FileIdentity::default())
                    .unwrap_err()
                    .contains("系统文件夹")
            );
        }
        for path in [
            "/Systematic/file",
            "/Users/me/file",
            "/System/Volumes/Data/Users/me/file",
        ] {
            assert!(can_move_to_trash(Path::new(path)), "{path}");
        }
        assert!(!can_move_to_trash(Path::new("/Users/me/.Trash/file")));
    }

    #[test]
    #[cfg(not(windows))]
    fn trash_ancestors_are_rejected_before_any_operation() {
        assert!(already_in_trash(Path::new("/Users/me/.Trash/file")));
        assert!(already_in_trash(Path::new(
            "/Volumes/disk/.Trashes/501/file"
        )));
        assert!(!already_in_trash(Path::new(
            "/Users/me/Trash-not-trash/file"
        )));
        assert!(
            move_to_trash(
                Path::new("/Users/me/.Trash/missing"),
                FileIdentity::default()
            )
            .unwrap_err()
            .contains("不会再次删除")
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn finder_production_codec_preserves_quoted_unicode_paths_without_dispatch() {
        let temp = Temp::new();
        let (path, _) = temp.file("quote\"-line\n中文");
        assert_eq!(finder::file_codec_without_send(&path).as_ref(), Some(&path));
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn finder_single_item_list_reply_is_unwrapped_without_sending_events() {
        use objc2_foundation::{NSAppleEventDescriptor, NSString};
        let list = NSAppleEventDescriptor::listDescriptor();
        let item = NSAppleEventDescriptor::descriptorWithString(&NSString::from_str("result"));
        list.insertDescriptor_atIndex(&item, 1);
        assert_eq!(
            finder::single_object(list)
                .unwrap()
                .stringValue()
                .unwrap()
                .to_string(),
            "result"
        );
        assert!(finder::single_object(NSAppleEventDescriptor::listDescriptor()).is_none());
    }
    #[test]
    #[cfg(unix)]
    fn application_recovery_files_are_protected_including_alias_paths() {
        let temp = Temp::new();
        let support = temp.0.join("Support");
        fs::create_dir(&support).unwrap();
        let file = support.join("journal.json");
        fs::write(&file, b"data").unwrap();
        assert!(protected_recovery_location(&file, &support));
        let alias = temp.0.join("alias");
        std::os::unix::fs::symlink(&support, &alias).unwrap();
        assert!(protected_recovery_location(
            &alias.join("journal.json"),
            &support
        ));
        assert!(!protected_recovery_location(
            &temp.0.join("other"),
            &support
        ));
    }
    #[test]
    #[cfg(target_os = "macos")]
    fn finder_application_error_is_not_mistaken_for_successful_transport() {
        use objc2_foundation::NSAppleEventDescriptor;
        let reply = NSAppleEventDescriptor::recordDescriptor();
        let code = NSAppleEventDescriptor::descriptorWithInt32(-1743);
        reply.setParamDescriptor_forKeyword(&code, u32::from_be_bytes(*b"errn"));
        assert!(
            finder::reply_error(&reply)
                .unwrap()
                .message
                .contains("自动化")
        );
        let ok = NSAppleEventDescriptor::recordDescriptor();
        assert!(finder::reply_error(&ok).is_none());
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires already-authorized Finder automation; never prompts or activates Finder"]
    fn finder_trash_and_recorded_restore_fixture_without_prompt() {
        if finder::permission(false).is_err() {
            eprintln!(
                "SKIPPED: Finder automation is not preauthorized; no prompt or file operation performed"
            );
            return;
        }
        let temp = Temp::new();
        let name = format!(
            "visualize-finder-trash-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let (path, identity) = temp.file(&name);
        // Use an isolated journal; never write application/user recovery records.
        let home = home_directory().unwrap();
        let store =
            recovery::Store::open_for_test(temp.0.join("records"), home.join(".Trash")).unwrap();
        let mut record = store.pending(&path, identity).unwrap().0;
        let result = finder::trash(&path).unwrap();
        let moved = result
            .and_then(destination_entry)
            .or_else(|| store.locate(identity))
            .expect("Finder should return or reveal the original object in Trash");
        record.trash_path = Some(moved.path.clone());
        store.save(&record).unwrap();
        struct Restore(std::path::PathBuf, std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::rename(&self.0, &self.1);
            }
        }
        let guard = Restore(moved.path.clone(), path.clone());
        assert!(!path.exists());
        let restored = store.restore_for_test(&record.id).unwrap().unwrap();
        assert_eq!(restored.path, path);
        assert_eq!(restored.identity, identity);
        assert!(!moved.path.exists());
        drop(guard);
    }
}
