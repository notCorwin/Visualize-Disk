//! Pure in-memory virtual dataset for testing layout and transition animations.
//! Never touches the filesystem, Finder, or the system Trash.
use crate::scan::{DirectoryFact, FileEntry, FileIdentity, ScanResult};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub const VIRTUAL_ROOT: &str = "/demo";
const VIRTUAL_DEVICE: u64 = 0xDE00_0001;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DemoAction {
    AddFile,
    ResizeRandom,
    DeleteRandom,
}

/// Generate a rich in-memory directory tree with leaf folders, nested
/// subdirectories, and varied logical file sizes for testing all view modes
/// and incremental transition animations without creating any real files.
pub fn virtual_scan() -> ScanResult {
    let root = PathBuf::from(VIRTUAL_ROOT);
    let mut next_inode = 1u64;
    let mut alloc_id = || {
        let id = FileIdentity {
            device: VIRTUAL_DEVICE,
            inode: next_inode.into(),
        };
        next_inode += 1;
        id
    };

    let root_id = alloc_id();
    let mut directories = vec![DirectoryFact {
        path: root.clone(),
        identity: root_id,
        enumeration_complete: true,
        has_subdirectories: true,
        excluded: false,
    }];
    let mut files = Vec::with_capacity(1024);

    for group in 0..32u64 {
        let folder_path = root.join(format!("示例文件夹-{group:02}"));
        let folder_id = alloc_id();
        let has_child = group % 4 == 0;
        let has_deep = group % 8 == 0;
        let has_subdirs = has_child || has_deep;

        directories.push(DirectoryFact {
            path: folder_path.clone(),
            identity: folder_id,
            enumeration_complete: true,
            has_subdirectories: has_subdirs,
            excluded: false,
        });

        let folder_arc: Arc<Path> = Arc::from(folder_path.as_path());
        for index in 0..24u64 {
            let seed = group * 31 + index;
            files.push(make_file(
                &folder_path,
                &folder_arc,
                folder_id,
                !has_subdirs,
                seed,
                alloc_id(),
            ));
        }

        if has_child {
            let child_path = folder_path.join("子文件夹");
            let child_id = alloc_id();
            directories.push(DirectoryFact {
                path: child_path.clone(),
                identity: child_id,
                enumeration_complete: true,
                has_subdirectories: false,
                excluded: false,
            });
            let child_arc: Arc<Path> = Arc::from(child_path.as_path());
            for index in 0..12u64 {
                let seed = group * 53 + index + 1000;
                files.push(make_file(
                    &child_path,
                    &child_arc,
                    child_id,
                    true,
                    seed,
                    alloc_id(),
                ));
            }
        }

        if has_deep {
            let branch_path = folder_path.join("深层目录");
            let branch_id = alloc_id();
            directories.push(DirectoryFact {
                path: branch_path.clone(),
                identity: branch_id,
                enumeration_complete: true,
                has_subdirectories: true,
                excluded: false,
            });
            let media_path = branch_path.join("媒体库");
            let media_id = alloc_id();
            directories.push(DirectoryFact {
                path: media_path.clone(),
                identity: media_id,
                enumeration_complete: true,
                has_subdirectories: false,
                excluded: false,
            });
            let media_arc: Arc<Path> = Arc::from(media_path.as_path());
            for index in 0..8u64 {
                let seed = group * 71 + index + 2000;
                files.push(make_file(
                    &media_path,
                    &media_arc,
                    media_id,
                    true,
                    seed,
                    alloc_id(),
                ));
            }
        }
    }

    ScanResult {
        root,
        files,
        directories: directories.into(),
        skipped: 0,
        errors: Vec::new(),
        elapsed: Duration::ZERO,
    }
}

fn make_file(
    parent_path: &Path,
    parent_arc: &Arc<Path>,
    parent_identity: FileIdentity,
    parent_is_leaf: bool,
    seed: u64,
    identity: FileIdentity,
) -> FileEntry {
    let size = (1 + (seed * 73 % 256)) * 256 * 1024;
    FileEntry {
        path: parent_path.join(format!("示例文件-{seed:04}.dat")),
        size,
        parent: Arc::clone(parent_arc),
        identity,
        parent_identity,
        parent_is_leaf,
        can_trash: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_scan_builds_in_memory_tree_with_mixed_depths() {
        let result = virtual_scan();
        assert_eq!(result.root, Path::new(VIRTUAL_ROOT));
        assert!(result.files.len() > 800);
        assert!(result.directories.len() > 40);
        assert!(result.files.iter().all(|f| f.can_trash && f.size > 0));
    }
}
