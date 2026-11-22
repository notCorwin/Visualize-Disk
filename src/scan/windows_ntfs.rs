use super::{
    EntryReport, FileEntry, FileIdentity, PendingDirectory, is_link_or_reparse,
    windows_handle_identity,
};
use std::{
    ffi::c_void,
    fs::{DirEntry, File, OpenOptions},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Component, Path, PathBuf, Prefix},
    sync::{Arc, OnceLock},
};
use windows::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{FILE_INFORMATION_CLASS, FILE_STAT_INFORMATION, FileStatInformation},
    },
    Win32::{
        Foundation::{HANDLE, NTSTATUS, OBJ_DONT_REPARSE, STATUS_SUCCESS, UNICODE_STRING},
        Storage::FileSystem::{
            FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
            FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            GetDriveTypeW, GetVolumeInformationByHandleW,
        },
        System::{
            IO::IO_STATUS_BLOCK,
            LibraryLoader::{GetModuleHandleW, GetProcAddress},
            WindowsProgramming::DRIVE_FIXED,
        },
    },
    core::{PCWSTR, PWSTR, s, w},
};

type QueryByName = unsafe extern "system" fn(
    *const OBJECT_ATTRIBUTES,
    *mut IO_STATUS_BLOCK,
    *mut c_void,
    u32,
    FILE_INFORMATION_CLASS,
) -> NTSTATUS;

fn get_query_by_name() -> Option<QueryByName> {
    static QUERY: OnceLock<Option<QueryByName>> = OnceLock::new();
    // Older Windows versions continue through the existing handle-based query.
    *QUERY.get_or_init(|| unsafe {
        let ntdll = GetModuleHandleW(w!("ntdll.dll")).ok()?;
        let query = GetProcAddress(ntdll, s!("NtQueryInformationByName"))?;
        // Windows returns this known export as an untyped function address.
        Some(std::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            QueryByName,
        >(query))
    })
}

pub(super) struct DirectoryQuery {
    query: QueryByName,
    anchor: File,
    device: u64,
    verbatim: bool,
}

impl DirectoryQuery {
    /// The caller should already have an iterator from `fs::read_dir`.
    /// A metadata handle opened with backup semantics must not grant listing.
    pub(super) fn open(directory: &PendingDirectory) -> Option<Self> {
        let query = get_query_by_name()?;
        let (drive, verbatim) = match directory.path.components().next() {
            Some(Component::Prefix(prefix)) if directory.path.is_absolute() => {
                match prefix.kind() {
                    Prefix::Disk(drive) => (drive, false),
                    Prefix::VerbatimDisk(drive) => (drive, true),
                    _ => return None,
                }
            }
            _ => return None,
        };
        if !verbatim {
            // Opening the terminal component and listing it with a trailing
            // wildcard can apply different DOS-name normalization. Check all
            // components in case the wildcard also crosses the long-path limit.
            let wide: Vec<u16> = directory.path.as_os_str().encode_wide().collect();
            if wide
                .get(2..)?
                .split(|&unit| unit == b'/' as u16 || unit == b'\\' as u16)
                .any(|component| requires_win32_normalization(component))
            {
                return None;
            }
        }
        let root = [u16::from(drive), b':' as u16, b'\\' as u16, 0];
        // The path is a complete NUL-terminated drive root.
        if unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } != DRIVE_FIXED {
            return None;
        }
        let file = OpenOptions::new()
            .access_mode((FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES).0)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .custom_flags((FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS).0)
            .open(&directory.path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_dir()
            || is_link_or_reparse(&metadata)
            || windows_handle_identity(&file).ok()? != directory.identity
        {
            return None;
        }
        let mut filesystem = [0u16; 16];
        // Confirm NTFS from the actual directory handle before combining its
        // volume identity with 64-bit IDs returned by relative queries.
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
        .ok()?;
        if !filesystem.starts_with(&[b'N' as u16, b'T' as u16, b'F' as u16, b'S' as u16, 0]) {
            return None;
        }
        Some(Self {
            query,
            anchor: file,
            device: directory.identity.device,
            verbatim,
        })
    }

    pub(super) fn entry(&self, entry: &DirEntry, parent: &Arc<Path>) -> Option<EntryReport> {
        let name = entry.file_name();
        let mut wide: Vec<u16> = name.encode_wide().collect();
        if !self.verbatim && requires_win32_normalization(&wide) {
            return None;
        }
        let info = query_entry(self.query, &self.anchor, &mut wide)?;
        Some(build_entry(parent.join(name), parent, self.device, info))
    }
}

// A relative NT query uses literal names. Let Win32 handle names whose DOS
// form can be trimmed or resolved as a device, including names with extensions.
pub(super) fn requires_win32_normalization(name: &[u16]) -> bool {
    const DOT: u16 = b'.' as u16;
    const SPACE: u16 = b' ' as u16;
    if matches!(name.last().copied(), Some(DOT | SPACE)) {
        return true;
    }
    let stem = name.split(|&unit| unit == DOT).next().unwrap_or_default();
    let start = stem
        .iter()
        .position(|&unit| unit != SPACE)
        .unwrap_or(stem.len());
    let stem = &stem[start..];
    let end = stem
        .iter()
        .rposition(|&unit| unit != SPACE)
        .map_or(0, |index| index + 1);
    let stem = &stem[..end];
    let eq_ascii = |wide: &[u16], ascii: &[u8]| {
        wide.len() == ascii.len()
            && wide.iter().zip(ascii).all(|(&unit, &expected)| {
                unit == u16::from(expected) || unit == u16::from(expected.to_ascii_lowercase())
            })
    };
    if let [prefix @ .., digit] = stem
        && (eq_ascii(prefix, b"COM") || eq_ascii(prefix, b"LPT"))
        && ((u16::from(b'0')..=u16::from(b'9')).contains(digit)
            || ['¹' as u16, '²' as u16, '³' as u16].contains(digit))
    {
        return true;
    }
    [
        b"CON".as_slice(),
        b"PRN",
        b"AUX",
        b"NUL",
        b"CONIN$",
        b"CONOUT$",
        b"CLOCK$",
    ]
    .into_iter()
    .any(|device| eq_ascii(stem, device))
}

fn query_entry(
    query: QueryByName,
    parent: &File,
    name: &mut [u16],
) -> Option<FILE_STAT_INFORMATION> {
    const DOT: u16 = b'.' as u16;
    const SLASH: u16 = b'/' as u16;
    const COLON: u16 = b':' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    if matches!(name, [] | [DOT] | [DOT, DOT])
        || name
            .iter()
            .any(|unit| matches!(*unit, 0 | SLASH | COLON | BACKSLASH))
    {
        return None;
    }
    let len = u16::try_from(std::mem::size_of_val(name)).ok()?;
    let name = UNICODE_STRING {
        Buffer: PWSTR(name.as_mut_ptr()),
        Length: len,
        MaximumLength: len,
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: HANDLE(parent.as_raw_handle()),
        ObjectName: &name,
        // Use the exact spelling from the directory record, including in a
        // case-sensitive directory.
        Attributes: OBJ_DONT_REPARSE,
        ..Default::default()
    };
    let mut info = FILE_STAT_INFORMATION::default();
    let mut io = IO_STATUS_BLOCK::default();
    // Both native outputs and the UTF-16 basename live through this synchronous
    // call. It cannot resolve a reparse point out of the directory's volume.
    let status = unsafe {
        query(
            &attributes,
            &mut io,
            (&mut info as *mut FILE_STAT_INFORMATION).cast(),
            std::mem::size_of::<FILE_STAT_INFORMATION>() as u32,
            FileStatInformation,
        )
    };
    // Verify access, identity, and type before admitting a by-name result.
    // The fallback retains the existing handle-based checks and diagnostics.
    if status != STATUS_SUCCESS
        || info.EffectiveAccess & FILE_READ_ATTRIBUTES.0 == 0
        || info.FileId == 0
        || info.FileId == -1
        || info.EndOfFile < 0
        || info.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DEVICE).0 != 0
        || info.ReparseTag != 0
    {
        return None;
    }
    Some(info)
}

fn build_entry(
    path: PathBuf,
    parent: &Arc<Path>,
    device: u64,
    info: FILE_STAT_INFORMATION,
) -> EntryReport {
    let identity = FileIdentity {
        device,
        // Microsoft [MS-FSA] 2.1.5.1.1: local NTFS zero-extends FileId64.
        inode: u128::from(info.FileId as u64),
    };
    if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        EntryReport::Directory(PendingDirectory { path, identity })
    } else {
        EntryReport::File(FileEntry {
            parent_identity: FileIdentity::default(),
            parent_is_leaf: false,
            can_trash: true,
            path,
            size: info.EndOfFile as u64,
            parent: Arc::clone(parent),
            identity,
        })
    }
}

#[cfg(test)]
pub(super) fn query_fixture_file(path: &Path) -> Option<FileEntry> {
    let parent = path.parent().expect("fixture file has a directory");
    let directory = PendingDirectory {
        path: parent.to_path_buf(),
        identity: super::directory_identity(parent).expect("resolve fixture directory identity"),
    };
    let entries = std::fs::read_dir(parent).expect("list fixture directory");
    let query = DirectoryQuery::open(&directory)?;
    let name = path.file_name().expect("fixture file has a name");
    let entry = entries
        .map(|entry| entry.expect("read fixture directory entry"))
        .find(|entry| entry.file_name() == name)
        .expect("find fixture file");
    let parent: Arc<Path> = parent.into();
    let EntryReport::File(file) = query
        .entry(&entry, &parent)
        .expect("query ordinary file on verified NTFS")
    else {
        panic!("native metadata must classify the fixture as a file");
    };
    Some(file)
}
