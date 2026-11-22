//! Incremental directory-entry model. Paths identify directory entries; filesystem
//! identities identify objects and are used for validation, never deduplication.
use crate::{file_actions, scan};
use std::{
    collections::{BTreeSet, HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct EntryId {
    pub slot: u32,
    pub generation: u32,
}
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct DirectoryId {
    pub slot: u32,
    pub generation: u32,
}
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum TileId {
    File(EntryId),
    Directory(DirectoryId),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Files,
    Folders,
    Hybrid,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tile {
    pub id: TileId,
    pub path: PathBuf,
    pub parent: PathBuf,
    pub size: u64,
    pub identity: scan::FileIdentity,
    pub parent_identity: scan::FileIdentity,
    pub parent_is_leaf: bool,
    pub can_trash: bool,
}
impl Tile {
    pub fn to_entry(&self) -> scan::FileEntry {
        scan::FileEntry {
            path: self.path.clone(),
            parent: Arc::from(self.parent.as_path()),
            size: self.size,
            identity: self.identity,
            parent_identity: self.parent_identity,
            parent_is_leaf: self.parent_is_leaf,
            can_trash: self.can_trash,
        }
    }
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
}
struct Arena<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
}
impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
        }
    }
}
impl<T> Arena<T> {
    fn insert(&mut self, value: T) -> (u32, u32) {
        if let Some(slot) = self.free.pop() {
            let s = &mut self.slots[slot as usize];
            debug_assert!(s.value.is_none());
            s.value = Some(value);
            (slot, s.generation)
        } else {
            let slot = u32::try_from(self.slots.len()).expect("scene capacity exceeded");
            self.slots.push(Slot {
                generation: 0,
                value: Some(value),
            });
            (slot, 0)
        }
    }
    fn get(&self, slot: u32, generation: u32) -> Option<&T> {
        self.slots
            .get(slot as usize)
            .filter(|s| s.generation == generation)?
            .value
            .as_ref()
    }
    fn get_mut(&mut self, slot: u32, generation: u32) -> Option<&mut T> {
        self.slots
            .get_mut(slot as usize)
            .filter(|s| s.generation == generation)?
            .value
            .as_mut()
    }
    fn remove(&mut self, slot: u32, generation: u32) -> Option<T> {
        let s = self
            .slots
            .get_mut(slot as usize)
            .filter(|s| s.generation == generation)?;
        let value = s.value.take()?;
        // Retire an exhausted slot rather than permit an old ID to become valid.
        if let Some(next) = s.generation.checked_add(1) {
            s.generation = next;
            self.free.push(slot);
        }
        Some(value)
    }
}

struct Directory {
    fact: scan::DirectoryFact,
    files: Vec<EntryId>,
    size: u128,
    unsafe_count: usize,
}
impl Directory {
    fn new(fact: scan::DirectoryFact) -> Self {
        Self {
            fact,
            files: Vec::new(),
            size: 0,
            unsafe_count: 0,
        }
    }
    fn leaf(&self) -> bool {
        self.fact.enumeration_complete && !self.fact.has_subdirectories
    }
}

/// Directory changes must include refreshed facts for affected parents. In
/// particular, adding/removing an empty directory can change hybrid projection.
#[derive(Clone, Debug)]
pub enum Change {
    UpsertFile(scan::FileEntry),
    RemoveFile {
        path: PathBuf,
        identity: scan::FileIdentity,
    },
    MoveFile {
        from: PathBuf,
        identity: scan::FileIdentity,
        to: scan::FileEntry,
    },
    UpsertDirectory(scan::DirectoryFact),
    RemoveDirectory {
        path: PathBuf,
        identity: scan::FileIdentity,
    },
}
#[derive(Default, Debug)]
pub struct Impact {
    pub directories: BTreeSet<PathBuf>,
    pub files: BTreeSet<EntryId>,
    pub removed: BTreeSet<TileId>,
}
#[derive(Debug, PartialEq, Eq)]
pub enum ChangeError {
    MissingParent,
    MissingEntry,
    IdentityMismatch,
    DestinationExists,
    NonEmptyDirectory,
    ConflictingBatch,
    InvalidParent,
}

enum FileStorage {
    Source(usize),
    Owned(Box<scan::FileEntry>),
}
struct FileRecord {
    storage: FileStorage,
    next: Option<EntryId>,
    directory_index: u32,
}
fn path_hash(path: &Path) -> u64 {
    let mut h = DefaultHasher::new();
    path.hash(&mut h);
    h.finish()
}

pub struct Scene {
    root: PathBuf,
    source: Arc<scan::ScanResult>,
    files: Arena<FileRecord>,
    directories: Arena<Directory>,
    file_paths: HashMap<u64, EntryId>,
    directory_paths: HashMap<PathBuf, DirectoryId>,
}
impl Scene {
    #[cfg(test)]
    pub fn from_scan(data: Arc<scan::ScanResult>) -> Self {
        Self::from_scan_cancellable(data, &AtomicBool::new(false)).expect("never cancelled")
    }
    pub fn from_scan_cancellable(
        data: Arc<scan::ScanResult>,
        cancelled: &AtomicBool,
    ) -> Option<Self> {
        let mut scene = Self {
            root: data.root.clone(),
            source: data.clone(),
            files: Arena::default(),
            directories: Arena::default(),
            file_paths: HashMap::new(),
            directory_paths: HashMap::new(),
        };
        for (i, fact) in data.directories.iter().enumerate() {
            if i % 4096 == 0 && cancelled.load(Ordering::Relaxed) {
                return None;
            }
            scene.upsert_directory(fact.clone());
        }
        // A directory may disappear between discovery and enumeration. Preserve
        // the entry, but never infer leaf status from incomplete metadata.
        for (source_index, file) in data.files.iter().enumerate() {
            if source_index % 4096 == 0 && cancelled.load(Ordering::Relaxed) {
                return None;
            }
            if !scene.directory_paths.contains_key(file.parent.as_ref()) {
                scene.upsert_directory(scan::DirectoryFact {
                    path: file.parent.to_path_buf(),
                    identity: file.parent_identity,
                    enumeration_complete: false,
                    has_subdirectories: true,
                });
            }
            let previous = scene.file_paths.get(&path_hash(&file.path)).copied();
            let (slot, generation) = scene.files.insert(FileRecord {
                storage: FileStorage::Source(source_index),
                next: previous,
                directory_index: 0,
            });
            let id = EntryId { slot, generation };
            scene.attach(id, file).expect("scan parent is present");
            scene.file_paths.insert(path_hash(&file.path), id);
        }
        (!cancelled.load(Ordering::Relaxed)).then_some(scene)
    }
    fn directory(&self, id: DirectoryId) -> Option<&Directory> {
        self.directories.get(id.slot, id.generation)
    }
    fn directory_mut(&mut self, id: DirectoryId) -> Option<&mut Directory> {
        self.directories.get_mut(id.slot, id.generation)
    }
    pub fn file(&self, id: EntryId) -> Option<&scan::FileEntry> {
        match &self.files.get(id.slot, id.generation)?.storage {
            FileStorage::Source(index) => self.source.files.get(*index),
            FileStorage::Owned(file) => Some(file),
        }
    }
    #[cfg(test)]
    pub fn source_id(&self, index: usize) -> Option<EntryId> {
        if index >= self.source.files.len() {
            return None;
        }
        // Source entries are inserted in snapshot order, starting at generation
        // zero. A recycled slot has a different generation and is rejected.
        let id = EntryId {
            slot: u32::try_from(index).ok()?,
            generation: 0,
        };
        self.files.get(id.slot, id.generation)?;
        Some(id)
    }
    pub fn file_id(&self, path: &Path) -> Option<EntryId> {
        let mut cursor = self.file_paths.get(&path_hash(path)).copied();
        while let Some(id) = cursor {
            let record = self.files.get(id.slot, id.generation)?;
            if self.file(id)?.path == path {
                return Some(id);
            }
            cursor = record.next;
        }
        None
    }
    fn unlink_path(&mut self, path: &Path, target: EntryId) {
        let hash = path_hash(path);
        let mut cursor = self.file_paths.get(&hash).copied();
        let mut previous: Option<EntryId> = None;
        while let Some(id) = cursor {
            let next = self.files.get(id.slot, id.generation).and_then(|r| r.next);
            if id == target {
                if let Some(prev) = previous {
                    self.files.get_mut(prev.slot, prev.generation).unwrap().next = next;
                } else if let Some(next) = next {
                    self.file_paths.insert(hash, next);
                } else {
                    self.file_paths.remove(&hash);
                }
                return;
            }
            previous = Some(id);
            cursor = next;
        }
    }
    fn link_path(&mut self, path: &Path, id: EntryId) {
        let hash = path_hash(path);
        let previous = self.file_paths.insert(hash, id);
        self.files.get_mut(id.slot, id.generation).unwrap().next = previous;
    }
    pub fn directory_leaf(&self, path: &Path) -> Option<bool> {
        Some(self.directory(self.directory_id(path)?)?.leaf())
    }
    pub fn directory_identity(&self, id: DirectoryId) -> Option<scan::FileIdentity> {
        Some(self.directory(id)?.fact.identity)
    }
    pub fn directory_id(&self, path: &Path) -> Option<DirectoryId> {
        self.directory_paths.get(path).copied()
    }
    fn upsert_directory(
        &mut self,
        fact: scan::DirectoryFact,
    ) -> (DirectoryId, Option<DirectoryId>) {
        if let Some(id) = self.directory_id(&fact.path) {
            if self.directory(id).unwrap().fact.identity == fact.identity {
                self.directory_mut(id).unwrap().fact = fact;
                return (id, None);
            }
            // Preserve direct members, but change directory identity so old
            // confirmation and tile handles cannot address the replacement.
            let old = self.directories.remove(id.slot, id.generation).unwrap();
            let mut replacement = Directory::new(fact.clone());
            replacement.files = old.files;
            replacement.size = old.size;
            replacement.unsafe_count = old.unsafe_count;
            let (slot, generation) = self.directories.insert(replacement);
            let new = DirectoryId { slot, generation };
            self.directory_paths.insert(fact.path, new);
            return (new, Some(id));
        }
        let path = fact.path.clone();
        let (slot, generation) = self.directories.insert(Directory::new(fact));
        let id = DirectoryId { slot, generation };
        self.directory_paths.insert(path, id);
        (id, None)
    }
    fn attach(&mut self, id: EntryId, file: &scan::FileEntry) -> Result<(), ChangeError> {
        let dir_id = self
            .directory_id(&file.parent)
            .ok_or(ChangeError::MissingParent)?;
        let dir = self.directory_mut(dir_id).unwrap();
        let index = u32::try_from(dir.files.len()).expect("scene capacity exceeded");
        dir.files.push(id);
        dir.size += u128::from(file.size);
        dir.unsafe_count +=
            usize::from(!file.can_trash || file.parent_identity != dir.fact.identity);
        self.files
            .get_mut(id.slot, id.generation)
            .unwrap()
            .directory_index = index;
        Ok(())
    }
    fn detach(&mut self, id: EntryId, file: &scan::FileEntry) {
        let Some(dir_id) = self.directory_id(&file.parent) else {
            return;
        };
        let index = self
            .files
            .get(id.slot, id.generation)
            .expect("live file")
            .directory_index as usize;
        let dir = self.directory_mut(dir_id).unwrap();
        assert_eq!(
            dir.files.get(index),
            Some(&id),
            "directory membership invariant"
        );
        dir.files.swap_remove(index);
        dir.size -= u128::from(file.size);
        dir.unsafe_count -=
            usize::from(!file.can_trash || file.parent_identity != dir.fact.identity);
        let moved = dir.files.get(index).copied();
        if let Some(moved) = moved {
            self.files
                .get_mut(moved.slot, moved.generation)
                .expect("live moved file")
                .directory_index = index as u32;
        }
    }
    fn upsert_file(
        &mut self,
        file: scan::FileEntry,
    ) -> Result<(EntryId, Option<EntryId>), ChangeError> {
        if file.path.parent() != Some(file.parent.as_ref()) {
            return Err(ChangeError::InvalidParent);
        }
        if self.directory_id(&file.path).is_some() {
            return Err(ChangeError::DestinationExists);
        }
        if self.directory_id(&file.parent).is_none() {
            return Err(ChangeError::MissingParent);
        }
        let mut removed = None;
        if let Some(id) = self.file_id(&file.path) {
            let old = self.file(id).unwrap().clone();
            if old.identity == file.identity {
                self.detach(id, &old);
                self.attach(id, &file)?;
                self.files.get_mut(id.slot, id.generation).unwrap().storage =
                    FileStorage::Owned(Box::new(file));
                return Ok((id, None));
            }
            self.detach(id, &old);
            self.unlink_path(&old.path, id);
            self.files.remove(id.slot, id.generation);
            removed = Some(id);
        }
        let (slot, generation) = self.files.insert(FileRecord {
            storage: FileStorage::Owned(Box::new(file.clone())),
            next: None,
            directory_index: 0,
        });
        let id = EntryId { slot, generation };
        self.attach(id, &file)?;
        self.link_path(&file.path, id);
        Ok((id, removed))
    }
    /// A change is checked before mutating; stale identities cannot remove a replacement.
    pub fn apply(&mut self, change: Change) -> Result<Impact, ChangeError> {
        let mut impact = Impact::default();
        match change {
            Change::UpsertDirectory(fact) => {
                if self.file_id(&fact.path).is_some() {
                    return Err(ChangeError::DestinationExists);
                }
                if let Some(id) = self.directory_id(&fact.path) {
                    let existing = self.directory(id).unwrap();
                    if existing.fact.identity != fact.identity
                        && (!existing.files.is_empty()
                            || self
                                .directory_paths
                                .keys()
                                .any(|p| p != &fact.path && p.starts_with(&fact.path)))
                    {
                        return Err(ChangeError::NonEmptyDirectory);
                    }
                }
                impact.directories.insert(fact.path.clone());
                let (_, old) = self.upsert_directory(fact);
                if let Some(id) = old {
                    impact.removed.insert(TileId::Directory(id));
                }
            }
            Change::RemoveDirectory { path, identity } => {
                let id = self.directory_id(&path).ok_or(ChangeError::MissingEntry)?;
                let dir = self.directory(id).unwrap();
                if dir.fact.identity != identity {
                    return Err(ChangeError::IdentityMismatch);
                }
                if !dir.files.is_empty()
                    || self
                        .directory_paths
                        .keys()
                        .any(|p| p != &path && p.starts_with(&path))
                {
                    return Err(ChangeError::NonEmptyDirectory);
                }
                self.directories.remove(id.slot, id.generation);
                self.directory_paths.remove(&path);
                impact.directories.insert(path);
                impact.removed.insert(TileId::Directory(id));
            }
            Change::UpsertFile(file) => {
                impact.directories.insert(file.parent.to_path_buf());
                if let Some(old) = self.file_id(&file.path).and_then(|id| self.file(id)) {
                    impact.directories.insert(old.parent.to_path_buf());
                }
                let (id, old) = self.upsert_file(file)?;
                impact.files.insert(id);
                if let Some(id) = old {
                    impact.removed.insert(TileId::File(id));
                }
            }
            Change::RemoveFile { path, identity } => {
                let id = self.file_id(&path).ok_or(ChangeError::MissingEntry)?;
                let file = self.file(id).unwrap().clone();
                if file.identity != identity {
                    return Err(ChangeError::IdentityMismatch);
                }
                self.detach(id, &file);
                self.unlink_path(&path, id);
                self.files.remove(id.slot, id.generation);
                impact.directories.insert(file.parent.to_path_buf());
                impact.removed.insert(TileId::File(id));
            }
            Change::MoveFile { from, identity, to } => {
                let id = self.file_id(&from).ok_or(ChangeError::MissingEntry)?;
                let old = self.file(id).unwrap().clone();
                if old.identity != identity || to.identity != identity {
                    return Err(ChangeError::IdentityMismatch);
                }
                if (from != to.path && self.file_id(&to.path).is_some())
                    || self.directory_id(&to.path).is_some()
                {
                    return Err(ChangeError::DestinationExists);
                }
                if to.path.parent() != Some(to.parent.as_ref()) {
                    return Err(ChangeError::InvalidParent);
                }
                if self.directory_id(&to.parent).is_none() {
                    return Err(ChangeError::MissingParent);
                }
                self.detach(id, &old);
                self.attach(id, &to)?;
                self.unlink_path(&from, id);
                self.link_path(&to.path, id);
                impact.directories.insert(old.parent.to_path_buf());
                impact.directories.insert(to.parent.to_path_buf());
                impact.files.insert(id);
                self.files.get_mut(id.slot, id.generation).unwrap().storage =
                    FileStorage::Owned(Box::new(to));
            }
        }
        Ok(impact)
    }
    /// Apply a non-overlapping batch transactionally. Every operation is
    /// validated against the current state and the batch's new directories
    /// before any mutation. Conflicting paths are rejected for reconciliation.
    pub fn apply_batch(&mut self, changes: Vec<Change>) -> Result<Impact, ChangeError> {
        use std::collections::{HashMap, HashSet};
        let mut paths = HashSet::new();
        let mut dirs = HashMap::new();
        for change in &changes {
            match change {
                Change::UpsertDirectory(fact) => {
                    if dirs.insert(fact.path.clone(), true).is_some() {
                        return Err(ChangeError::ConflictingBatch);
                    }
                    if let Some(id) = self.directory_id(&fact.path) {
                        let old = self.directory(id).unwrap();
                        if old.fact.identity != fact.identity
                            && (!old.files.is_empty()
                                || self
                                    .directory_paths
                                    .keys()
                                    .any(|p| p != &fact.path && p.starts_with(&fact.path)))
                        {
                            return Err(ChangeError::NonEmptyDirectory);
                        }
                    }
                }
                Change::RemoveDirectory { path, identity } => {
                    if dirs.insert(path.clone(), false).is_some() {
                        return Err(ChangeError::ConflictingBatch);
                    }
                    let id = self.directory_id(path).ok_or(ChangeError::MissingEntry)?;
                    let old = self.directory(id).unwrap();
                    if old.fact.identity != *identity {
                        return Err(ChangeError::IdentityMismatch);
                    }
                    if !old.files.is_empty()
                        || self
                            .directory_paths
                            .keys()
                            .any(|p| p != path && p.starts_with(path))
                    {
                        return Err(ChangeError::NonEmptyDirectory);
                    }
                }
                _ => {}
            }
        }
        for (path, present) in &dirs {
            if *present && self.file_id(path).is_some() {
                return Err(ChangeError::DestinationExists);
            }
        }
        // A newly created descendant would make a prevalidated empty
        // directory non-empty before its removal is committed.
        for (path, present) in &dirs {
            if !present
                && dirs
                    .iter()
                    .any(|(other, created)| *created && other != path && other.starts_with(path))
            {
                return Err(ChangeError::NonEmptyDirectory);
            }
        }
        let parent_exists = |p: &Path| {
            dirs.get(p)
                .copied()
                .unwrap_or_else(|| self.directory_id(p).is_some())
        };
        for change in &changes {
            match change {
                Change::UpsertFile(file) => {
                    if !paths.insert(file.path.clone()) {
                        return Err(ChangeError::ConflictingBatch);
                    }
                    if file.path.parent() != Some(file.parent.as_ref()) {
                        return Err(ChangeError::InvalidParent);
                    }
                    if self.directory_id(&file.path).is_some() || dirs.contains_key(&file.path) {
                        return Err(ChangeError::DestinationExists);
                    }
                    if !parent_exists(&file.parent) {
                        return Err(ChangeError::MissingParent);
                    }
                }
                Change::RemoveFile { path, identity } => {
                    if !paths.insert(path.clone()) {
                        return Err(ChangeError::ConflictingBatch);
                    }
                    let id = self.file_id(path).ok_or(ChangeError::MissingEntry)?;
                    if self.file(id).unwrap().identity != *identity {
                        return Err(ChangeError::IdentityMismatch);
                    }
                }
                Change::MoveFile { from, identity, to } => {
                    if !paths.insert(from.clone())
                        || (from != &to.path && !paths.insert(to.path.clone()))
                    {
                        return Err(ChangeError::ConflictingBatch);
                    }
                    let id = self.file_id(from).ok_or(ChangeError::MissingEntry)?;
                    if self.file(id).unwrap().identity != *identity || to.identity != *identity {
                        return Err(ChangeError::IdentityMismatch);
                    }
                    if (from != &to.path && self.file_id(&to.path).is_some())
                        || self.directory_id(&to.path).is_some()
                        || dirs.contains_key(&to.path)
                    {
                        return Err(ChangeError::DestinationExists);
                    }
                    if to.path.parent() != Some(to.parent.as_ref()) {
                        return Err(ChangeError::InvalidParent);
                    }
                    if !parent_exists(&to.parent) {
                        return Err(ChangeError::MissingParent);
                    }
                }
                _ => {}
            }
        }
        // Directory creations must precede files regardless of caller order.
        let mut impact = Impact::default();
        let mut rest = Vec::new();
        for change in changes {
            if matches!(change, Change::UpsertDirectory(_)) {
                let i = self.apply(change).expect("validated directory");
                impact.directories.extend(i.directories);
                impact.files.extend(i.files);
                impact.removed.extend(i.removed);
            } else {
                rest.push(change);
            }
        }
        for change in rest {
            let i = self.apply(change).expect("validated change");
            impact.directories.extend(i.directories);
            impact.files.extend(i.files);
            impact.removed.extend(i.removed);
        }
        Ok(impact)
    }
    /// Validate a pre-existing projection slot against the current scene before
    /// binding its geometry/owner to a stable ID. Same inode is insufficient:
    /// sizes and grouping facts may have changed while the old layout survived.
    pub fn matches_entry(&self, id: TileId, entry: &scan::FileEntry) -> bool {
        match id {
            TileId::File(id) => self.file(id).is_some_and(|file| {
                file.path == entry.path
                    && file.parent == entry.parent
                    && file.size == entry.size
                    && file.identity == entry.identity
                    && file.parent_identity == entry.parent_identity
                    && self.directory_leaf(&file.parent) == Some(entry.parent_is_leaf)
                    && file.can_trash == entry.can_trash
            }),
            TileId::Directory(id) => self.directory(id).is_some_and(|dir| {
                let path = &dir.fact.path;
                !dir.files.is_empty()
                    && path == &entry.path
                    && path.parent().unwrap_or(path) == entry.parent.as_ref()
                    && dir.size.min(u128::from(u64::MAX)) as u64 == entry.size
                    && dir.fact.identity == entry.identity
                    && entry.parent_identity == scan::FileIdentity::default()
                    && !entry.parent_is_leaf
                    && entry.can_trash
                        == (dir.unsafe_count == 0
                            && path != &self.root
                            && file_actions::can_move_target(
                                path,
                                file_actions::TargetKind::Directory,
                            ))
            }),
        }
    }
    pub fn tile(&self, id: TileId) -> Option<Tile> {
        match id {
            TileId::File(id) => {
                let f = self.file(id)?;
                Some(Tile {
                    id: TileId::File(id),
                    path: f.path.clone(),
                    parent: f.parent.to_path_buf(),
                    size: f.size,
                    identity: f.identity,
                    parent_identity: f.parent_identity,
                    parent_is_leaf: self
                        .directory_id(&f.parent)
                        .and_then(|id| self.directory(id))
                        .is_some_and(Directory::leaf),
                    can_trash: f.can_trash,
                })
            }
            TileId::Directory(id) => {
                let d = self.directory(id)?;
                if d.files.is_empty() {
                    return None;
                }
                let path = &d.fact.path;
                Some(Tile {
                    id: TileId::Directory(id),
                    path: path.clone(),
                    parent: path.parent().unwrap_or(path).to_path_buf(),
                    size: d.size.min(u128::from(u64::MAX)) as u64,
                    identity: d.fact.identity,
                    parent_identity: scan::FileIdentity::default(),
                    parent_is_leaf: false,
                    can_trash: d.unsafe_count == 0
                        && path != &self.root
                        && file_actions::can_move_target(path, file_actions::TargetKind::Directory),
                })
            }
        }
    }
    /// Project one direct directory. Compare this before and after applying a
    /// change to update only its tiles; no unrelated directory is visited.
    pub fn project_directory(&self, view: View, path: &Path) -> Vec<Tile> {
        let Some(id) = self.directory_id(path) else {
            return Vec::new();
        };
        let d = self.directory(id).unwrap();
        let mut out = if view == View::Folders || (view == View::Hybrid && d.leaf()) {
            self.tile(TileId::Directory(id)).into_iter().collect()
        } else {
            d.files
                .iter()
                .filter_map(|id| self.tile(TileId::File(*id)))
                .collect::<Vec<_>>()
        };
        out.sort_unstable_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
        out
    }
    #[cfg(test)]
    pub fn project(&self, view: View) -> Vec<Tile> {
        let mut out = Vec::new();
        for path in self.directory_paths.keys() {
            out.extend(self.project_directory(view, path));
        }
        out.sort_unstable_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    fn identity(n: u64) -> scan::FileIdentity {
        scan::FileIdentity {
            device: n,
            ..Default::default()
        }
    }
    fn dir(path: &str, n: u64, complete: bool, children: bool) -> scan::DirectoryFact {
        scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: complete,
            has_subdirectories: children,
        }
    }
    fn file(path: &str, size: u64, object: u64, parent: u64) -> scan::FileEntry {
        let path = PathBuf::from(path);
        scan::FileEntry {
            parent: Arc::from(path.parent().unwrap()),
            path,
            size,
            identity: identity(object),
            parent_identity: identity(parent),
            parent_is_leaf: false,
            can_trash: true,
        }
    }
    fn fixture() -> scan::ScanResult {
        scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/f", 10, 3, 2),
                file("/disk/a/g", 5, 3, 2),
                file("/disk/b/h", 7, 4, 5),
            ],
            directories: vec![
                dir("/disk", 1, true, true),
                dir("/disk/a", 2, true, false),
                dir("/disk/b", 5, false, false),
                dir("/disk/empty", 6, true, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        }
    }
    #[test]
    fn projections_count_hardlinks_as_entries_and_respect_incomplete_directories() {
        let scene = Scene::from_scan(Arc::new(fixture()));
        assert_eq!(scene.project(View::Files).len(), 3);
        let folders = scene.project(View::Folders);
        assert_eq!(folders.len(), 2);
        assert_eq!(
            folders
                .iter()
                .find(|t| t.path == Path::new("/disk/a"))
                .unwrap()
                .size,
            15
        );
        let hybrid = scene.project(View::Hybrid);
        assert_eq!(hybrid.len(), 2);
        assert!(
            hybrid
                .iter()
                .any(|t| t.path == Path::new("/disk/a") && matches!(t.id, TileId::Directory(_)))
        );
        assert!(
            hybrid
                .iter()
                .any(|t| t.path == Path::new("/disk/b/h") && matches!(t.id, TileId::File(_)))
        );
        assert!(scene.directory_id(Path::new("/disk/empty")).is_some());
    }
    #[test]
    fn stale_handles_and_identity_guards_survive_slot_reuse() {
        let mut scene = Scene::from_scan(Arc::new(fixture()));
        let path = PathBuf::from("/disk/a/f");
        let first = scene.file_id(&path).unwrap();
        assert_eq!(
            scene
                .apply(Change::RemoveFile {
                    path: path.clone(),
                    identity: identity(99)
                })
                .unwrap_err(),
            ChangeError::IdentityMismatch
        );
        scene
            .apply(Change::RemoveFile {
                path: path.clone(),
                identity: identity(3),
            })
            .unwrap();
        scene
            .apply(Change::UpsertFile(file("/disk/a/f", 20, 8, 2)))
            .unwrap();
        let new = scene.file_id(&path).unwrap();
        assert_ne!(first, new);
        assert!(scene.file(first).is_none());
        assert_eq!(
            scene
                .apply(Change::RemoveFile {
                    path: path.clone(),
                    identity: identity(3)
                })
                .unwrap_err(),
            ChangeError::IdentityMismatch
        );
        assert_eq!(
            scene
                .tile(TileId::Directory(
                    scene.directory_id(Path::new("/disk/a")).unwrap()
                ))
                .unwrap()
                .size,
            25
        );
    }
    #[test]
    fn move_preserves_entry_handle_and_empty_directory_changes_hybrid_projection() {
        let mut scene = Scene::from_scan(Arc::new(fixture()));
        let id = scene.file_id(Path::new("/disk/a/f")).unwrap();
        scene
            .apply(Change::MoveFile {
                from: "/disk/a/f".into(),
                identity: identity(3),
                to: file("/disk/b/f", 10, 3, 5),
            })
            .unwrap();
        assert_eq!(scene.file_id(Path::new("/disk/b/f")), Some(id));
        assert_eq!(
            scene
                .apply(Change::MoveFile {
                    from: "/disk/b/f".into(),
                    identity: identity(3),
                    to: file("/disk/b/h", 10, 3, 5)
                })
                .unwrap_err(),
            ChangeError::DestinationExists
        );
        scene
            .apply(Change::UpsertDirectory(dir("/disk/a", 2, true, true)))
            .unwrap();
        let tiles = scene.project(View::Hybrid);
        assert!(
            tiles
                .iter()
                .any(|t| t.path == Path::new("/disk/a/g") && matches!(t.id, TileId::File(_)))
        );
        assert!(
            !tiles
                .iter()
                .any(|t| t.path == Path::new("/disk/a") && matches!(t.id, TileId::Directory(_)))
        );
    }
    #[test]
    fn batch_is_atomic_and_reuses_source_storage() {
        let source = Arc::new(fixture());
        let first = source.files[0].path.as_os_str().as_encoded_bytes().as_ptr();
        let mut scene = Scene::from_scan(source.clone());
        let original = scene.source_id(0).unwrap();
        assert_eq!(
            scene
                .file(original)
                .unwrap()
                .path
                .as_os_str()
                .as_encoded_bytes()
                .as_ptr(),
            first
        );
        let result = scene.apply_batch(vec![
            Change::RemoveFile {
                path: "/disk/a/f".into(),
                identity: identity(3),
            },
            Change::RemoveFile {
                path: "/disk/a/g".into(),
                identity: identity(88),
            },
        ]);
        assert_eq!(result.unwrap_err(), ChangeError::IdentityMismatch);
        assert_eq!(scene.source_id(0), Some(original));
        assert_eq!(scene.project(View::Files).len(), 3);
        let impact = scene
            .apply_batch(vec![
                Change::RemoveFile {
                    path: "/disk/a/f".into(),
                    identity: identity(3),
                },
                Change::UpsertFile(file("/disk/b/new", 9, 11, 5)),
            ])
            .unwrap();
        assert!(impact.removed.contains(&TileId::File(original)));
        assert!(impact.directories.contains(Path::new("/disk/a")));
        assert!(impact.directories.contains(Path::new("/disk/b")));
        assert!(scene.source_id(0).is_none());
        assert_eq!(scene.project(View::Files).len(), 3);
    }
    #[test]
    #[ignore = "large synthetic performance measurement"]
    fn scene_shared_snapshot_scale() {
        let count = std::env::var("SCENE_BENCH_FILES")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(250_000);
        let mut data = fixture();
        let parent: Arc<Path> = Arc::from(Path::new("/disk/a"));
        data.files = (0..count)
            .map(|n| scan::FileEntry {
                path: format!("/disk/a/{n:08}").into(),
                size: n,
                parent: Arc::clone(&parent),
                identity: identity(n + 10),
                parent_identity: identity(2),
                parent_is_leaf: true,
                can_trash: true,
            })
            .collect();
        let source = Arc::new(data);
        #[cfg(target_os = "macos")]
        fn peak_rss() -> u64 {
            let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
            unsafe {
                libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
                usage.assume_init().ru_maxrss as u64
            }
        }
        #[cfg(target_os = "macos")]
        let source_peak = peak_rss();
        let started = std::time::Instant::now();
        let scene = Scene::from_scan(source.clone());
        let elapsed = started.elapsed();
        let index_bytes = scene.files.slots.capacity() * std::mem::size_of::<Slot<FileRecord>>()
            + scene.file_paths.capacity() * (std::mem::size_of::<(u64, EntryId)>() + 1)
            + scene
                .directory(scene.directory_id(Path::new("/disk/a")).unwrap())
                .unwrap()
                .files
                .capacity()
                * std::mem::size_of::<EntryId>();
        eprintln!(
            "scene {count}: {:?}, approximate index capacity {:.1} MiB (excluding allocator/table overhead)",
            elapsed,
            index_bytes as f64 / 1048576.0
        );
        #[cfg(target_os = "macos")]
        eprintln!(
            "peak RSS: source {:.1} MiB, scene {:.1} MiB",
            source_peak as f64 / 1048576.0,
            peak_rss() as f64 / 1048576.0
        );
        assert!(Arc::ptr_eq(&scene.source, &source));
        assert_eq!(scene.source.files.len(), count as usize);
    }

    #[test]
    fn conflicting_directory_batch_does_not_partially_commit() {
        let mut scene = Scene::from_scan(Arc::new(fixture()));
        let child = dir("/disk/empty/child", 7, true, false);
        let result = scene.apply_batch(vec![
            Change::UpsertDirectory(child),
            Change::RemoveDirectory {
                path: "/disk/empty".into(),
                identity: identity(6),
            },
        ]);
        assert_eq!(result.unwrap_err(), ChangeError::NonEmptyDirectory);
        assert!(scene.directory_id(Path::new("/disk/empty/child")).is_none());
        assert!(scene.directory_id(Path::new("/disk/empty")).is_some());
    }
    #[test]
    fn saturated_size_can_recover_after_removal() {
        let mut data = fixture();
        data.files = vec![
            file("/disk/a/f", u64::MAX, 3, 2),
            file("/disk/a/g", 5, 4, 2),
        ];
        let mut scene = Scene::from_scan(Arc::new(data));
        let tile = TileId::Directory(scene.directory_id(Path::new("/disk/a")).unwrap());
        assert_eq!(scene.tile(tile).unwrap().size, u64::MAX);
        scene
            .apply(Change::RemoveFile {
                path: "/disk/a/f".into(),
                identity: identity(3),
            })
            .unwrap();
        assert_eq!(scene.tile(tile).unwrap().size, 5);
    }
}
