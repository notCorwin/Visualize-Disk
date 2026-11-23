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

#[derive(Clone)]
struct Slot<T> {
    generation: u32,
    value: Option<T>,
}
// A checkpoint shares immutable pages with the active scene. Mutating one
// entry copies at most one page, rather than the entire multi-million item
// arena or a very large directory membership vector.
const PAGE_SIZE: usize = 1024;
#[derive(Clone)]
struct PagedVec<T> {
    pages: Vec<Arc<Vec<T>>>,
    len: usize,
}
impl<T> Default for PagedVec<T> {
    fn default() -> Self {
        Self {
            pages: Vec::new(),
            len: 0,
        }
    }
}
impl<T: Clone> PagedVec<T> {
    fn len(&self) -> usize {
        self.len
    }
    fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn get(&self, index: usize) -> Option<&T> {
        if index >= self.len {
            return None;
        }
        self.pages.get(index / PAGE_SIZE)?.get(index % PAGE_SIZE)
    }
    fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index >= self.len {
            return None;
        }
        Arc::make_mut(self.pages.get_mut(index / PAGE_SIZE)?).get_mut(index % PAGE_SIZE)
    }
    fn push(&mut self, value: T) {
        if self.len % PAGE_SIZE == 0 {
            // Most directories contain only a few files. Reserving a full
            // page per directory would multiply memory for small folders.
            self.pages.push(Arc::new(Vec::new()));
        }
        Arc::make_mut(self.pages.last_mut().unwrap()).push(value);
        self.len += 1;
    }
    fn pop(&mut self) -> Option<T> {
        let result = Arc::make_mut(self.pages.last_mut()?).pop()?;
        self.len -= 1;
        if self.len % PAGE_SIZE == 0 {
            self.pages.pop();
        }
        Some(result)
    }
    fn swap_remove(&mut self, index: usize) -> T {
        assert!(index < self.len);
        let last = self.pop().unwrap();
        if index == self.len {
            last
        } else {
            std::mem::replace(self.get_mut(index).unwrap(), last)
        }
    }
    fn iter(&self) -> impl Iterator<Item = &T> {
        self.pages.iter().flat_map(|page| page.iter())
    }
    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.pages.iter().map(|p| p.capacity()).sum()
    }
}
impl<T: Clone> IntoIterator for PagedVec<T> {
    type Item = T;
    type IntoIter = PagedIntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        PagedIntoIter {
            pages: self.pages.into_iter(),
            current: Vec::new().into_iter(),
        }
    }
}
struct PagedIntoIter<T> {
    pages: std::vec::IntoIter<Arc<Vec<T>>>,
    current: std::vec::IntoIter<T>,
}
impl<T: Clone> Iterator for PagedIntoIter<T> {
    type Item = T;
    fn next(&mut self) -> Option<T> {
        loop {
            if let Some(item) = self.current.next() {
                return Some(item);
            }
            self.current = Arc::unwrap_or_clone(self.pages.next()?).into_iter();
        }
    }
}

#[derive(Clone)]
struct Arena<T> {
    slots: PagedVec<Slot<T>>,
    free: PagedVec<u32>,
}
impl<T> Default for Arena<T> {
    fn default() -> Self {
        Self {
            slots: PagedVec::default(),
            free: PagedVec::default(),
        }
    }
}
impl<T: Clone> Arena<T> {
    fn insert(&mut self, value: T) -> (u32, u32) {
        if let Some(slot) = self.free.pop() {
            let s = self.slots.get_mut(slot as usize).unwrap();
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

#[derive(Clone)]
struct Directory {
    fact: scan::DirectoryFact,
    files: PagedVec<EntryId>,
    size: u128,
    unsafe_count: usize,
}
impl Directory {
    fn new(fact: scan::DirectoryFact) -> Self {
        Self {
            fact,
            files: PagedVec::default(),
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
    /// Remove a previously fully enumerated leaf and its direct files. The
    /// parent fact must be freshly enumerated after the filesystem operation.
    RemoveLeafDirectory {
        path: PathBuf,
        identity: scan::FileIdentity,
        parent: scan::DirectoryFact,
    },
    /// Remove a previously scanned directory subtree after its root was moved
    /// to Trash. The parent is independently enumerated after the operation.
    RemoveSubtreeDirectory {
        path: PathBuf,
        identity: scan::FileIdentity,
        parent: scan::DirectoryFact,
    },
    /// Native Trash confirmed the old object moved, but its parent could not
    /// be safely re-enumerated. The parent remains conservatively incomplete.
    RemoveConfirmedSubtree {
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
    Cancelled,
}

#[derive(Clone)]
enum FileStorage {
    Source(usize),
    Owned(Box<scan::FileEntry>),
}
#[derive(Clone)]
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

// Hashes are already well distributed; shard by their high bits so a single
// path edit only copies one small table while checkpoints share the rest.
const PATH_SHARDS: usize = 256;
#[derive(Clone)]
struct FilePathIndex {
    shards: Vec<Arc<HashMap<u64, EntryId>>>,
}
impl Default for FilePathIndex {
    fn default() -> Self {
        Self {
            shards: (0..PATH_SHARDS).map(|_| Arc::new(HashMap::new())).collect(),
        }
    }
}
impl FilePathIndex {
    fn shard(hash: u64) -> usize {
        (hash >> 56) as usize
    }
    fn get(&self, hash: &u64) -> Option<&EntryId> {
        self.shards[Self::shard(*hash)].get(hash)
    }
    fn insert(&mut self, hash: u64, id: EntryId) -> Option<EntryId> {
        Arc::make_mut(&mut self.shards[Self::shard(hash)]).insert(hash, id)
    }
    fn remove(&mut self, hash: &u64) -> Option<EntryId> {
        Arc::make_mut(&mut self.shards[Self::shard(*hash)]).remove(hash)
    }
    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.shards.iter().map(|s| s.capacity()).sum()
    }
}

// Directory-dense trees can be as large as file-dense trees. Keep their
// path and completeness indexes sharded as well, so checkpoints do not
// duplicate every directory path.
#[derive(Clone)]
struct DirectoryPathMap<V> {
    shards: Vec<Arc<HashMap<PathBuf, V>>>,
}
impl<V> Default for DirectoryPathMap<V> {
    fn default() -> Self {
        Self {
            shards: (0..PATH_SHARDS).map(|_| Arc::new(HashMap::new())).collect(),
        }
    }
}
impl<V: Clone> DirectoryPathMap<V> {
    fn shard(path: &Path) -> usize {
        FilePathIndex::shard(path_hash(path))
    }
    fn get(&self, path: &Path) -> Option<&V> {
        self.shards[Self::shard(path)].get(path)
    }
    fn get_mut(&mut self, path: &Path) -> Option<&mut V> {
        Arc::make_mut(&mut self.shards[Self::shard(path)]).get_mut(path)
    }
    fn contains_key(&self, path: &Path) -> bool {
        self.get(path).is_some()
    }
    fn insert(&mut self, path: PathBuf, value: V) -> Option<V> {
        Arc::make_mut(&mut self.shards[Self::shard(&path)]).insert(path, value)
    }
    fn remove(&mut self, path: &Path) -> Option<V> {
        Arc::make_mut(&mut self.shards[Self::shard(path)]).remove(path)
    }
    fn keys(&self) -> impl Iterator<Item = &PathBuf> {
        self.shards.iter().flat_map(|shard| shard.keys())
    }
    fn iter(&self) -> impl Iterator<Item = (&PathBuf, &V)> {
        self.shards.iter().flat_map(|shard| shard.iter())
    }
}

#[derive(Clone)]
pub struct Scene {
    root: PathBuf,
    source: Arc<scan::ScanResult>,
    files: Arena<FileRecord>,
    directories: Arena<Directory>,
    file_paths: FilePathIndex,
    directory_paths: DirectoryPathMap<DirectoryId>,
    // Number of incompletely enumerated or excluded directories in each
    // subtree. Directory actions cover descendants, not just direct files.
    incomplete_subtrees: DirectoryPathMap<usize>,
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
            file_paths: FilePathIndex::default(),
            directory_paths: DirectoryPathMap::default(),
            incomplete_subtrees: DirectoryPathMap::default(),
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
                    excluded: false,
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
    /// A missing directory or a complete leaf without direct files has no
    /// projected tiles in any view. Incomplete directories are never assumed
    /// empty, even if their current scan happened to find no files.
    pub(crate) fn absent_or_empty_complete_leaf(&self, path: &Path) -> bool {
        let Some(id) = self.directory_id(path) else {
            return true;
        };
        self.directory(id)
            .is_some_and(|dir| !dir.fact.excluded && dir.leaf() && dir.files.is_empty())
    }
    /// Whether the scene currently knows of no direct files. This says
    /// nothing about unenumerated filesystem entries.
    pub(crate) fn directory_has_no_known_files(&self, path: &Path) -> bool {
        self.directory_id(path)
            .and_then(|id| self.directory(id))
            .is_some_and(|dir| dir.files.is_empty())
    }
    pub fn directory_identity(&self, id: DirectoryId) -> Option<scan::FileIdentity> {
        Some(self.directory(id)?.fact.identity)
    }
    pub fn directory_id(&self, path: &Path) -> Option<DirectoryId> {
        self.directory_paths.get(path).copied()
    }
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    fn contains_directory_path(&self, path: &Path) -> bool {
        path.strip_prefix(&self.root).ok().is_some_and(|relative| {
            !relative
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        })
    }

    /// Reject lexical escapes before looking up identities or mutating any
    /// member of a batch. Filesystem identity checks remain mandatory at the
    /// operation boundary; this only enforces the scene's scan-root scope.
    fn validate_change_paths(&self, change: &Change) -> Result<(), ChangeError> {
        let within = |path: &Path| self.contains_directory_path(path);
        let file_path = |path: &Path| path != self.root && within(path);
        let directory_removal = |path: &Path| path != self.root && within(path);
        let entry = |file: &scan::FileEntry| {
            file_path(&file.path)
                && within(&file.parent)
                && file.path.parent() == Some(file.parent.as_ref())
        };
        let valid = match change {
            Change::UpsertDirectory(fact) => within(&fact.path),
            Change::RemoveDirectory { path, .. } | Change::RemoveConfirmedSubtree { path, .. } => {
                directory_removal(path)
            }
            Change::RemoveLeafDirectory { path, parent, .. }
            | Change::RemoveSubtreeDirectory { path, parent, .. } => {
                directory_removal(path)
                    && within(&parent.path)
                    && path.parent() == Some(parent.path.as_path())
            }
            Change::UpsertFile(file) => entry(file),
            Change::RemoveFile { path, .. } => file_path(path),
            Change::MoveFile { from, to, .. } => file_path(from) && entry(to),
        };
        if valid {
            Ok(())
        } else {
            Err(ChangeError::InvalidParent)
        }
    }
    pub(crate) fn subtree_directories(&self, path: &Path) -> Vec<PathBuf> {
        self.directory_paths
            .keys()
            .filter(|p| p.starts_with(path))
            .cloned()
            .collect()
    }
    pub(crate) fn for_each_directory(&self, mut visit: impl FnMut(&scan::DirectoryFact)) {
        for slot in self.directories.slots.iter() {
            if let Some(dir) = &slot.value {
                visit(&dir.fact);
            }
        }
    }
    pub(crate) fn for_each_file(&self, mut visit: impl FnMut(&scan::FileEntry)) {
        for (index, slot) in self.files.slots.iter().enumerate() {
            if slot.value.is_some() {
                let id = EntryId {
                    slot: index as u32,
                    generation: slot.generation,
                };
                if let Some(file) = self.file(id) {
                    visit(file);
                }
            }
        }
    }
    fn upsert_directory(
        &mut self,
        fact: scan::DirectoryFact,
    ) -> (DirectoryId, Option<DirectoryId>) {
        let was_incomplete = self
            .directory_id(&fact.path)
            .and_then(|id| self.directory(id))
            .is_some_and(|dir| !dir.fact.enumeration_complete || dir.fact.excluded);
        let is_incomplete = !fact.enumeration_complete || fact.excluded;
        if was_incomplete != is_incomplete {
            self.adjust_incomplete_subtrees(&fact.path, is_incomplete);
        }
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
    fn adjust_incomplete_subtrees(&mut self, path: &Path, adding: bool) {
        let root = &self.root;
        for ancestor in path.ancestors().take_while(|p| p.starts_with(root)) {
            if adding {
                if let Some(count) = self.incomplete_subtrees.get_mut(ancestor) {
                    *count += 1;
                } else {
                    self.incomplete_subtrees.insert(ancestor.to_path_buf(), 1);
                }
            } else {
                let count = self
                    .incomplete_subtrees
                    .get_mut(ancestor)
                    .expect("incomplete subtree count must exist");
                *count -= 1;
                if *count == 0 {
                    self.incomplete_subtrees.remove(ancestor);
                }
            }
        }
    }
    fn remove_directory_record(&mut self, id: DirectoryId, path: &Path) {
        let incomplete = self
            .directory(id)
            .is_some_and(|d| !d.fact.enumeration_complete || d.fact.excluded);
        if incomplete {
            self.adjust_incomplete_subtrees(path, false);
        }
        self.directories.remove(id.slot, id.generation);
        self.directory_paths.remove(path);
    }
    fn directory_is_complete(&self, path: &Path) -> bool {
        !self.incomplete_subtrees.contains_key(path)
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
        self.validate_change_paths(&change)?;
        let mut impact = Impact::default();
        match change {
            Change::RemoveConfirmedSubtree { path, identity } => {
                let parent_path = path.parent().ok_or(ChangeError::InvalidParent)?;
                let parent_id = self
                    .directory_id(parent_path)
                    .ok_or(ChangeError::MissingParent)?;
                let mut parent = self.directory(parent_id).unwrap().fact.clone();
                // This synthetic value is only used to reuse the identity-
                // checked atomic remove; it is never published as complete.
                parent.enumeration_complete = true;
                parent.has_subdirectories = true;
                let impact = self.apply(Change::RemoveSubtreeDirectory {
                    path,
                    identity,
                    parent: parent.clone(),
                })?;
                parent.enumeration_complete = false;
                self.upsert_directory(parent);
                return Ok(impact);
            }
            Change::RemoveSubtreeDirectory {
                path,
                identity,
                parent,
            } => {
                if path == self.root || !path.starts_with(&self.root) {
                    return Err(ChangeError::InvalidParent);
                }
                let root_id = self.directory_id(&path).ok_or(ChangeError::MissingEntry)?;
                if self.directory(root_id).unwrap().fact.identity != identity {
                    return Err(ChangeError::IdentityMismatch);
                }
                if path.parent() != Some(parent.path.as_path()) || !parent.enumeration_complete {
                    return Err(ChangeError::InvalidParent);
                }
                let parent_id = self
                    .directory_id(&parent.path)
                    .ok_or(ChangeError::MissingParent)?;
                if self.directory(parent_id).unwrap().fact.identity != parent.identity {
                    return Err(ChangeError::IdentityMismatch);
                }
                if !parent.has_subdirectories
                    && self.directory_paths.keys().any(|other| {
                        !other.starts_with(&path) && other.parent() == Some(parent.path.as_path())
                    })
                {
                    return Err(ChangeError::ConflictingBatch);
                }
                // Validate everything before the first mutation. Directory membership
                // owns its files, so collect each descendant directory only once.
                let mut directories: Vec<_> = self
                    .directory_paths
                    .iter()
                    .filter(|(candidate, _)| candidate.starts_with(&path))
                    .map(|(candidate, id)| (candidate.clone(), *id))
                    .collect();
                // Replaying the same journal from a fresh scan must retire
                // slots in the same order, independent of HashMap seeds.
                directories.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                for (directory_path, id) in directories {
                    let files = self.directory(id).expect("live directory").files.clone();
                    for file_id in files {
                        let file = self.file(file_id).expect("live directory file").clone();
                        self.detach(file_id, &file);
                        self.unlink_path(&file.path, file_id);
                        self.files.remove(file_id.slot, file_id.generation);
                        impact.removed.insert(TileId::File(file_id));
                    }
                    self.remove_directory_record(id, &directory_path);
                    impact.directories.insert(directory_path);
                    impact.removed.insert(TileId::Directory(id));
                }
                self.upsert_directory(parent.clone());
                impact.directories.insert(parent.path);
            }
            Change::RemoveLeafDirectory {
                path,
                identity,
                parent,
            } => {
                if path == self.root {
                    return Err(ChangeError::InvalidParent);
                }
                let id = self.directory_id(&path).ok_or(ChangeError::MissingEntry)?;
                let dir = self.directory(id).unwrap();
                if dir.fact.identity != identity {
                    return Err(ChangeError::IdentityMismatch);
                }
                if !dir.leaf()
                    || self
                        .directory_paths
                        .keys()
                        .any(|p| p != &path && p.starts_with(&path))
                {
                    return Err(ChangeError::NonEmptyDirectory);
                }
                if path.parent() != Some(parent.path.as_path()) || !parent.enumeration_complete {
                    return Err(ChangeError::InvalidParent);
                }
                let parent_id = self
                    .directory_id(&parent.path)
                    .ok_or(ChangeError::MissingParent)?;
                if self.directory(parent_id).unwrap().fact.identity != parent.identity {
                    return Err(ChangeError::IdentityMismatch);
                }
                // A post-delete leaf fact contradicts a still-cached sibling:
                // another external change occurred and requires a rescan.
                if !parent.has_subdirectories
                    && self.directory_paths.keys().any(|other| {
                        other != &path && other.parent() == Some(parent.path.as_path())
                    })
                {
                    return Err(ChangeError::ConflictingBatch);
                }
                let files = dir.files.clone();
                for id in files {
                    let file = self.file(id).expect("live directory file").clone();
                    self.detach(id, &file);
                    self.unlink_path(&file.path, id);
                    self.files.remove(id.slot, id.generation);
                    impact.removed.insert(TileId::File(id));
                }
                self.remove_directory_record(id, &path);
                self.upsert_directory(parent.clone());
                impact.directories.insert(path);
                impact.directories.insert(parent.path);
                impact.removed.insert(TileId::Directory(id));
            }
            Change::UpsertDirectory(fact) => {
                if !self.contains_directory_path(&fact.path) {
                    return Err(ChangeError::InvalidParent);
                }
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
                self.remove_directory_record(id, &path);
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
        // Validate the entire transaction before any operation can mutate
        // scene state, including single-item subtree operations.
        for change in &changes {
            self.validate_change_paths(change)?;
        }
        // A leaf removal is a self-contained atomic operation: mixing it with
        // other paths would require validating interactions before mutation.
        if changes.iter().any(|c| {
            matches!(
                c,
                Change::RemoveLeafDirectory { .. }
                    | Change::RemoveSubtreeDirectory { .. }
                    | Change::RemoveConfirmedSubtree { .. }
            )
        }) {
            if changes.len() != 1 {
                return Err(ChangeError::ConflictingBatch);
            }
            return self.apply(changes.into_iter().next().unwrap());
        }
        let mut paths = HashSet::new();
        let mut dirs = HashMap::new();
        for change in &changes {
            match change {
                Change::UpsertDirectory(fact) => {
                    if !self.contains_directory_path(&fact.path) {
                        return Err(ChangeError::InvalidParent);
                    }
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
                    && self.file_can_trash(file) == entry.can_trash
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
                            && self.directory_is_complete(path)
                            && path != &self.root
                            && file_actions::can_move_target(
                                path,
                                file_actions::TargetKind::Directory,
                            ))
            }),
        }
    }
    fn file_can_trash(&self, file: &scan::FileEntry) -> bool {
        file.can_trash
            && self
                .directory_id(&file.parent)
                .and_then(|id| self.directory(id))
                .is_some_and(|parent| parent.fact.identity == file.parent_identity)
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
                    can_trash: self.file_can_trash(f),
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
                        && self.directory_is_complete(path)
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

    #[test]
    fn checkpoint_pages_isolate_boundary_swaps_and_generation_reuse() {
        let mut arena = Arena::default();
        for value in 0..(PAGE_SIZE * 2 + 1) {
            arena.insert(value);
        }
        let checkpoint = arena.clone();
        assert!(Arc::ptr_eq(
            &checkpoint.slots.pages[0],
            &arena.slots.pages[0]
        ));
        assert_eq!(arena.remove(PAGE_SIZE as u32, 0), Some(PAGE_SIZE));
        assert!(!Arc::ptr_eq(
            &checkpoint.slots.pages[1],
            &arena.slots.pages[1]
        ));
        assert!(Arc::ptr_eq(
            &checkpoint.slots.pages[0],
            &arena.slots.pages[0]
        ));
        let (slot, generation) = arena.insert(99);
        assert_eq!((slot, generation), (PAGE_SIZE as u32, 1));
        assert_eq!(checkpoint.get(slot, 0), Some(&PAGE_SIZE));
        assert_eq!(arena.get(slot, 0), None);
        assert_eq!(arena.get(slot, 1), Some(&99));

        let mut members = PagedVec::default();
        for value in 0..(PAGE_SIZE + 1) {
            members.push(value);
        }
        let old = members.clone();
        assert_eq!(members.swap_remove(0), 0);
        assert_eq!(members.get(0), Some(&PAGE_SIZE));
        assert_eq!(old.get(0), Some(&0));
        assert_eq!(old.len(), PAGE_SIZE + 1);
    }

    #[test]
    fn checkpoint_path_index_copies_only_affected_hash_shard() {
        let mut index = FilePathIndex::default();
        let a = 0x0100_0000_0000_0001;
        let b = 0x0200_0000_0000_0001;
        let first = EntryId {
            slot: 1,
            generation: 0,
        };
        let replacement = EntryId {
            slot: 1,
            generation: 1,
        };
        index.insert(a, first);
        index.insert(b, first);
        let old = index.clone();
        index.insert(a, replacement);
        assert_eq!(old.get(&a), Some(&first));
        assert_eq!(index.get(&a), Some(&replacement));
        assert!(!Arc::ptr_eq(&old.shards[1], &index.shards[1]));
        assert!(Arc::ptr_eq(&old.shards[2], &index.shards[2]));
        assert_eq!(index.remove(&a), Some(replacement));
        assert_eq!(old.get(&a), Some(&first));
    }

    #[test]
    fn checkpoint_directory_indexes_share_and_isolate_changes() {
        let mut paths = DirectoryPathMap::default();
        let a = PathBuf::from("/disk/a");
        let id = DirectoryId {
            slot: 7,
            generation: 1,
        };
        paths.insert(a.clone(), id);
        let old = paths.clone();
        let shard = DirectoryPathMap::<DirectoryId>::shard(&a);
        assert!(Arc::ptr_eq(&old.shards[shard], &paths.shards[shard]));
        paths.remove(&a);
        assert_eq!(old.get(&a), Some(&id));
        assert_eq!(paths.get(&a), None);
        assert!(!Arc::ptr_eq(&old.shards[shard], &paths.shards[shard]));
        assert!(old.keys().any(|key| key == &a));
    }
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
            excluded: false,
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
    fn directory_updates_cannot_escape_scan_root_or_partially_apply_a_batch() {
        let mut scene = Scene::from_scan(Arc::new(fixture()));
        for outside in ["/elsewhere", "/disk-other/new", "/disk/../elsewhere"] {
            assert!(matches!(
                scene.apply(Change::UpsertDirectory(dir(outside, 10, true, false))),
                Err(ChangeError::InvalidParent)
            ));
            assert!(scene.directory_id(Path::new(outside)).is_none());
        }
        let valid = "/disk/new";
        assert!(matches!(
            scene.apply_batch(vec![
                Change::UpsertDirectory(dir(valid, 11, true, false)),
                Change::UpsertDirectory(dir("/elsewhere", 12, true, false)),
            ]),
            Err(ChangeError::InvalidParent)
        ));
        assert!(scene.directory_id(Path::new(valid)).is_none());
        assert!(scene.directory_id(Path::new("/disk/a")).is_some());
        scene
            .apply_batch(vec![Change::UpsertDirectory(dir(valid, 11, true, false))])
            .unwrap();
        assert!(scene.directory_id(Path::new(valid)).is_some());
    }

    #[test]
    fn every_change_rejects_root_escape_before_any_batch_mutation() {
        let original = Arc::new(fixture());
        let bad_changes = vec![
            Change::UpsertDirectory(dir("/disk/../outside", 11, true, false)),
            Change::RemoveDirectory {
                path: "/disk".into(),
                identity: identity(1),
            },
            Change::RemoveDirectory {
                path: "/disk/../outside".into(),
                identity: identity(1),
            },
            Change::RemoveLeafDirectory {
                path: "/disk".into(),
                identity: identity(1),
                parent: dir("/", 0, true, true),
            },
            Change::RemoveSubtreeDirectory {
                path: "/disk/../outside".into(),
                identity: identity(1),
                parent: dir("/disk/..", 0, true, true),
            },
            Change::UpsertFile(file("/disk/../outside/new", 1, 7, 1)),
            Change::RemoveFile {
                path: "/disk/../outside/item".into(),
                identity: identity(7),
            },
            Change::MoveFile {
                from: "/disk/a/f".into(),
                identity: identity(3),
                to: file("/disk/../outside/moved", 10, 3, 1),
            },
        ];
        for bad in bad_changes {
            let mut scene = Scene::from_scan(original.clone());
            let before = scene.file_id(Path::new("/disk/a/f"));
            let valid = Change::UpsertFile(file("/disk/a/new", 1, 20, 2));
            assert!(
                matches!(
                    scene.apply_batch(vec![valid, bad.clone()]),
                    Err(ChangeError::InvalidParent)
                ),
                "{bad:?}"
            );
            assert!(scene.file_id(Path::new("/disk/a/new")).is_none());
            assert_eq!(scene.file_id(Path::new("/disk/a/f")), before);
            assert!(matches!(scene.apply(bad), Err(ChangeError::InvalidParent)));
        }
    }
    #[test]
    fn collision_chain_mutation_does_not_change_checkpoint() {
        let mut scene = Scene::from_scan(Arc::new(fixture()));
        let path = Path::new("/disk/a/f");
        let first = scene.file_id(path).unwrap();
        let collision = scene.file_id(Path::new("/disk/a/g")).unwrap();
        // Construct the linked chain that two equal path hashes would create.
        scene.file_paths.insert(path_hash(path), collision);
        scene
            .files
            .get_mut(collision.slot, collision.generation)
            .unwrap()
            .next = Some(first);
        let checkpoint = scene.clone();
        assert_eq!(scene.file_id(path), Some(first));
        scene.unlink_path(path, first);
        assert_eq!(scene.file_id(path), None);
        let removed = scene.files.remove(first.slot, first.generation).unwrap();
        let (reused, generation) = scene.files.insert(removed);
        assert_eq!(reused, first.slot);
        assert_ne!(generation, first.generation);
        assert_eq!(checkpoint.file_id(path), Some(first));
        assert_eq!(
            checkpoint
                .files
                .get(collision.slot, collision.generation)
                .unwrap()
                .next,
            Some(first)
        );
    }
    #[test]
    fn folder_delete_requires_every_descendant_to_be_enumerated() {
        let mut data = fixture();
        data.directories = vec![
            dir("/disk", 1, true, true),
            dir("/disk/a", 2, true, true),
            dir("/disk/a/unknown", 7, false, false),
            dir("/disk/b", 5, true, false),
        ]
        .into();
        let mut scene = Scene::from_scan(Arc::new(data));
        let a = TileId::Directory(scene.directory_id(Path::new("/disk/a")).unwrap());
        let b = TileId::Directory(scene.directory_id(Path::new("/disk/b")).unwrap());
        assert!(!scene.tile(a).unwrap().can_trash);
        assert!(scene.tile(b).unwrap().can_trash);
        let old = scene.clone();
        scene
            .apply(Change::UpsertDirectory(dir(
                "/disk/a/unknown",
                7,
                true,
                false,
            )))
            .unwrap();
        assert!(scene.tile(a).unwrap().can_trash);
        assert!(!old.tile(a).unwrap().can_trash);
        let mut excluded = dir("/disk/a/unknown", 7, false, false);
        excluded.excluded = true;
        scene.apply(Change::UpsertDirectory(excluded)).unwrap();
        assert!(!scene.tile(a).unwrap().can_trash);
        scene
            .apply(Change::RemoveDirectory {
                path: "/disk/a/unknown".into(),
                identity: identity(7),
            })
            .unwrap();
        assert!(scene.tile(a).unwrap().can_trash);
    }
    #[test]
    fn subtree_removal_is_atomic_and_preserves_unrelated_entries() {
        let mut data = fixture();
        data.files.push(file("/disk/a/nested/item", 11, 8, 7));
        data.directories = vec![
            dir("/disk", 1, true, true),
            dir("/disk/a", 2, true, true),
            dir("/disk/a/nested", 7, false, true),
            dir("/disk/a/nested/empty", 9, true, false),
            dir("/disk/b", 5, false, false),
        ]
        .into();
        let mut scene = Scene::from_scan(Arc::new(data));
        let old = scene.project(View::Files);
        let unrelated = scene.file_id(Path::new("/disk/b/h")).unwrap();
        let removed = scene.file_id(Path::new("/disk/a/nested/item")).unwrap();
        let change = |id, parent| Change::RemoveSubtreeDirectory {
            path: "/disk/a".into(),
            identity: identity(id),
            parent,
        };
        assert_eq!(
            scene
                .apply_batch(vec![change(99, dir("/disk", 1, true, true))])
                .unwrap_err(),
            ChangeError::IdentityMismatch
        );
        assert_eq!(scene.project(View::Files), old);
        assert_eq!(
            scene
                .apply_batch(vec![change(2, dir("/disk", 99, true, true))])
                .unwrap_err(),
            ChangeError::IdentityMismatch
        );
        assert_eq!(scene.project(View::Files), old);
        assert_eq!(
            scene
                .apply_batch(vec![change(2, dir("/disk", 1, false, true))])
                .unwrap_err(),
            ChangeError::InvalidParent
        );
        assert_eq!(scene.project(View::Files), old);
        let impact = scene
            .apply_batch(vec![change(2, dir("/disk", 1, true, true))])
            .unwrap();
        assert!(impact.removed.contains(&TileId::File(removed)));
        assert!(scene.file(removed).is_none());
        assert!(
            scene
                .directory_id(Path::new("/disk/a/nested/empty"))
                .is_none()
        );
        assert_eq!(scene.file_id(Path::new("/disk/b/h")), Some(unrelated));
        assert_eq!(scene.project(View::Files).len(), 1);
    }
    #[test]
    fn confirmed_subtree_removal_keeps_parent_conservatively_incomplete() {
        let mut data = fixture();
        data.files.push(file("/disk/a/nested/item", 11, 8, 7));
        data.directories = vec![
            dir("/disk", 1, true, true),
            dir("/disk/a", 2, true, true),
            dir("/disk/a/nested", 7, true, false),
            dir("/disk/b", 5, true, false),
        ]
        .into();
        let mut scene = Scene::from_scan(Arc::new(data));
        let old_id = scene.directory_id(Path::new("/disk/a/nested")).unwrap();
        let parent_id = scene.directory_id(Path::new("/disk/a")).unwrap();
        let action = |id| Change::RemoveConfirmedSubtree {
            path: "/disk/a/nested".into(),
            identity: identity(id),
        };
        assert_eq!(
            scene.apply_batch(vec![action(99)]).unwrap_err(),
            ChangeError::IdentityMismatch
        );
        assert!(scene.directory(old_id).is_some());
        assert!(scene.tile(TileId::Directory(parent_id)).unwrap().can_trash);
        scene.apply_batch(vec![action(7)]).unwrap();
        assert!(scene.directory(old_id).is_none());
        assert_eq!(scene.directory_leaf(Path::new("/disk/a")), Some(false));
        assert!(!scene.tile(TileId::Directory(parent_id)).unwrap().can_trash);
        assert!(scene.file_id(Path::new("/disk/b/h")).is_some());
        scene
            .apply_batch(vec![Change::UpsertDirectory(dir(
                "/disk/a", 2, true, false,
            ))])
            .unwrap();
        assert!(scene.tile(TileId::Directory(parent_id)).unwrap().can_trash);
        assert_eq!(scene.directory_leaf(Path::new("/disk/a")), Some(true));
    }
    #[test]
    fn cloned_scene_isolated_for_reconciliation() {
        let scene = Scene::from_scan(Arc::new(fixture()));
        let mut shadow = scene.clone();
        let id = scene.file_id(Path::new("/disk/a/f")).unwrap();
        shadow
            .apply_batch(vec![Change::RemoveFile {
                path: "/disk/a/f".into(),
                identity: identity(3),
            }])
            .unwrap();
        assert!(shadow.file(id).is_none());
        assert!(scene.file(id).is_some());
        assert_eq!(scene.project(View::Files).len(), 3);
        assert_eq!(shadow.project(View::Files).len(), 2);
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
    fn leaf_removal_checks_identity_and_parent_before_mutating() {
        let mut scene = Scene::from_scan(Arc::new(fixture()));
        let action = |id, parent_id| Change::RemoveLeafDirectory {
            path: "/disk/a".into(),
            identity: identity(id),
            parent: dir("/disk", parent_id, true, true),
        };
        assert!(matches!(
            scene.apply_batch(vec![action(99, 1)]),
            Err(ChangeError::IdentityMismatch)
        ));
        assert!(matches!(
            scene.apply_batch(vec![action(2, 99)]),
            Err(ChangeError::IdentityMismatch)
        ));
        assert!(matches!(
            scene.apply_batch(vec![Change::RemoveLeafDirectory {
                path: "/disk/a".into(),
                identity: identity(2),
                parent: dir("/disk", 1, true, false),
            }]),
            Err(ChangeError::ConflictingBatch)
        ));
        assert_eq!(scene.project(View::Files).len(), 3);
        scene.apply_batch(vec![action(2, 1)]).unwrap();
        assert_eq!(scene.project(View::Files).len(), 1);
        assert!(scene.directory_id(Path::new("/disk/a")).is_none());
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
