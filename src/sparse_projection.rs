//! Stable display slots over an immutable scan projection.
//!
//! The base and its lookup index are shared by versions. Changes touch only
//! sparse overrides; old versions remain usable by an already published image.
use crate::{scan, scene_model::TileId};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Debug, PartialEq, Eq)]
pub enum ProjectionError {
    LengthMismatch,
    DuplicateId,
    MissingId,
    StaleId,
    IdentityMismatch,
    CapacityExceeded,
}

#[derive(Clone)]
pub struct SparseProjection {
    base: Arc<scan::ScanResult>,
    base_ids: Arc<[Option<TileId>]>,
    base_index: Arc<HashMap<TileId, usize>>,
    // An override of None is a tombstone. Slots are never recycled within a
    // projection generation while an earlier texture can still reference them.
    overrides: HashMap<usize, Option<scan::FileEntry>>,
    extra_ids: Vec<TileId>,
    extra_index: HashMap<TileId, usize>,
    // A directory may stop being a tile when hybrid view expands, then
    // become a tile again when it folds. Retain its identity separately so a
    // later reappearance gets a fresh slot, never the published tombstone.
    retired_directories: HashMap<TileId, (std::path::PathBuf, scan::FileIdentity)>,
    // A hybrid projection can hide a still-live file inside a folder tile.
    // Only actor-validated deltas may reintroduce it, in a fresh display slot.
    retired_files: [Arc<HashMap<TileId, scan::FileIdentity>>; 64],
    live_count: usize,
}

impl SparseProjection {
    #[cfg(test)]
    pub fn new(base: Arc<scan::ScanResult>, ids: Vec<TileId>) -> Result<Self, ProjectionError> {
        Self::new_optional(base, ids.into_iter().map(Some).collect())
    }
    /// Preserve original texture slots when some source entries are already
    /// hidden. A None is a permanent tombstone, never a reusable slot.
    pub fn new_optional(
        base: Arc<scan::ScanResult>,
        ids: Vec<Option<TileId>>,
    ) -> Result<Self, ProjectionError> {
        if ids.len() != base.files.len() {
            return Err(ProjectionError::LengthMismatch);
        }
        if ids.len() > u32::MAX as usize {
            return Err(ProjectionError::CapacityExceeded);
        }
        let mut index = HashMap::with_capacity(ids.len());
        for (slot, id) in ids.iter().enumerate() {
            if let Some(id) = id
                && index.insert(*id, slot).is_some()
            {
                return Err(ProjectionError::DuplicateId);
            }
        }
        let live_count = index.len();
        Ok(Self {
            base,
            base_ids: ids.into(),
            base_index: Arc::new(index),
            overrides: HashMap::new(),
            extra_ids: Vec::new(),
            extra_index: HashMap::new(),
            retired_directories: HashMap::new(),
            retired_files: std::array::from_fn(|_| Arc::new(HashMap::new())),
            live_count,
        })
    }
    pub fn base(&self) -> &Arc<scan::ScanResult> {
        &self.base
    }
    fn retired_file_shard(id: TileId) -> usize {
        match id {
            TileId::File(entry) => entry.slot as usize % 64,
            TileId::Directory(_) => 0,
        }
    }
    /// The actor has confirmed these generational IDs no longer exist in the
    /// scene. Their old display slots remain tombstones, but their identities
    /// are no longer needed for a later hybrid projection expansion.
    pub(crate) fn forget_deleted_files(&mut self, ids: &[TileId]) {
        for &id in ids {
            if !matches!(id, TileId::File(_)) {
                continue;
            }
            let shard = &mut self.retired_files[Self::retired_file_shard(id)];
            if shard.contains_key(&id) {
                Arc::make_mut(shard).remove(&id);
            }
        }
    }
    pub fn len(&self) -> usize {
        self.base_ids.len() + self.extra_ids.len()
    }
    pub fn is_empty(&self) -> bool {
        self.live_count == 0
    }
    pub fn id(&self, slot: usize) -> Option<TileId> {
        if self.get(slot).is_none() {
            return None;
        }
        self.raw_id(slot)
    }
    fn raw_id(&self, slot: usize) -> Option<TileId> {
        if slot < self.base_ids.len() {
            self.base_ids.get(slot).copied().flatten()
        } else {
            self.extra_ids.get(slot - self.base_ids.len()).copied()
        }
    }
    fn raw_slot(&self, id: TileId) -> Option<usize> {
        self.extra_index
            .get(&id)
            .or_else(|| self.base_index.get(&id))
            .copied()
    }
    pub fn slot(&self, id: TileId) -> Option<usize> {
        let slot = self.raw_slot(id)?;
        self.get(slot).map(|_| slot)
    }
    pub fn get(&self, slot: usize) -> Option<&scan::FileEntry> {
        self.raw_id(slot)?;
        if let Some(value) = self.overrides.get(&slot) {
            return value.as_ref();
        }
        self.base.files.get(slot)
    }
    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = (usize, TileId, &scan::FileEntry)> {
        (0..self.len()).filter_map(|slot| Some((slot, self.id(slot)?, self.get(slot)?)))
    }
    /// Replace the tiles of affected direct directories, using identities from
    /// the scene before and after its atomic batch. Both lists may include
    /// unchanged tiles; IDs retained in `after` retain their published slots.
    /// Validation happens before any mutation, including additions.
    pub fn replace(
        &mut self,
        before: &[TileId],
        after: Vec<(TileId, scan::FileEntry)>,
    ) -> Result<(), ProjectionError> {
        self.replace_inner(before, after, false)
    }

    /// Apply a delta produced by the version-checked Scene actor. A file may
    /// have disappeared only from this mode's projection while the underlying
    /// entry remained alive; its generational ID and filesystem identity must
    /// both still match. Ordinary callers must use `replace` instead.
    pub(crate) fn replace_from_scene(
        &mut self,
        before: &[TileId],
        after: Vec<(TileId, scan::FileEntry)>,
    ) -> Result<(), ProjectionError> {
        self.replace_inner(before, after, true)
    }

    fn replace_inner(
        &mut self,
        before: &[TileId],
        after: Vec<(TileId, scan::FileEntry)>,
        scene_validated: bool,
    ) -> Result<(), ProjectionError> {
        let mut old = HashSet::with_capacity(before.len());
        for &id in before {
            if !old.insert(id) {
                return Err(ProjectionError::DuplicateId);
            }
            if self.slot(id).is_none() {
                return Err(ProjectionError::MissingId);
            }
        }
        let mut new = HashSet::with_capacity(after.len());
        let mut additional = 0usize;
        for (id, entry) in &after {
            if !new.insert(*id) {
                return Err(ProjectionError::DuplicateId);
            }
            if let Some(slot) = self.raw_slot(*id) {
                let Some(existing) = self.get(slot) else {
                    if matches!(id, TileId::Directory(_))
                        && self
                            .retired_directories
                            .get(id)
                            .is_some_and(|(path, identity)| {
                                *path == entry.path && *identity == entry.identity
                            })
                    {
                        additional += 1;
                        continue;
                    }
                    if scene_validated
                        && matches!(id, TileId::File(_))
                        && self.retired_files[Self::retired_file_shard(*id)]
                            .get(id)
                            .is_some_and(|identity| *identity == entry.identity)
                    {
                        additional += 1;
                        continue;
                    }
                    return Err(ProjectionError::StaleId);
                };
                if !old.contains(id) {
                    return Err(ProjectionError::DuplicateId);
                }
                if existing.identity != entry.identity {
                    return Err(ProjectionError::IdentityMismatch);
                }
            } else {
                additional += 1;
            }
        }
        if self
            .len()
            .checked_add(additional)
            .is_none_or(|len| len > u32::MAX as usize)
        {
            return Err(ProjectionError::CapacityExceeded);
        }
        for id in old.difference(&new) {
            let slot = self.slot(*id).expect("validated live id");
            if matches!(id, TileId::Directory(_)) {
                let entry = self.get(slot).expect("validated live entry");
                self.retired_directories
                    .insert(*id, (entry.path.clone(), entry.identity));
            } else {
                let identity = self.get(slot).expect("validated live entry").identity;
                Arc::make_mut(&mut self.retired_files[Self::retired_file_shard(*id)])
                    .insert(*id, identity);
            }
            self.overrides.insert(slot, None);
            self.live_count -= 1;
        }
        for (id, entry) in after {
            let slot = match self.raw_slot(id).filter(|slot| self.get(*slot).is_some()) {
                Some(slot) => slot,
                None => {
                    let slot = self.len();
                    self.extra_ids.push(id);
                    self.extra_index.insert(id, slot);
                    self.retired_directories.remove(&id);
                    let shard = &mut self.retired_files[Self::retired_file_shard(id)];
                    if shard.contains_key(&id) {
                        Arc::make_mut(shard).remove(&id);
                    }
                    self.live_count += 1;
                    slot
                }
            };
            self.overrides.insert(slot, Some(entry));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_model::{DirectoryId, EntryId, TileId};
    use std::{path::PathBuf, time::Duration};
    fn id(slot: u32, generation: u32) -> TileId {
        TileId::File(EntryId { slot, generation })
    }
    fn file(name: &str, size: u64) -> scan::FileEntry {
        scan::FileEntry {
            path: PathBuf::from(format!("/disk/{name}")),
            size,
            parent: Arc::from(std::path::Path::new("/disk")),
            identity: scan::FileIdentity::default(),
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: true,
            can_trash: true,
        }
    }
    fn projection() -> SparseProjection {
        let base = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("a", 1), file("b", 2)],
            directories: Vec::new().into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        });
        SparseProjection::new(base, vec![id(0, 0), id(1, 0)]).unwrap()
    }
    #[test]
    fn preserves_slots_old_snapshot_and_unchanged_values() {
        let mut next = projection();
        let old = next.clone();
        next.replace(&[id(0, 0)], vec![(id(2, 0), file("c", 3))])
            .unwrap();
        assert_eq!(old.get(0).unwrap().path, PathBuf::from("/disk/a"));
        assert!(next.get(0).is_none());
        assert_eq!(next.slot(id(1, 0)), Some(1));
        assert_eq!(next.slot(id(2, 0)), Some(2));
        assert_eq!(
            next.iter().map(|(_, _, f)| f.size).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
    #[test]
    fn update_is_atomic_and_removed_generation_cannot_reappear() {
        let mut p = projection();
        assert_eq!(
            p.replace(&[id(0, 0), id(9, 0)], vec![]),
            Err(ProjectionError::MissingId)
        );
        assert_eq!(p.get(0).unwrap().size, 1);
        p.replace(&[id(0, 0)], vec![]).unwrap();
        assert_eq!(
            p.replace(&[], vec![(id(0, 0), file("replacement", 9))]),
            Err(ProjectionError::StaleId)
        );
        p.replace(&[], vec![(id(0, 1), file("replacement", 9))])
            .unwrap();
        assert_eq!(p.slot(id(0, 1)), Some(2));
    }
    #[test]
    fn same_id_update_preserves_slot() {
        let mut p = projection();
        p.replace(&[id(0, 0)], vec![(id(0, 0), file("a", 10))])
            .unwrap();
        assert_eq!(p.slot(id(0, 0)), Some(0));
        assert_eq!(p.get(0).unwrap().size, 10);
    }
    #[test]
    fn scene_validated_hidden_file_reappears_in_fresh_slot_with_new_path() {
        let mut p = projection();
        let old = p.clone();
        p.replace(&[id(0, 0)], vec![]).unwrap();
        let hidden = p.clone();
        let moved = file("elsewhere", 7);
        assert_eq!(
            p.replace(&[], vec![(id(0, 0), moved.clone())]),
            Err(ProjectionError::StaleId)
        );
        p.replace_from_scene(&[], vec![(id(0, 0), moved)]).unwrap();
        assert_eq!(p.slot(id(0, 0)), Some(2));
        assert!(p.get(0).is_none());
        assert!(hidden.get(0).is_none());
        assert_eq!(old.get(0).unwrap().path, PathBuf::from("/disk/a"));
    }
    #[test]
    fn scene_validated_reappearance_rejects_identity_replacement_and_old_slots_stay_dead() {
        let mut p = projection();
        p.replace(&[id(0, 0)], vec![]).unwrap();
        let mut replacement = file("a", 7);
        replacement.identity.device = 999;
        assert_eq!(
            p.replace_from_scene(&[], vec![(id(0, 0), replacement.clone())]),
            Err(ProjectionError::StaleId)
        );
        p.replace_from_scene(&[], vec![(id(0, 1), replacement)])
            .unwrap();
        assert!(p.get(0).is_none());
        assert!(p.slot(id(0, 0)).is_none());
        assert_eq!(p.slot(id(0, 1)), Some(2));
    }
    #[test]
    fn confirmed_deletion_reclaims_retired_identity_and_does_not_revive_old_slot() {
        let mut p = projection();
        p.replace(&[id(0, 0)], vec![]).unwrap();
        let hidden_snapshot = p.clone();
        assert_eq!(p.retired_files.iter().map(|s| s.len()).sum::<usize>(), 1);
        p.forget_deleted_files(&[id(0, 0)]);
        assert_eq!(p.retired_files.iter().map(|s| s.len()).sum::<usize>(), 0);
        assert_eq!(
            hidden_snapshot
                .retired_files
                .iter()
                .map(|s| s.len())
                .sum::<usize>(),
            1
        );
        assert_eq!(
            p.replace_from_scene(&[], vec![(id(0, 0), file("a", 1))]),
            Err(ProjectionError::StaleId)
        );
        assert!(p.get(0).is_none());
        p.replace_from_scene(&[], vec![(id(0, 1), file("a", 1))])
            .unwrap();
        assert_eq!(p.slot(id(0, 1)), Some(2));
        assert!(p.get(0).is_none());
    }
    #[test]
    fn folded_directory_reappears_in_a_new_slot_without_reviving_old_frames() {
        let directory = TileId::Directory(DirectoryId {
            slot: 4,
            generation: 1,
        });
        let base = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("a", 1)],
            directories: Vec::new().into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        });
        let mut projection = SparseProjection::new(base, vec![directory]).unwrap();
        let old = projection.clone();
        projection.replace(&[directory], vec![]).unwrap();
        let unfolded = projection.clone();
        assert_eq!(
            projection.replace(&[], vec![(directory, file("other", 2))]),
            Err(ProjectionError::StaleId)
        );
        projection
            .replace(&[], vec![(directory, file("a", 2))])
            .unwrap();
        assert_eq!(projection.slot(directory), Some(1));
        assert!(projection.get(0).is_none());
        assert!(unfolded.get(0).is_none());
        assert_eq!(old.get(0).unwrap().size, 1);
        assert_eq!(projection.get(1).unwrap().size, 2);
        projection.replace(&[directory], vec![]).unwrap();
        projection
            .replace(&[], vec![(directory, file("a", 3))])
            .unwrap();
        assert_eq!(projection.slot(directory), Some(2));
        assert!(projection.get(1).is_none());
    }
    #[test]
    fn initial_tombstones_preserve_slots_and_never_resurrect_base() {
        let initial = projection();
        let mut p =
            SparseProjection::new_optional(initial.base().clone(), vec![None, Some(id(1, 0))])
                .unwrap();
        assert_eq!(p.len(), 2);
        assert!(!p.is_empty());
        assert!(p.get(0).is_none());
        assert!(p.id(0).is_none());
        assert_eq!(p.slot(id(1, 0)), Some(1));
        p.replace(&[id(1, 0)], vec![]).unwrap();
        assert!(p.is_empty());
        p.replace(&[], vec![(id(0, 1), file("c", 3))]).unwrap();
        assert_eq!(p.slot(id(0, 1)), Some(2));
        assert!(!p.is_empty());
        assert!(p.get(0).is_none());
    }
}
