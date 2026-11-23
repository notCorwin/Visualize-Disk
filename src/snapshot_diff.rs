//! Reconcile a newly scanned snapshot with the actor's current scene.
//!
//! A missing entry is not proof of deletion when its parent could not be
//! enumerated. In that case we keep the prior entry until a complete scan can
//! establish its absence.
use crate::{
    scan,
    scene_model::{Change, Scene},
};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Cancelled,
    RootChanged,
    InvalidSnapshot,
    /// A replacement cannot be proved because its parent was not enumerated.
    UncertainReplacement,
}

pub fn plan(
    scene: &Scene,
    data: &scan::ScanResult,
    cancel: &AtomicBool,
) -> Result<Vec<Change>, Error> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    if scene.root() != data.root {
        return Err(Error::RootChanged);
    }
    let mut dirs = HashMap::with_capacity(data.directories.len());
    for fact in data.directories.iter() {
        if !fact.path.starts_with(&data.root) || dirs.insert(fact.path.as_path(), fact).is_some() {
            return Err(Error::InvalidSnapshot);
        }
    }
    for fact in data.directories.iter() {
        if (fact.excluded && fact.enumeration_complete)
            || fact
                .path
                .ancestors()
                .skip(1)
                .any(|p| dirs.get(p).is_some_and(|d| d.excluded))
        {
            return Err(Error::InvalidSnapshot);
        }
    }
    let root = dirs
        .get(data.root.as_path())
        .ok_or(Error::InvalidSnapshot)?;
    let root_id = scene.directory_id(&data.root).ok_or(Error::RootChanged)?;
    if scene.directory_identity(root_id) != Some(root.identity) {
        return Err(Error::RootChanged);
    }

    let mut current_dirs = HashMap::new();
    scene.for_each_directory(|fact| {
        current_dirs.insert(fact.path.clone(), fact.clone());
    });
    if !current_dirs.contains_key(&data.root) {
        return Err(Error::RootChanged);
    }
    // A positive exclusion is authoritative even when its parent was only
    // partly enumerated. An unavailable directory, by contrast, says nothing
    // about its old contents and must not be treated as an exclusion.
    let excluded_root = root.excluded;
    // Find only the topmost removed or replaced directories. A complete,
    // identity-matching parent proves that the old directory entry no longer
    // names the same object. Incomplete parents do not prove absence.
    let mut candidates: Vec<_> = current_dirs
        .values()
        .filter(|old| {
            old.path != data.root
                && (excluded_root && old.path.parent() == Some(data.root.as_path())
                    || dirs.get(old.path.as_path()).is_none_or(|new| {
                        new.identity != old.identity || (new.excluded && !old.excluded)
                    }))
        })
        .collect();
    candidates.sort_unstable_by_key(|fact| fact.path.components().count());
    let mut removed: HashSet<&Path> = HashSet::new();
    let mut changes = Vec::new();
    let mut parent_refresh = HashSet::new();
    for (index, old) in candidates.into_iter().enumerate() {
        if index % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if old.path.ancestors().any(|p| removed.contains(p)) {
            continue;
        }
        let Some(parent) = old.path.parent().and_then(|p| dirs.get(p)) else {
            continue;
        };
        let positively_excluded =
            excluded_root || dirs.get(old.path.as_path()).is_some_and(|new| new.excluded);
        if current_dirs
            .get(&parent.path)
            .is_none_or(|p| p.identity != parent.identity)
            || (!parent.enumeration_complete && !positively_excluded)
        {
            if dirs.contains_key(old.path.as_path()) {
                return Err(Error::UncertainReplacement);
            }
            continue;
        }
        let mut intermediate = (*parent).clone();
        // The verified child exclusion proves removal from the *view*, not
        // completeness of the parent. Restore the real parent fact below.
        intermediate.enumeration_complete = true;
        // Other siblings from the old scene may be removed later in this
        // transaction. Never claim this parent is already a leaf mid-plan.
        intermediate.has_subdirectories = true;
        changes.push(Change::RemoveSubtreeDirectory {
            path: old.path.clone(),
            identity: old.identity,
            parent: intermediate,
        });
        parent_refresh.insert(parent.path.as_path());
        removed.insert(old.path.as_path());
    }
    let mut files: HashSet<&Path> = HashSet::with_capacity(data.files.len());
    for (index, file) in data.files.iter().enumerate() {
        if index % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if !file.path.starts_with(&data.root)
            || file.path.parent() != Some(file.parent.as_ref())
            || !files.insert(&file.path)
            || dirs
                .get(file.parent.as_ref())
                .is_none_or(|parent| parent.identity != file.parent_identity || parent.excluded)
        {
            return Err(Error::InvalidSnapshot);
        }
    }
    let mut seen = 0usize;
    let mut cancelled = false;
    scene.for_each_file(|file| {
        seen += 1;
        if seen % 4096 == 0 && cancel.load(Ordering::Relaxed) {
            cancelled = true;
        }
        if cancelled || file.path.ancestors().any(|p| removed.contains(p)) {
            return;
        }
        let now_directory = dirs.contains_key(file.path.as_path());
        if excluded_root
            || (now_directory || !files.contains(file.path.as_path()))
                && dirs.get(file.parent.as_ref()).is_some_and(|parent| {
                    parent.enumeration_complete && parent.identity == file.parent_identity
                })
        {
            changes.push(Change::RemoveFile {
                path: file.path.clone(),
                identity: file.identity,
            });
        } else if !files.contains(file.path.as_path())
            && file.path.ancestors().any(|p| {
                dirs.get(p)
                    .is_some_and(|fact| !fact.enumeration_complete && !fact.excluded)
            })
            && file.can_trash
        {
            // Retain stale, unobserved entries, but do not offer deletion
            // until a subsequent scan confirms them again.
            let mut stale = file.clone();
            stale.can_trash = false;
            changes.push(Change::UpsertFile(stale));
        }
    });
    if cancelled || cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    // Directory entries can replace files, and directory parents must exist
    // before their children. The actor applies this ordered list to a private
    // scene and publishes only after the entire list succeeds.
    let mut ordered_dirs: Vec<_> = data.directories.iter().collect();
    ordered_dirs.sort_unstable_by_key(|fact| fact.path.components().count());
    for fact in ordered_dirs {
        let replaced = fact.path.ancestors().any(|p| removed.contains(p));
        let changed = current_dirs.get(&fact.path).is_none_or(|old| {
            old.identity != fact.identity
                || old.enumeration_complete != fact.enumeration_complete
                || old.has_subdirectories != fact.has_subdirectories
                || old.excluded != fact.excluded
        });
        if replaced || changed || parent_refresh.contains(fact.path.as_path()) {
            changes.push(Change::UpsertDirectory(fact.clone()));
        }
    }
    for file in &data.files {
        let replaced = file.path.ancestors().any(|p| removed.contains(p));
        let unchanged = scene
            .file_id(&file.path)
            .and_then(|id| scene.file(id))
            .is_some_and(|old| {
                old.identity == file.identity
                    && old.parent_identity == file.parent_identity
                    && old.size == file.size
                    && old.parent_is_leaf == file.parent_is_leaf
                    && old.can_trash == file.can_trash
            });
        if replaced || !unchanged {
            changes.push(Change::UpsertFile(file.clone()));
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, sync::Arc, time::Duration};

    fn id(n: u64) -> scan::FileIdentity {
        scan::FileIdentity {
            device: 1,
            inode: n as _,
        }
    }
    fn fact(path: &str, n: u64, complete: bool, has_children: bool) -> scan::DirectoryFact {
        scan::DirectoryFact {
            path: path.into(),
            identity: id(n),
            enumeration_complete: complete,
            has_subdirectories: has_children,
            excluded: false,
        }
    }
    fn file(path: &str, n: u64, parent: u64, size: u64) -> scan::FileEntry {
        let path = PathBuf::from(path);
        scan::FileEntry {
            parent: Arc::from(path.parent().unwrap()),
            path,
            size,
            identity: id(n),
            parent_identity: id(parent),
            parent_is_leaf: true,
            can_trash: true,
        }
    }
    fn snapshot(files: Vec<scan::FileEntry>, dirs: Vec<scan::DirectoryFact>) -> scan::ScanResult {
        scan::ScanResult {
            root: "/r".into(),
            files,
            directories: dirs.into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        }
    }
    fn changes(scene: &Scene, data: &scan::ScanResult) -> Result<Vec<Change>, Error> {
        plan(scene, data, &AtomicBool::new(false))
    }

    #[test]
    fn complete_parent_detects_changes_and_preserves_stable_id() {
        let original = snapshot(
            vec![file("/r/a", 2, 1, 4), file("/r/b", 3, 1, 5)],
            vec![fact("/r", 1, true, false)],
        );
        let mut scene = Scene::from_scan(Arc::new(original));
        let a = scene.file_id(Path::new("/r/a")).unwrap();
        let updated = snapshot(
            vec![file("/r/a", 2, 1, 8), file("/r/c", 4, 1, 3)],
            vec![fact("/r", 1, true, false)],
        );
        scene
            .apply_batch(changes(&scene, &updated).unwrap())
            .unwrap();
        assert_eq!(scene.file_id(Path::new("/r/a")), Some(a));
        assert_eq!(scene.file(a).unwrap().size, 8);
        assert!(scene.file_id(Path::new("/r/b")).is_none());
        assert!(scene.file_id(Path::new("/r/c")).is_some());
    }

    #[test]
    fn incomplete_parent_does_not_delete_unobserved_files() {
        let original = snapshot(
            vec![file("/r/a", 2, 1, 4)],
            vec![fact("/r", 1, true, false)],
        );
        let mut scene = Scene::from_scan(Arc::new(original));
        let updated = snapshot(vec![], vec![fact("/r", 1, false, false)]);
        scene
            .apply_batch(changes(&scene, &updated).unwrap())
            .unwrap();
        assert!(scene.file_id(Path::new("/r/a")).is_some());
        assert!(
            !scene
                .file(scene.file_id(Path::new("/r/a")).unwrap())
                .unwrap()
                .can_trash
        );
        assert_eq!(scene.directory_leaf(Path::new("/r")), Some(false));
        let recovered = snapshot(
            vec![file("/r/a", 2, 1, 4)],
            vec![fact("/r", 1, true, false)],
        );
        scene
            .apply_batch(changes(&scene, &recovered).unwrap())
            .unwrap();
        assert!(
            scene
                .file(scene.file_id(Path::new("/r/a")).unwrap())
                .unwrap()
                .can_trash
        );
    }

    #[test]
    fn confirmed_exclusion_removes_old_subtree_even_when_parent_is_incomplete() {
        let original = snapshot(
            vec![file("/r/a/deep/file", 4, 3, 1), file("/r/keep", 5, 1, 2)],
            vec![
                fact("/r", 1, true, true),
                fact("/r/a", 2, true, true),
                fact("/r/a/deep", 3, true, false),
            ],
        );
        let mut scene = Scene::from_scan(Arc::new(original));
        let mut excluded = fact("/r/a", 2, false, false);
        excluded.excluded = true;
        let updated = snapshot(vec![], vec![fact("/r", 1, false, true), excluded]);
        for change in changes(&scene, &updated).unwrap() {
            scene.apply(change).unwrap();
        }
        assert!(scene.file_id(Path::new("/r/a/deep/file")).is_none());
        assert!(scene.directory_id(Path::new("/r/a/deep")).is_none());
        let keep = scene.file_id(Path::new("/r/keep")).unwrap();
        assert!(!scene.file(keep).unwrap().can_trash);
    }

    #[test]
    fn excluded_root_clears_previous_view_and_recovers() {
        let original = snapshot(
            vec![file("/r/a", 2, 1, 1), file("/r/sub/b", 4, 3, 2)],
            vec![fact("/r", 1, true, true), fact("/r/sub", 3, true, false)],
        );
        let mut scene = Scene::from_scan(Arc::new(original));
        let mut root = fact("/r", 1, false, false);
        root.excluded = true;
        let excluded = snapshot(vec![], vec![root]);
        for change in changes(&scene, &excluded).unwrap() {
            scene.apply(change).unwrap();
        }
        assert!(scene.file_id(Path::new("/r/a")).is_none());
        assert!(scene.file_id(Path::new("/r/sub/b")).is_none());
        assert!(scene.directory_id(Path::new("/r/sub")).is_none());
        let recovered = snapshot(
            vec![file("/r/a", 2, 1, 1)],
            vec![fact("/r", 1, true, false)],
        );
        for change in changes(&scene, &recovered).unwrap() {
            scene.apply(change).unwrap();
        }
        assert!(scene.file_id(Path::new("/r/a")).is_some());
    }

    #[test]
    fn mixed_subtree_removal_and_directory_replacement_produce_ordered_changes() {
        let original = snapshot(
            vec![file("/r/sub/a", 3, 2, 4)],
            vec![fact("/r", 1, true, true), fact("/r/sub", 2, true, false)],
        );
        let mut scene = Scene::from_scan(Arc::new(original));
        let removed = snapshot(vec![], vec![fact("/r", 1, true, false)]);
        for change in changes(&scene, &removed).unwrap() {
            scene.apply(change).unwrap();
        }
        assert!(scene.file_id(Path::new("/r/sub/a")).is_none());
        assert!(scene.directory_id(Path::new("/r/sub")).is_none());
        let original = snapshot(
            vec![file("/r/sub/a", 3, 2, 4)],
            vec![fact("/r", 1, true, true), fact("/r/sub", 2, true, false)],
        );
        let mut scene = Scene::from_scan(Arc::new(original));
        let replaced = snapshot(
            vec![file("/r/sub/b", 5, 4, 8)],
            vec![fact("/r", 1, true, true), fact("/r/sub", 4, true, false)],
        );
        for change in changes(&scene, &replaced).unwrap() {
            scene.apply(change).unwrap();
        }
        assert!(scene.file_id(Path::new("/r/sub/a")).is_none());
        assert!(scene.file_id(Path::new("/r/sub/b")).is_some());
        assert_eq!(
            scene.directory_identity(scene.directory_id(Path::new("/r/sub")).unwrap()),
            Some(id(4))
        );
    }

    #[test]
    fn root_replacement_and_cancellation_do_not_change_scene() {
        let original = snapshot(vec![], vec![fact("/r", 1, true, false)]);
        let scene = Scene::from_scan(Arc::new(original));
        let changed = snapshot(vec![], vec![fact("/r", 2, true, false)]);
        assert_eq!(changes(&scene, &changed).unwrap_err(), Error::RootChanged);
        assert_eq!(
            plan(&scene, &changed, &AtomicBool::new(true)).unwrap_err(),
            Error::Cancelled
        );
    }

    #[test]
    fn multiple_removed_siblings_and_file_directory_exchange() {
        let original = snapshot(
            vec![
                file("/r/a/old", 4, 2, 1),
                file("/r/b/old", 5, 3, 1),
                file("/r/c", 6, 1, 1),
            ],
            vec![
                fact("/r", 1, true, true),
                fact("/r/a", 2, true, false),
                fact("/r/b", 3, true, false),
            ],
        );
        let mut scene = Scene::from_scan(Arc::new(original));
        let next = snapshot(
            vec![file("/r/a", 7, 1, 10), file("/r/c/new", 9, 8, 11)],
            vec![fact("/r", 1, true, true), fact("/r/c", 8, true, false)],
        );
        for change in changes(&scene, &next).unwrap() {
            scene.apply(change).unwrap();
        }
        assert!(scene.directory_id(Path::new("/r/a")).is_none());
        assert!(scene.directory_id(Path::new("/r/b")).is_none());
        assert!(scene.file_id(Path::new("/r/a")).is_some());
        assert!(scene.directory_id(Path::new("/r/c")).is_some());
        assert!(scene.file_id(Path::new("/r/c/new")).is_some());
    }
}
