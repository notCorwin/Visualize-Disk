use std::path::{Path, PathBuf};

pub const TRASH_NAME: &str = if cfg!(windows) {
    "回收站"
} else {
    "废纸篓"
};
pub const REVEAL_LABEL: &str = if cfg!(windows) {
    "在资源管理器中显示"
} else {
    "在 Finder 中显示"
};

pub fn font_paths() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        windows_directory()
            .map(|directory| {
                ["msyh.ttc", "msyh.ttf", "simsun.ttc"]
                    .into_iter()
                    .map(|name| directory.join("Fonts").join(name))
                    .collect()
            })
            .unwrap_or_default()
    }
    #[cfg(not(windows))]
    {
        [
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
}

pub fn scan_root() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        let directory = windows_directory()?;
        directory
            .ancestors()
            .find(|ancestor| ancestor.has_root() && ancestor.parent().is_none())
            .map(Path::to_path_buf)
            .ok_or_else(|| "无法确定 Windows 系统盘，请确认系统目录可用后重试。".into())
    }
    #[cfg(not(windows))]
    {
        Ok(PathBuf::from("/"))
    }
}

#[cfg(windows)]
pub(crate) fn windows_directory() -> Result<PathBuf, String> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetWindowsDirectoryW(Some(&mut buffer)) } as usize;
    if length == 0 || length >= buffer.len() {
        return Err("Windows 未能提供系统目录，请确认系统状态后重试。".into());
    }
    Ok(PathBuf::from(OsString::from_wide(&buffer[..length])))
}

pub fn reveal(path: &Path) -> Result<(), String> {
    match path.try_exists() {
        Ok(false) => return Err("文件已移动或删除，请重新扫描。".into()),
        Err(_) => return Err("无法读取文件，请检查文件访问权限。".into()),
        Ok(true) => {}
    }
    native_reveal(path)
}

#[cfg(target_os = "macos")]
fn native_reveal(path: &Path) -> Result<(), String> {
    match std::process::Command::new("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .output()
    {
        Ok(output) if output.status.success() => Ok(()),
        _ => Err("无法在 Finder 中显示文件，请确认 Finder 可用后重试。".into()),
    }
}

#[cfg(windows)]
fn native_reveal(path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::{
            System::Com::{
                COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
            },
            UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName},
        },
        core::PCWSTR,
    };
    // This runs on its own worker; initialize and release its Shell apartment.
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
        .ok()
        .map_err(|_| "Windows 无法初始化资源管理器，请重试。")?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut item = std::ptr::null_mut();
    let result = unsafe {
        SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut item, 0, None)
            .and_then(|()| SHOpenFolderAndSelectItems(item, None, 0))
    };
    unsafe {
        CoTaskMemFree(Some(item.cast()));
        CoUninitialize();
    }
    result.map_err(|_| "Windows 无法在资源管理器中显示文件，请确认文件仍存在后重试。".into())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn native_reveal(_path: &Path) -> Result<(), String> {
    Err("当前系统不支持在文件管理器中定位文件。".into())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn system_root_and_chinese_font_are_resolved_from_windows() {
        let root = scan_root().unwrap();
        assert!(root.is_absolute());
        assert!(root.parent().is_none());
        assert!(windows_directory().unwrap().starts_with(&root));
        assert!(font_paths().iter().any(|path| path.is_file()));
        assert_eq!(
            crate::permission::check(),
            crate::permission::Access::Granted
        );
    }
}
