use super::{
    DirectoryReport, FileEntry, FileIdentity, PendingDirectory, ScanProgress, publish, record_error,
};
use libc::{
    ATTR_BIT_MAP_COUNT, ATTR_CMN_DEVID, ATTR_CMN_FILEID, ATTR_CMN_FLAGS, ATTR_CMN_NAME,
    ATTR_CMN_OBJTYPE, ATTR_CMN_RETURNED_ATTRS, ATTR_DIR_MOUNTSTATUS, ATTR_FILE_DATALENGTH,
    DIR_MNTSTATUS_MNTPOINT,
};
use std::cell::RefCell;
use std::ffi::{CStr, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// Darwin's <sys/attr.h>, <sys/stat.h>, and <sys/vnode.h>, respectively.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const SF_FIRMLINK: u32 = 0x0080_0000;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VBLK: u32 = 3;
const VFIFO: u32 = 7;
const COMMON_ATTRIBUTES: u32 = ATTR_CMN_RETURNED_ATTRS
    | ATTR_CMN_ERROR
    | ATTR_CMN_NAME
    | ATTR_CMN_DEVID
    | ATTR_CMN_OBJTYPE
    | ATTR_CMN_FLAGS
    | ATTR_CMN_FILEID;

// Darwin packs records at 8-byte boundaries. Reuse one bounded buffer per
// worker across directories, and release it before the portable path can run.
#[repr(align(8))]
struct BulkBuffer([u8; 32 * 1024]);

thread_local! {
    static BUFFER: RefCell<BulkBuffer> = const { RefCell::new(BulkBuffer([0; 32 * 1024])) };
}

pub(super) fn scan_directory(
    directory: &PendingDirectory,
    cancel: &AtomicBool,
    progress: &Mutex<ScanProgress>,
) -> Option<DirectoryReport> {
    if cancel.load(Ordering::Relaxed) {
        return Some(DirectoryReport::default());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(&directory.path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if FileIdentity::from_metadata(&metadata) != directory.identity {
        let error = io::ErrorKind::Interrupted.into();
        let mut report = DirectoryReport::default();
        let mut delta = ScanProgress::default();
        record_error(&mut delta, &mut report.errors, &directory.path, &error);
        publish(progress, delta);
        report.unreadable = Some(error);
        return Some(report);
    }
    BUFFER.with_borrow_mut(|buffer| scan_bulk(&file, &directory.path, cancel, progress, buffer))
}

fn scan_bulk(
    file: &File,
    directory: &Path,
    cancel: &AtomicBool,
    progress: &Mutex<ScanProgress>,
    buffer: &mut BulkBuffer,
) -> Option<DirectoryReport> {
    let mut attrs = libc::attrlist {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: COMMON_ATTRIBUTES,
        volattr: 0,
        dirattr: ATTR_DIR_MOUNTSTATUS,
        fileattr: ATTR_FILE_DATALENGTH,
        forkattr: 0,
    };
    let mut report = DirectoryReport::default();
    let mut published = false;
    while !cancel.load(Ordering::Relaxed) {
        // SAFETY: file is an open directory descriptor. Both pointers refer to
        // live, aligned writable storage for exactly the sizes passed.
        let count = unsafe {
            libc::getattrlistbulk(
                file.as_raw_fd(),
                (&mut attrs as *mut libc::attrlist).cast(),
                buffer.0.as_mut_ptr().cast(),
                buffer.0.len(),
                0,
            )
        };
        if count == 0 {
            break;
        }
        let mut delta = ScanProgress::default();
        if let Err(error) = read_batch(
            count,
            &buffer.0,
            file.as_fd(),
            directory,
            cancel,
            &mut report,
            &mut delta,
        ) {
            // Unsupported filesystems and directories without search permission
            // can still use read_dir. Never replay already published entries.
            if !published {
                return None;
            }
            record_error(&mut delta, &mut report.errors, directory, &error);
            publish(progress, delta);
            break;
        }
        publish(progress, delta);
        published = true;
    }
    Some(report)
}

fn read_batch(
    count: libc::c_int,
    mut bytes: &[u8],
    fd: BorrowedFd<'_>,
    directory: &Path,
    cancel: &AtomicBool,
    report: &mut DirectoryReport,
    delta: &mut ScanProgress,
) -> io::Result<()> {
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut parent = report.files.first().map(|file| Arc::clone(&file.parent));
    for _ in 0..count {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let (entry, rest) = read_entry(bytes).ok_or(io::ErrorKind::InvalidData)?;
        bytes = rest;
        let metadata = entry.metadata.map_or_else(|| entry.stat(fd), Ok);
        match metadata {
            Ok(EntryMetadata::File(identity, size)) => {
                delta.files += 1;
                delta.bytes = delta.bytes.saturating_add(size);
                report.files.push(FileEntry {
                    path: child_path(directory, entry.name),
                    parent: Arc::clone(parent.get_or_insert_with(|| Arc::from(directory))),
                    size,
                    identity,
                });
            }
            Ok(EntryMetadata::Directory(identity)) => {
                report.directories.push(PendingDirectory {
                    path: child_path(directory, entry.name),
                    identity,
                });
            }
            Ok(EntryMetadata::Other) => delta.skipped += 1,
            Err(error) => record_error(
                delta,
                &mut report.errors,
                &child_path(directory, entry.name),
                &error,
            ),
        }
    }
    Ok(())
}

fn child_path(directory: &Path, name: &CStr) -> PathBuf {
    let name = OsStr::from_bytes(name.to_bytes());
    let mut path = PathBuf::with_capacity(directory.as_os_str().len() + 1 + name.len());
    path.push(directory);
    path.push(name);
    path
}

#[derive(Clone, Copy)]
enum EntryMetadata {
    File(FileIdentity, u64),
    Directory(FileIdentity),
    Other,
}

struct Entry<'a> {
    name: &'a CStr,
    metadata: Option<EntryMetadata>,
}

impl Entry<'_> {
    fn stat(&self, fd: BorrowedFd<'_>) -> io::Result<EntryMetadata> {
        let mut stat = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: fd and the NUL-terminated name stay alive for the call. The
        // kernel initializes stat on success and never follows a symlink here.
        let status = unsafe {
            libc::fstatat(
                fd.as_raw_fd(),
                self.name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatat succeeded.
        let stat = unsafe { stat.assume_init() };
        let identity = FileIdentity {
            device: stat.st_dev as u64,
            inode: stat.st_ino,
        };
        Ok(match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG => EntryMetadata::File(identity, stat.st_size as u64),
            libc::S_IFDIR => EntryMetadata::Directory(identity),
            _ => EntryMetadata::Other,
        })
    }
}

fn read_entry(bytes: &[u8]) -> Option<(Entry<'_>, &[u8])> {
    let length = u32::from_ne_bytes(*bytes.first_chunk()?) as usize;
    if length % 8 != 0 || length < 32 {
        return None;
    }
    let (record, rest) = bytes.split_at_checked(length)?;
    // Records contain the length then an attribute_set_t with 5 u32 bitmaps.
    let mut fields = &record[4..];
    let common = u32::from_ne_bytes(read(&mut fields)?);
    let volume = u32::from_ne_bytes(read(&mut fields)?);
    let directory = u32::from_ne_bytes(read(&mut fields)?);
    let file = u32::from_ne_bytes(read(&mut fields)?);
    let fork = u32::from_ne_bytes(read(&mut fields)?);
    if common & ATTR_CMN_NAME == 0
        || common & !COMMON_ATTRIBUTES != 0
        || volume != 0
        || directory & !ATTR_DIR_MOUNTSTATUS != 0
        || file & !ATTR_FILE_DATALENGTH != 0
        || fork != 0
    {
        return None;
    }

    // The optional per-entry errno precedes the common attributes.
    let error = if common & ATTR_CMN_ERROR != 0 {
        u32::from_ne_bytes(read(&mut fields)?)
    } else {
        0
    };
    let reference = record.len() - fields.len();
    let offset = i32::from_ne_bytes(read(&mut fields)?);
    let name_start = reference.checked_add_signed(offset as isize)?;
    let name_end = name_start.checked_add(u32::from_ne_bytes(read(&mut fields)?) as usize)?;
    let metadata = if error == 0 {
        read_metadata(&mut fields, common, directory, file)?
    } else {
        None
    };
    if name_start < record.len() - fields.len() {
        return None;
    }
    let name = CStr::from_bytes_with_nul(record.get(name_start..name_end)?).ok()?;
    let name_bytes = name.to_bytes();
    // Only permit relative basenames when constructing paths or calling fstatat.
    if matches!(name_bytes, b"" | b"." | b"..") || name_bytes.contains(&b'/') {
        return None;
    }
    Some((Entry { name, metadata }, rest))
}

fn read_metadata(
    fields: &mut &[u8],
    common: u32,
    directory: u32,
    file: u32,
) -> Option<Option<EntryMetadata>> {
    let device = if common & ATTR_CMN_DEVID != 0 {
        Some(i32::from_ne_bytes(read(fields)?) as u64)
    } else {
        None
    };
    let kind = if common & ATTR_CMN_OBJTYPE != 0 {
        Some(u32::from_ne_bytes(read(fields)?))
    } else {
        None
    };
    let flags = if common & ATTR_CMN_FLAGS != 0 {
        Some(u32::from_ne_bytes(read(fields)?))
    } else {
        None
    };
    let inode = if common & ATTR_CMN_FILEID != 0 {
        Some(u64::from_ne_bytes(read(fields)?))
    } else {
        None
    };
    let mount = if directory & ATTR_DIR_MOUNTSTATUS != 0 {
        Some(u32::from_ne_bytes(read(fields)?))
    } else {
        None
    };
    let size = if file & ATTR_FILE_DATALENGTH != 0 {
        u64::try_from(i64::from_ne_bytes(read(fields)?)).ok()
    } else {
        None
    };

    let metadata = match (kind, device, inode) {
        // For a firmlink or mount, the bulk API describes the underlying
        // directory. fstatat resolves the identity of the traversed target.
        (Some(VDIR), Some(device), Some(inode))
            if inode != 0
                && flags.is_some_and(|flags| flags & SF_FIRMLINK == 0)
                && mount.is_some_and(|mount| mount & DIR_MNTSTATUS_MNTPOINT == 0) =>
        {
            Some(EntryMetadata::Directory(FileIdentity { device, inode }))
        }
        (Some(VREG), Some(device), Some(inode)) if inode != 0 => {
            size.map(|size| EntryMetadata::File(FileIdentity { device, inode }, size))
        }
        (Some(VBLK..=VFIFO), _, _) => Some(EntryMetadata::Other),
        _ => None,
    };
    Some(metadata)
}

fn read<const N: usize>(fields: &mut &[u8]) -> Option<[u8; N]> {
    let (value, rest) = fields.split_first_chunk()?;
    *fields = rest;
    Some(*value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_entry_uses_stat_and_malformed_names_are_rejected() {
        // ATTR_CMN_NAME is the only field after the returned attributes.
        let mut record = [0; 40];
        record[..4].copy_from_slice(&40u32.to_ne_bytes());
        record[4..8].copy_from_slice(&(ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_NAME).to_ne_bytes());
        record[24..28].copy_from_slice(&8i32.to_ne_bytes());
        record[28..32].copy_from_slice(&5u32.to_ne_bytes());
        record[32..37].copy_from_slice(b"name\0");
        let (entry, rest) = read_entry(&record).unwrap();
        assert!(rest.is_empty());
        assert_eq!(entry.name.to_bytes(), b"name");
        assert!(entry.metadata.is_none());

        assert!(read_entry(&record[..39]).is_none());
        record[32..37].copy_from_slice(b"a/bc\0");
        assert!(read_entry(&record).is_none());
        record[24..28].copy_from_slice(&(-4i32).to_ne_bytes());
        assert!(read_entry(&record).is_none());
    }
}
