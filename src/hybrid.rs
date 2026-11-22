use crate::{file_actions, scan};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub(crate) struct HybridSnapshot {
    pub data: Arc<scan::ScanResult>,
    /// The source file index, or None when the tile represents a directory.
    pub source_ids: Arc<[Option<usize>]>,
}

pub(crate) fn build(
    data: &scan::ScanResult,
    hidden: &BTreeSet<usize>,
    token: &AtomicU64,
    generation: u64,
) -> Option<HybridSnapshot> {
    let mut groups: BTreeMap<&Path, (u64, scan::FileIdentity, bool)> = BTreeMap::new();
    let mut files = Vec::new();
    let mut source_ids = Vec::new();
    for (id, file) in data.files.iter().enumerate() {
        if id % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
            return None;
        }
        if hidden.contains(&id) {
            continue;
        }
        if file.parent_is_leaf {
            let group = groups
                .entry(&file.parent)
                .or_insert((0, file.parent_identity, true));
            group.0 = group.0.saturating_add(file.size);
            group.2 &= group.1 == file.parent_identity && file.can_trash;
        } else {
            files.push(file.clone());
            source_ids.push(Some(id));
        }
    }
    for (path, (size, identity, safe)) in groups {
        files.push(scan::FileEntry {
            path: path.to_path_buf(),
            parent: Arc::from(path.parent().unwrap_or(path)),
            identity,
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: false,
            size,
            can_trash: safe
                && path != data.root
                && file_actions::can_move_target(path, file_actions::TargetKind::Directory),
        });
        source_ids.push(None);
    }
    (token.load(Ordering::Relaxed) == generation).then(|| HybridSnapshot {
        data: Arc::new(scan::ScanResult {
            root: data.root.clone(),
            files,
            directories: Arc::clone(&data.directories),
            skipped: data.skipped,
            errors: Vec::new(),
            elapsed: data.elapsed,
        }),
        source_ids: source_ids.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, time::Duration};

    fn file(path: &str, size: u64, leaf: bool) -> scan::FileEntry {
        let path = PathBuf::from(path);
        scan::FileEntry {
            parent: Arc::from(path.parent().unwrap()),
            path,
            size,
            identity: scan::FileIdentity::default(),
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: leaf,
            can_trash: true,
        }
    }

    #[test]
    fn groups_leaf_files_and_preserves_other_file_indices() {
        let data = scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/branch/a", 4, false),
                file("/disk/branch/b", 5, false),
                file("/disk/branch/leaf/c", 6, true),
                file("/disk/branch/leaf/d", 7, true),
            ],
            directories: Vec::new().into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let token = AtomicU64::new(1);
        let result = build(&data, &BTreeSet::from([1]), &token, 1).unwrap();
        assert_eq!(result.source_ids.as_ref(), &[Some(0), None]);
        assert_eq!(result.data.files[0].path, Path::new("/disk/branch/a"));
        assert_eq!(result.data.files[1].path, Path::new("/disk/branch/leaf"));
        assert_eq!(result.data.files[1].size, 13);
        assert!(build(&data, &BTreeSet::new(), &token, 0).is_none());
    }
}
