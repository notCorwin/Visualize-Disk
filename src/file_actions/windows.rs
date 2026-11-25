use super::{
    FileEntry, FileIdentity, TargetKind, already_in_trash, destination_entry, verify_target,
};
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf, Prefix},
    sync::{Arc, Mutex},
};
use windows::{
    Win32::{
        Foundation::{E_ABORT, E_ACCESSDENIED, S_OK},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
        UI::Shell::{
            self, FileOperation, IFileOperation, IFileOperationProgressSink,
            IFileOperationProgressSink_Impl, IShellItem, SHCreateItemFromParsingName,
            SIGDN_FILESYSPATH, TSF_DELETE_RECYCLE_IF_POSSIBLE,
        },
    },
    core::{Error, HRESULT, PCWSTR, Ref, implement},
};

struct Apartment;
impl Apartment {
    fn new() -> Result<Self, String> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(|_| "无法连接 Windows 回收站，请关闭应用后重试。".to_owned())?;
        Ok(Self)
    }
}
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[derive(Default)]
struct Outcome {
    completed: bool,
    destination: Option<PathBuf>,
    failure: Option<HRESULT>,
}

#[implement(IFileOperationProgressSink)]
struct RecycleGuard {
    path: PathBuf,
    expected_identity: FileIdentity,
    kind: TargetKind,
    outcome: Arc<Mutex<Outcome>>,
}

impl IFileOperationProgressSink_Impl for RecycleGuard_Impl {
    fn PreDeleteItem(&self, flags: u32, _: Ref<IShellItem>) -> windows::core::Result<()> {
        // Reject a permanent-delete callback even if a volume or its recycle-bin
        // policy cannot honor the operation's RECYCLEONDELETE requirement.
        if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            return Err(Error::from_hresult(E_ABORT));
        }
        verify_target(&self.path, self.expected_identity, self.kind)
            .map_err(|_| Error::from_hresult(E_ABORT))
    }
    fn PostDeleteItem(
        &self,
        _: u32,
        _: Ref<IShellItem>,
        result: HRESULT,
        destination: Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        let mut outcome = self.outcome.lock().unwrap_or_else(|e| e.into_inner());
        if result != S_OK && result != Shell::COPYENGINE_S_DONT_PROCESS_CHILDREN {
            outcome.failure = Some(result);
            return Ok(());
        }
        outcome.completed = true;
        outcome.destination = destination.as_ref().and_then(shell_path);
        Ok(())
    }
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, result: HRESULT) -> windows::core::Result<()> {
        if result.is_err() {
            self.outcome
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .failure = Some(result);
        }
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: &windows::core::PCWSTR,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PostRenameItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: &windows::core::PCWSTR,
        _: windows::core::HRESULT,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PreMoveItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: windows::core::Ref<IShellItem>,
        _: &windows::core::PCWSTR,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PostMoveItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: windows::core::Ref<IShellItem>,
        _: &windows::core::PCWSTR,
        _: windows::core::HRESULT,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PreCopyItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: windows::core::Ref<IShellItem>,
        _: &windows::core::PCWSTR,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PostCopyItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: windows::core::Ref<IShellItem>,
        _: &windows::core::PCWSTR,
        _: windows::core::HRESULT,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PreNewItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: &windows::core::PCWSTR,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
    fn PostNewItem(
        &self,
        _: u32,
        _: windows::core::Ref<IShellItem>,
        _: &windows::core::PCWSTR,
        _: &windows::core::PCWSTR,
        _: u32,
        _: windows::core::HRESULT,
        _: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Err(Error::from_hresult(E_ABORT))
    }
}

fn shell_path(item: &IShellItem) -> Option<PathBuf> {
    unsafe {
        let value = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = PathBuf::from(OsString::from_wide(value.as_wide()));
        CoTaskMemFree(Some(value.0.cast()));
        Some(path)
    }
}

fn shell_parsing_path(path: &Path) -> Result<PathBuf, String> {
    let mut components = path.components();
    let prefix = match components.next() {
        Some(Component::Prefix(prefix)) => prefix,
        _ => return Err("文件路径不是 Windows 绝对路径，请重新扫描。".into()),
    };
    let mut output = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", drive as char)),
        Prefix::VerbatimUNC(host, share) => {
            let mut path = PathBuf::from(r"\\");
            path.push(host);
            path.push(share);
            path
        }
        Prefix::Disk(_) | Prefix::UNC(_, _) => return Ok(path.to_path_buf()),
        _ => return Err("此位置不支持系统回收站，请在资源管理器中检查。".into()),
    };
    for component in components {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => output.push(name),
            _ => return Err("文件路径无效，请重新扫描。".into()),
        }
    }
    Ok(output)
}

fn source_is_gone(path: &Path) -> bool {
    matches!(std::fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

fn operation_error(error: HRESULT) -> String {
    if error == E_ACCESSDENIED
        || error == Shell::COPYENGINE_E_ACCESS_DENIED_SRC
        || error == Shell::COPYENGINE_E_ACCESSDENIED_READONLY
    {
        "Windows 拒绝移动文件，请检查当前账户的删除权限，或文件是否被其他应用占用。".into()
    } else {
        "Windows 未完成回收站操作；此位置可能不支持回收站或文件被占用。请重新扫描后在资源管理器中检查；本应用不会永久删除文件。".into()
    }
}

pub(super) fn move_to_trash(
    path: &Path,
    expected: FileIdentity,
    kind: TargetKind,
) -> Result<Option<FileEntry>, String> {
    let _apartment = Apartment::new()?;
    let parsing_path = shell_parsing_path(path)?;
    // Converting verbatim paths to Shell paths must not select a different file
    // (for example a trailing-dot or trailing-space name).
    verify_target(&parsing_path, expected, kind)?;
    let encoded: Vec<u16> = parsing_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    if encoded[..encoded.len() - 1].contains(&0) {
        return Err("文件路径无效，请重新扫描。".into());
    }
    let outcome = Arc::new(Mutex::new(Outcome::default()));
    unsafe {
        let operation: IFileOperation =
            CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| operation_error(e.code()))?;
        // No elevation, dialogs, connected-file deletion, copy hooks, or permanent
        // deletion fallback. The sink rejects non-recycle callbacks as well.
        operation
            .SetOperationFlags(
                Shell::FOFX_RECYCLEONDELETE
                    | Shell::FOFX_ADDUNDORECORD
                    | Shell::FOFX_EARLYFAILURE
                    | Shell::FOFX_NOCOPYHOOKS
                    | Shell::FOF_SILENT
                    | Shell::FOF_NOCONFIRMATION
                    | Shell::FOF_NOERRORUI
                    | Shell::FOF_NO_CONNECTED_ELEMENTS
                    | Shell::FOF_NORECURSEREPARSE,
            )
            .map_err(|e| operation_error(e.code()))?;
        let item: IShellItem = SHCreateItemFromParsingName(PCWSTR(encoded.as_ptr()), None)
            .map_err(|e| operation_error(e.code()))?;
        let sink: IFileOperationProgressSink = RecycleGuard {
            path: path.to_path_buf(),
            expected_identity: expected,
            kind,
            outcome: outcome.clone(),
        }
        .into();
        operation
            .DeleteItem(&item, &sink)
            .map_err(|e| operation_error(e.code()))?;
        verify_target(path, expected, kind)?;
        let result = operation.PerformOperations();
        let aborted = operation
            .GetAnyOperationsAborted()
            .map_err(|e| operation_error(e.code()))?
            .as_bool();
        let state = outcome.lock().unwrap_or_else(|e| e.into_inner());
        // Preserve the actual destination when Windows reports a committed move,
        // even if a late notification causes PerformOperations to return failure.
        if let Some(file) = state
            .destination
            .clone()
            .and_then(|path| {
                if kind == TargetKind::File {
                    destination_entry(path)
                } else {
                    let metadata = std::fs::symlink_metadata(&path).ok()?;
                    if !metadata.is_dir() {
                        return None;
                    }
                    Some(FileEntry {
                        identity: FileIdentity::from_path(&path, &metadata).ok()?,
                        parent_identity: FileIdentity::default(),
                        parent_is_leaf: false,
                        parent: path.parent()?.into(),
                        can_trash: false,
                        size: 0,
                        path,
                    })
                }
            })
            .filter(|entry| entry.identity == expected && already_in_trash(&entry.path))
        {
            if source_is_gone(path) {
                return Ok(Some(file));
            }
        }
        if let Some(error) = state.failure {
            return Err(operation_error(error));
        }
        result.map_err(|e| operation_error(e.code()))?;
        if aborted || !state.completed {
            return Err(operation_error(E_ABORT));
        }
        // Microsoft documents a null destination as permanent deletion. Never
        // report success without verifying the recycled object's identity.
        Err("无法确认文件已进入回收站，请重新扫描并在系统回收站中检查；不要重复操作。".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recycle_guard_rejects_permanent_delete_before_touching_file() {
        let guard = RecycleGuard {
            path: PathBuf::from(r"C:\missing"),
            expected_identity: FileIdentity::default(),
            kind: TargetKind::File,
            outcome: Arc::new(Mutex::new(Outcome::default())),
        };
        let sink: IFileOperationProgressSink = guard.into();
        assert!(unsafe { sink.PreDeleteItem(0, None) }.is_err());
    }
}

#[cfg(test)]
mod native_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn recycle_paths_are_case_insensitive_and_protect_descendants() {
        for path in [
            r"C:\$Recycle.Bin\S-1-5\$Rfile",
            r"\\?\D:\$RECYCLE.BIN\file",
            r"C:\$recycle.bin",
        ] {
            assert!(already_in_trash(Path::new(path)), "{path}");
        }
        assert!(!already_in_trash(Path::new(
            r"C:\Downloads\$Recycle.Bin-backup\file"
        )));
        assert!(!already_in_trash(Path::new(r"C:\Downloads\.Trash\file")));
    }

    #[test]
    #[ignore = "recycles one dedicated temporary file without dialogs; run on the disposable Windows CI runner"]
    fn windows_recycle_smoke() {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "visualize-disk-recycle-smoke-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _fixture = Fixture(root.clone());
        let path = root.join("回收测试-🧪.txt");
        std::fs::write(&path, b"isolated recycle smoke fixture").unwrap();
        let metadata = std::fs::symlink_metadata(&path).unwrap();
        let identity = FileIdentity::from_path(&path, &metadata).unwrap();
        let moved = super::super::move_to_trash(&path, identity)
            .expect("Windows must confirm the fixture was recycled");
        assert!(source_is_gone(&path));
        if let Some(moved) = moved {
            assert!(already_in_trash(&moved.path));
            assert_eq!(moved.identity, identity);
            assert_eq!(
                std::fs::read(&moved.path).unwrap(),
                b"isolated recycle smoke fixture"
            );
        }
        // The runner is disposable. Never empty the user's recycle bin or edit
        // its private metadata to clean up this one recoverable fixture.
    }
}

#[cfg(test)]
mod parsing_tests {
    use super::*;

    #[test]
    fn canonical_shell_paths_preserve_drive_unc_and_unicode_names() {
        assert_eq!(
            shell_parsing_path(Path::new(r"\\?\C:\目录\a.txt")).unwrap(),
            Path::new(r"C:\目录\a.txt")
        );
        assert_eq!(
            shell_parsing_path(Path::new(r"\\?\UNC\server\share\a.txt")).unwrap(),
            Path::new(r"\\server\share\a.txt")
        );
        assert_eq!(
            shell_parsing_path(Path::new(r"\\?\C:\")).unwrap(),
            Path::new(r"C:\")
        );
        assert!(shell_parsing_path(Path::new(r"\\.\pipe\name")).is_err());
    }
}
