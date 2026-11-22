//! Transactional local layout updates keyed by stable scene tile identity.
//!
//! This layer owns no pixels. A worker prepares a candidate layout and dirty
//! rectangles; its caller must rasterize and publish them with the matching
//! scene version. If a structural change would move a nested directory, or the
//! requested area error is exceeded, the caller must rebuild the full scene.
use crate::{
    directory_map::{DirectoryPartition, PartitionItem, PartitionItemId},
    incremental_layout::{self, Partition, Placement},
    scan,
    scene_model::{Scene, TileId},
    treemap::Rect,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Item {
    // Intern directory paths once per scene, rather than embedding a PathBuf
    // in every file key in a potentially multimillion-item BTreeMap.
    Directory(u32),
    Tile(TileId),
}

/// Before/after is expressed in the layout parent coordinate system. Folder
/// projection tiles belong to their folder's *parent*, as in directory_map.
#[derive(Clone, Debug)]
pub struct TileState {
    pub id: TileId,
    pub parent: PathBuf,
    pub weight: u64,
}

#[derive(Clone, Debug)]
pub struct TileDelta {
    pub before: Option<TileState>,
    pub after: Option<TileState>,
}

#[derive(Debug)]
pub enum Error {
    InvalidInitialLayout,
    MissingPartition,
    InvalidDelta,
    Cancelled,
    /// A nested directory would move, or error exceeds the supplied budget.
    Rebuild,
}

/// Identifies the kind of each original render slot. Hybrid's source map has
/// one element per projected item; `None` denotes an aggregated directory.
pub enum Projection<'a> {
    Files,
    Folders,
    Hybrid(&'a [Option<usize>]),
}

/// Map the scene IDs to the original projection's rendering slots. Reject
/// changed identities and duplicate slots rather than pair an old owner pixel
/// with a different filesystem object.
pub fn projection_slots(
    scene: &Scene,
    data: &scan::ScanResult,
    projection: Projection<'_>,
) -> Result<Vec<TileId>, Error> {
    if let Projection::Hybrid(indices) = &projection {
        if indices.len() != data.files.len() {
            return Err(Error::InvalidInitialLayout);
        }
    }
    let mut seen = HashSet::with_capacity(data.files.len());
    let mut result = Vec::with_capacity(data.files.len());
    for (index, file) in data.files.iter().enumerate() {
        let directory = match &projection {
            Projection::Files => false,
            Projection::Folders => true,
            Projection::Hybrid(indices) => indices[index].is_none(),
        };
        let id = if directory {
            let id = scene
                .directory_id(&file.path)
                .ok_or(Error::InvalidInitialLayout)?;
            if scene.directory_identity(id) != Some(file.identity) {
                return Err(Error::InvalidInitialLayout);
            }
            TileId::Directory(id)
        } else {
            let id = scene
                .file_id(&file.path)
                .ok_or(Error::InvalidInitialLayout)?;
            let source = scene.file(id).ok_or(Error::InvalidInitialLayout)?;
            if source.identity != file.identity || source.path != file.path {
                return Err(Error::InvalidInitialLayout);
            }
            TileId::File(id)
        };
        if !seen.insert(id) {
            return Err(Error::InvalidInitialLayout);
        }
        result.push(id);
    }
    Ok(result)
}

#[derive(Clone)]
pub struct IncrementalGeometry {
    partitions: BTreeMap<Option<PathBuf>, Arc<Partition<Item>>>,
    /// Directory child -> actual exported parent, including synthetic root.
    parents: Arc<BTreeMap<PathBuf, (Option<PathBuf>, u32)>>,
}

pub struct GeometryUpdate {
    pub dirty: Vec<Rect>,
    /// Geometry changed for these tiles; removed tiles are None.
    pub tiles: BTreeMap<TileId, Option<Rect>>,
}

impl IncrementalGeometry {
    /// Export the currently published allocations, rather than the original
    /// scan's allocations, so a structural update can keep unrelated regions.
    pub fn export_partitions(
        &self,
        projection: &crate::sparse_projection::SparseProjection,
        platforms: &[crate::render::Platform],
    ) -> Result<Vec<DirectoryPartition>, Error> {
        let by_id: BTreeMap<_, _> = self
            .parents
            .iter()
            .map(|(path, (_, id))| (*id, path))
            .collect();
        let rect_key = |r: Rect| [r.x.to_bits(), r.y.to_bits(), r.w.to_bits(), r.h.to_bits()];
        let mut surfaces = BTreeMap::<[u32; 4], Option<PathBuf>>::new();
        for (path, partition) in &self.partitions {
            let key = rect_key(partition.bounds());
            let depth = |p: &Option<PathBuf>| p.as_ref().map_or(0, |p| p.components().count());
            match surfaces.get_mut(&key) {
                Some(existing) if depth(path) < depth(existing) => *existing = path.clone(),
                None => {
                    surfaces.insert(key, path.clone());
                }
                _ => {}
            }
        }
        let mut owner = BTreeMap::<Option<PathBuf>, crate::render::Platform>::new();
        for platform in platforms {
            let matched = surfaces
                .get(&rect_key(platform.top))
                .ok_or(Error::Rebuild)?;
            if owner.insert(matched.clone(), *platform).is_some() {
                return Err(Error::Rebuild);
            }
        }
        let mut out = Vec::with_capacity(self.partitions.len());
        for (path, partition) in &self.partitions {
            let platform = owner.get(path).copied();
            let mut items = Vec::new();
            for (id, placement) in partition.entries() {
                let id = match id {
                    Item::Directory(id) => {
                        PartitionItemId::Directory((*by_id.get(id).ok_or(Error::Rebuild)?).clone())
                    }
                    Item::Tile(id) => {
                        PartitionItemId::File(projection.slot(*id).ok_or(Error::Rebuild)?)
                    }
                };
                items.push(PartitionItem {
                    id,
                    rect: placement.rect,
                    weight: placement.weight,
                });
            }
            out.push(DirectoryPartition {
                path: path.clone(),
                bounds: partition.bounds(),
                platform,
                items,
            });
        }
        Ok(out)
    }

    /// A fixed outer allocation can be retained only while its ancestors'
    /// discrepancy from their new weights remains within the pixel budget.
    pub fn ancestor_area_error(&self, path: &std::path::Path, pixels_per_point: f32) -> f64 {
        self.partitions
            .iter()
            .filter(|(key, _)| key.as_ref().is_none_or(|p| path.starts_with(p)))
            .map(|(_, p)| p.max_area_error(pixels_per_point))
            .fold(0.0, f64::max)
    }

    /// Leaf folder deletion is local only when its parent platform remains
    /// present and at least two independently drawn tiles share the parent.
    /// Nested directory allocations remain fixed during adjacency compaction.
    pub fn can_compact_leaf_parent(&self, path: &std::path::Path) -> bool {
        self.partitions
            .get(&Some(path.to_path_buf()))
            .is_some_and(|partition| {
                partition
                    .entries()
                    .filter(|(item, _)| matches!(item, Item::Tile(_)))
                    .take(2)
                    .count()
                    == 2
            })
    }
    /// Translate original render slots to stable typed tile handles once.
    pub fn from_layout(parts: &[DirectoryPartition], slots: &[TileId]) -> Result<Self, Error> {
        Self::from_layout_mapped(
            parts,
            |index| slots.get(index).copied().map(|id| Some((id, None))),
            false,
        )
    }

    /// Recover from a previously published map, keeping tombstoned slots as
    /// reusable holes. The slot/owner mapping must already be validated by the
    /// coordinator before constructing this scene.
    pub fn from_layout_optional(
        parts: &[DirectoryPartition],
        slots: &[Option<TileId>],
    ) -> Result<Self, Error> {
        Self::from_layout_mapped(
            parts,
            |index| slots.get(index).copied().map(|id| id.map(|id| (id, None))),
            true,
        )
    }

    /// Restore old geometry against the currently published projection sizes.
    /// Geometry stays fixed; retained tiles take their current weights and
    /// tombstones remain reusable holes.
    pub fn from_layout_reconciled(
        parts: &[DirectoryPartition],
        slots: &[Option<TileId>],
        weights: &[u64],
    ) -> Result<Self, Error> {
        if slots.len() != weights.len() {
            return Err(Error::InvalidInitialLayout);
        }
        Self::from_layout_mapped(
            parts,
            |index| {
                slots
                    .get(index)
                    .copied()
                    .map(|id| id.map(|id| (id, Some(weights[index]))))
            },
            true,
        )
    }

    fn from_layout_mapped(
        parts: &[DirectoryPartition],
        mut lookup: impl FnMut(usize) -> Option<Option<(TileId, Option<u64>)>>,
        restore_weights: bool,
    ) -> Result<Self, Error> {
        let mut partitions = BTreeMap::new();
        let mut parents = BTreeMap::new();
        for part in parts {
            let mut items = Vec::with_capacity(part.items.len());
            let mut vacant = Vec::new();
            for entry in &part.items {
                let (id, value) = match &entry.id {
                    PartitionItemId::Directory(path) => {
                        let index = u32::try_from(parents.len())
                            .map_err(|_| Error::InvalidInitialLayout)?;
                        if parents
                            .insert(path.clone(), (part.path.clone(), index))
                            .is_some()
                        {
                            return Err(Error::InvalidInitialLayout);
                        }
                        (Item::Directory(index), entry.weight)
                    }
                    PartitionItemId::File(slot) => {
                        match lookup(*slot).ok_or(Error::InvalidInitialLayout)? {
                            Some((id, value)) => (Item::Tile(id), value.unwrap_or(entry.weight)),
                            None => {
                                vacant.push(entry.rect);
                                continue;
                            }
                        }
                    }
                };
                items.push((
                    id,
                    Placement {
                        rect: entry.rect,
                        weight: value,
                    },
                ));
            }
            let mut partition = Partition::from_layout(part.bounds, items)
                .map_err(|_| Error::InvalidInitialLayout)?;
            for rect in vacant {
                partition
                    .restore_vacancy(rect)
                    .map_err(|_| Error::InvalidInitialLayout)?;
            }
            if partitions.insert(part.path.clone(), partition).is_some() {
                return Err(Error::InvalidInitialLayout);
            }
        }
        if restore_weights {
            // Removed slots change direct weights. Propagate live child totals
            // bottom-up so no later delta observes the old snapshot's totals.
            let mut paths: Vec<_> = parents.keys().cloned().collect();
            paths.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
            for path in paths {
                let child = partitions
                    .get(&Some(path.clone()))
                    .ok_or(Error::InvalidInitialLayout)?;
                let total = child.entries().fold(0u64, |sum, (_, entry)| {
                    sum.saturating_add(entry.weight.max(1))
                });
                let (parent, id) = &parents[&path];
                partitions
                    .get_mut(parent)
                    .ok_or(Error::InvalidInitialLayout)?
                    .set_weight(&Item::Directory(*id), total)
                    .map_err(|_| Error::InvalidInitialLayout)?;
            }
        }
        Ok(Self {
            partitions: partitions
                .into_iter()
                .map(|(k, v)| (k, Arc::new(v)))
                .collect(),
            parents: Arc::new(parents),
        })
    }

    /// Reconcile a retained published geometry with a recovered projection.
    /// Only deletions and weight changes in the same stable slot are safe;
    /// additions, replacement, relocation and changed identity require rebuild.
    /// All mutations are staged so an error leaves the original intact.
    pub fn reconcile_retained(
        &mut self,
        old: &crate::sparse_projection::SparseProjection,
        new: &crate::sparse_projection::SparseProjection,
    ) -> Result<(), Error> {
        if old.len() != new.len() {
            return Err(Error::Rebuild);
        }
        let mut staged: BTreeMap<Option<PathBuf>, Partition<Item>> = BTreeMap::new();
        let mut pending = BTreeSet::new();
        for index in 0..old.len() {
            match (old.id(index), new.id(index)) {
                (None, None) => continue,
                (Some(id), next_id) => {
                    if next_id.is_some_and(|next| next != id) {
                        return Err(Error::Rebuild);
                    }
                    let previous = old.get(index).ok_or(Error::Rebuild)?;
                    let current = new.get(index);
                    if let Some(entry) = current {
                        if next_id != Some(id)
                            || previous.path != entry.path
                            || previous.parent != entry.parent
                            || previous.identity != entry.identity
                        {
                            return Err(Error::Rebuild);
                        }
                        if previous.size == entry.size {
                            continue;
                        }
                    } else if next_id.is_some() {
                        return Err(Error::Rebuild);
                    }
                    let key = Some(previous.parent.to_path_buf());
                    let base = self.partitions.get(&key).ok_or(Error::Rebuild)?;
                    let partition = staged
                        .entry(key.clone())
                        .or_insert_with(|| base.as_ref().clone());
                    if partition
                        .get(&Item::Tile(id))
                        .is_none_or(|p| p.weight.max(1) != previous.size.max(1))
                    {
                        return Err(Error::Rebuild);
                    }
                    if let Some(entry) = current {
                        partition
                            .set_weight(&Item::Tile(id), entry.size)
                            .map_err(|_| Error::Rebuild)?;
                    } else {
                        partition
                            .remove(&Item::Tile(id))
                            .map_err(|_| Error::Rebuild)?;
                    }
                    pending.insert(previous.parent.to_path_buf());
                }
                (None, Some(_)) => return Err(Error::Rebuild),
            }
        }
        while let Some(path) = pending
            .iter()
            .max_by_key(|p| p.components().count())
            .cloned()
        {
            pending.remove(&path);
            let key = Some(path.clone());
            let child = staged
                .get(&key)
                .or_else(|| self.partitions.get(&key).map(Arc::as_ref))
                .ok_or(Error::Rebuild)?;
            let total = child
                .entries()
                .fold(0u64, |sum, (_, p)| sum.saturating_add(p.weight.max(1)));
            if let Some((parent_key, child_id)) = self.parents.get(&path) {
                let base = self.partitions.get(parent_key).ok_or(Error::Rebuild)?;
                let parent = staged
                    .entry(parent_key.clone())
                    .or_insert_with(|| base.as_ref().clone());
                parent
                    .set_weight(&Item::Directory(*child_id), total)
                    .map_err(|_| Error::Rebuild)?;
                if let Some(parent_path) = parent_key {
                    pending.insert(parent_path.clone());
                }
            }
        }
        self.partitions
            .extend(staged.into_iter().map(|(k, p)| (k, Arc::new(p))));
        Ok(())
    }

    /// Run off-thread on a private scene version. Only partitions in affected
    /// branches are cloned; an error leaves the original state untouched.
    pub fn apply(
        &mut self,
        changes: &[TileDelta],
        pixels_per_point: f32,
        tolerance_pixels: f64,
        token: &AtomicU64,
        generation: u64,
    ) -> Result<GeometryUpdate, Error> {
        self.apply_inner(
            changes,
            pixels_per_point,
            tolerance_pixels,
            token,
            generation,
            false,
        )
    }

    pub fn apply_compacting(
        &mut self,
        changes: &[TileDelta],
        pixels_per_point: f32,
        tolerance_pixels: f64,
        token: &AtomicU64,
        generation: u64,
    ) -> Result<GeometryUpdate, Error> {
        let result = self.apply_inner(
            changes,
            pixels_per_point,
            tolerance_pixels,
            token,
            generation,
            true,
        );
        // Compaction is optional: retain the old hole behavior if a nested
        // platform or its ancestor would otherwise require a whole rebuild.
        if matches!(result, Err(Error::Rebuild)) {
            self.apply_inner(
                changes,
                pixels_per_point,
                tolerance_pixels,
                token,
                generation,
                false,
            )
        } else {
            result
        }
    }

    fn apply_inner(
        &mut self,
        changes: &[TileDelta],
        pixels_per_point: f32,
        tolerance_pixels: f64,
        token: &AtomicU64,
        generation: u64,
        compact: bool,
    ) -> Result<GeometryUpdate, Error> {
        if token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        let mut staged: BTreeMap<Option<PathBuf>, Partition<Item>> = BTreeMap::new();
        let mut touched = BTreeSet::new();
        let mut result = GeometryUpdate {
            dirty: Vec::new(),
            tiles: BTreeMap::new(),
        };
        let deletion_batch = compact
            && changes.iter().all(|c| match (&c.before, &c.after) {
                (Some(_), None) => true,
                (Some(old), Some(new)) => {
                    old.id == new.id && old.parent == new.parent && new.weight <= old.weight
                }
                _ => false,
            });
        for change in changes {
            if change.before.is_none() && change.after.is_none() {
                return Err(Error::InvalidDelta);
            }
            if let (Some(old), Some(new)) = (&change.before, &change.after) {
                if old.id != new.id {
                    return Err(Error::InvalidDelta);
                }
                // Updating a folder aggregate during a deletion must not
                // temporarily insert it into its own vacated slot.
                if deletion_batch && old.parent == new.parent {
                    let key = Some(old.parent.clone());
                    if !self.partitions.contains_key(&key) {
                        return Err(Error::MissingPartition);
                    }
                    let p = staged
                        .entry(key.clone())
                        .or_insert_with(|| self.partitions[&key].as_ref().clone());
                    if p.get(&Item::Tile(old.id))
                        .is_none_or(|entry| entry.weight.max(1) != old.weight.max(1))
                    {
                        return Err(Error::InvalidDelta);
                    }
                    p.set_weight(&Item::Tile(old.id), new.weight)
                        .map_err(|_| Error::InvalidDelta)?;
                    touched.insert(key);
                    continue;
                }
            }
            // Removing and re-adding ensures a size change can use its old slot
            // only when it fits the supplied area error budget.
            if let Some(old) = &change.before {
                let key = Some(old.parent.clone());
                if !self.partitions.contains_key(&key) {
                    return Err(Error::MissingPartition);
                }
                let p = staged
                    .entry(key.clone())
                    .or_insert_with(|| self.partitions[&key].as_ref().clone());
                if p.get(&Item::Tile(old.id))
                    .is_none_or(|entry| entry.weight.max(1) != old.weight.max(1))
                {
                    return Err(Error::InvalidDelta);
                }
                let removed = p
                    .remove(&Item::Tile(old.id))
                    .map_err(|_| Error::InvalidDelta)?;
                result.dirty.push(removed.dirty);
                result.tiles.insert(old.id, None);
                touched.insert(key);
            }
            if let Some(new) = &change.after {
                let key = Some(new.parent.clone());
                if !self.partitions.contains_key(&key) {
                    return Err(Error::MissingPartition);
                }
                let p = staged
                    .entry(key.clone())
                    .or_insert_with(|| self.partitions[&key].as_ref().clone());
                let update = p
                    .insert(
                        Item::Tile(new.id),
                        new.weight,
                        pixels_per_point,
                        tolerance_pixels,
                        token,
                        generation,
                    )
                    .map_err(|e| {
                        if e == incremental_layout::Error::Cancelled {
                            Error::Cancelled
                        } else {
                            Error::InvalidDelta
                        }
                    })?;
                result.dirty.push(update.dirty);
                for (item, rect) in update.moved {
                    match item {
                        Item::Tile(id) => {
                            result.tiles.insert(id, Some(rect));
                        }
                        // Moving nested content requires rebuilding its platform
                        // and descendants, not just translating a file rect.
                        Item::Directory(_) => return Err(Error::Rebuild),
                    }
                }
                touched.insert(key);
            }
        }
        // A removal leaves a vacant rectangle. Compact the affected partition
        // in the background so its surviving tiles can transition into place.
        // A nested directory owns its own platform and descendants; moving it
        // requires a structural relayout, so retain the vacancy in that case.
        if deletion_batch {
            for key in &touched {
                let p = staged.get_mut(key).ok_or(Error::MissingPartition)?;
                if p.entries()
                    .any(|(item, _)| matches!(item, Item::Directory(_)))
                {
                    let updates = p
                        .compact_adjacent(|item| matches!(item, Item::Tile(_)), token, generation)
                        .map_err(|e| {
                            if e == incremental_layout::Error::Cancelled {
                                Error::Cancelled
                            } else {
                                Error::InvalidDelta
                            }
                        })?;
                    for update in updates {
                        result.dirty.push(update.dirty);
                        for (item, rect) in update.moved {
                            if let Item::Tile(id) = item {
                                result.tiles.insert(id, Some(rect));
                            }
                        }
                    }
                    continue;
                }
                let bounds = p.bounds();
                let update = p.repack(bounds, token, generation).map_err(|e| {
                    if e == incremental_layout::Error::Cancelled {
                        Error::Cancelled
                    } else {
                        Error::InvalidDelta
                    }
                })?;
                result.dirty.push(update.dirty);
                for (item, rect) in update.moved {
                    if let Item::Tile(id) = item {
                        result.tiles.insert(id, Some(rect));
                    }
                }
            }
        }
        // Update child aggregate weights up the exported directory hierarchy.
        // Process deepest first so a common ancestor observes all changes.
        let mut paths: Vec<_> = touched.iter().filter_map(Clone::clone).collect();
        paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        let mut pending: BTreeSet<PathBuf> = paths.into_iter().collect();
        while let Some(path) = pending
            .iter()
            .max_by_key(|p| p.components().count())
            .cloned()
        {
            pending.remove(&path);
            let key = Some(path.clone());
            let p = staged
                .get(&key)
                .or_else(|| self.partitions.get(&key).map(Arc::as_ref))
                .ok_or(Error::MissingPartition)?;
            if p.max_area_error(pixels_per_point) > tolerance_pixels.max(0.0) {
                return Err(Error::Rebuild);
            }
            let total = p.entries().fold(0u64, |sum, (_, placement)| {
                sum.saturating_add(placement.weight.max(1))
            });
            if let Some((parent_key, child_id)) = self.parents.get(&path) {
                let parent = staged
                    .entry(parent_key.clone())
                    .or_insert_with(|| self.partitions[parent_key].as_ref().clone());
                parent
                    .set_weight(&Item::Directory(*child_id), total)
                    .map_err(|_| Error::InvalidDelta)?;
                if let Some(parent_path) = parent_key {
                    pending.insert(parent_path.clone());
                } else if parent.max_area_error(pixels_per_point) > tolerance_pixels.max(0.0) {
                    return Err(Error::Rebuild);
                }
            }
        }
        if token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        self.partitions
            .extend(staged.into_iter().map(|(k, p)| (k, Arc::new(p))));
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{directory_map::PartitionItem, scene_model::EntryId};
    use std::{sync::Arc, time::Duration};

    fn tile(slot: u32) -> TileId {
        TileId::File(EntryId {
            slot,
            generation: 0,
        })
    }
    fn rect(x: f32, width: f32) -> Rect {
        Rect {
            x,
            y: 0.0,
            w: width,
            h: 100.0,
        }
    }
    fn state(id: u32, weight: u64) -> TileState {
        TileState {
            id: tile(id),
            parent: "/disk/a".into(),
            weight,
        }
    }
    fn geometry() -> IncrementalGeometry {
        let parts = vec![
            DirectoryPartition {
                path: None,
                bounds: rect(0.0, 100.0),
                platform: None,
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory("/disk/a".into()),
                    rect: rect(0.0, 100.0),
                    weight: 100,
                }],
            },
            DirectoryPartition {
                path: Some("/disk/a".into()),
                bounds: rect(0.0, 100.0),
                platform: None,
                items: vec![
                    PartitionItem {
                        id: PartitionItemId::File(0),
                        rect: rect(0.0, 50.0),
                        weight: 50,
                    },
                    PartitionItem {
                        id: PartitionItemId::File(1),
                        rect: rect(50.0, 50.0),
                        weight: 50,
                    },
                ],
            },
        ];
        IncrementalGeometry::from_layout(&parts, &[tile(0), tile(1)]).unwrap()
    }
    #[test]
    fn delete_then_insert_reuses_hole_and_retains_other_tile() {
        let token = AtomicU64::new(1);
        let mut g = geometry();
        let delete = g
            .apply(
                &[TileDelta {
                    before: Some(state(0, 50)),
                    after: None,
                }],
                1.0,
                5000.0,
                &token,
                1,
            )
            .unwrap();
        assert_eq!(delete.tiles[&tile(0)], None);
        let add = g
            .apply(
                &[TileDelta {
                    before: None,
                    after: Some(state(2, 50)),
                }],
                1.0,
                0.0,
                &token,
                1,
            )
            .unwrap();
        assert_eq!(add.tiles[&tile(2)], Some(rect(0.0, 50.0)));
        assert!(!add.tiles.contains_key(&tile(1)));
    }

    #[test]
    fn mixed_partition_deletion_compacts_adjacent_tile_without_moving_directory() {
        let parts = vec![
            DirectoryPartition {
                path: None,
                bounds: rect(0.0, 100.0),
                platform: None,
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory("/disk/a".into()),
                    rect: rect(0.0, 100.0),
                    weight: 100,
                }],
            },
            DirectoryPartition {
                path: Some("/disk/a".into()),
                bounds: rect(0.0, 100.0),
                platform: None,
                items: vec![
                    PartitionItem {
                        id: PartitionItemId::File(0),
                        rect: rect(0.0, 25.0),
                        weight: 25,
                    },
                    PartitionItem {
                        id: PartitionItemId::File(1),
                        rect: rect(25.0, 25.0),
                        weight: 25,
                    },
                    PartitionItem {
                        id: PartitionItemId::Directory("/disk/a/nested".into()),
                        rect: rect(50.0, 50.0),
                        weight: 50,
                    },
                ],
            },
            DirectoryPartition {
                path: Some("/disk/a/nested".into()),
                bounds: rect(50.0, 50.0),
                platform: None,
                items: vec![PartitionItem {
                    id: PartitionItemId::File(2),
                    rect: rect(50.0, 50.0),
                    weight: 50,
                }],
            },
        ];
        let mut geometry =
            IncrementalGeometry::from_layout(&parts, &[tile(0), tile(1), tile(2)]).unwrap();
        assert!(geometry.can_compact_leaf_parent(std::path::Path::new("/disk/a")));
        let token = AtomicU64::new(1);
        let changed = geometry
            .apply_compacting(
                &[TileDelta {
                    before: Some(state(0, 25)),
                    after: None,
                }],
                1.0,
                10000.0,
                &token,
                1,
            )
            .unwrap();
        assert_eq!(changed.tiles[&tile(0)], None);
        assert_eq!(changed.tiles[&tile(1)], Some(rect(0.0, 50.0)));
        assert!(!changed.tiles.contains_key(&tile(2)));
    }

    #[test]
    fn optional_slots_restore_vacancies_and_propagate_live_weights() {
        let parts = vec![
            DirectoryPartition {
                path: None,
                bounds: rect(0.0, 100.0),
                platform: None,
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory("/disk".into()),
                    rect: rect(0.0, 100.0),
                    weight: 100,
                }],
            },
            DirectoryPartition {
                path: Some("/disk".into()),
                bounds: rect(0.0, 100.0),
                platform: None,
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory("/disk/a".into()),
                    rect: rect(0.0, 100.0),
                    weight: 100,
                }],
            },
            DirectoryPartition {
                path: Some("/disk/a".into()),
                bounds: rect(0.0, 100.0),
                platform: None,
                items: vec![
                    PartitionItem {
                        id: PartitionItemId::File(0),
                        rect: rect(0.0, 50.0),
                        weight: 50,
                    },
                    PartitionItem {
                        id: PartitionItemId::File(1),
                        rect: rect(50.0, 50.0),
                        weight: 50,
                    },
                ],
            },
        ];
        let mut g =
            IncrementalGeometry::from_layout_optional(&parts, &[None, Some(tile(1))]).unwrap();
        let a = &g.partitions[&Some("/disk/a".into())];
        assert_eq!(a.vacancies(), &[rect(0.0, 50.0)]);
        let (parent_a, key_a) = &g.parents[&PathBuf::from("/disk/a")];
        assert_eq!(
            g.partitions[parent_a]
                .get(&Item::Directory(*key_a))
                .unwrap()
                .weight,
            50
        );
        let (parent_disk, key_disk) = &g.parents[&PathBuf::from("/disk")];
        assert_eq!(
            g.partitions[parent_disk]
                .get(&Item::Directory(*key_disk))
                .unwrap()
                .weight,
            50
        );
        let token = AtomicU64::new(1);
        let update = g
            .apply(
                &[TileDelta {
                    before: None,
                    after: Some(state(2, 50)),
                }],
                1.0,
                0.01,
                &token,
                1,
            )
            .unwrap();
        assert_eq!(update.tiles[&tile(2)], Some(rect(0.0, 50.0)));
        assert!(IncrementalGeometry::from_layout_optional(&parts, &[None]).is_err());
        let reconciled =
            IncrementalGeometry::from_layout_reconciled(&parts, &[None, Some(tile(1))], &[0, 17])
                .unwrap();
        let (parent_a, key_a) = &reconciled.parents[&PathBuf::from("/disk/a")];
        let (parent_disk, key_disk) = &reconciled.parents[&PathBuf::from("/disk")];
        assert_eq!(
            reconciled.partitions[parent_a]
                .get(&Item::Directory(*key_a))
                .unwrap()
                .weight,
            17
        );
        assert_eq!(
            reconciled.partitions[parent_disk]
                .get(&Item::Directory(*key_disk))
                .unwrap()
                .weight,
            17
        );
        assert_eq!(
            reconciled.partitions[&Some("/disk/a".into())]
                .get(&Item::Tile(tile(1)))
                .unwrap()
                .weight,
            17
        );
        assert!(
            IncrementalGeometry::from_layout_reconciled(&parts, &[None, Some(tile(1))], &[0])
                .is_err()
        );
    }

    #[test]
    fn retained_reconciliation_preserves_geometry_and_shares_unchanged_versions() {
        use crate::sparse_projection::SparseProjection;
        use std::path::Path;
        let file = |name: &str, size| scan::FileEntry {
            path: format!("/disk/a/{name}").into(),
            parent: Arc::from(Path::new("/disk/a")),
            size,
            identity: scan::FileIdentity::default(),
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: false,
            can_trash: true,
        };
        let data = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("f", 50), file("g", 50)],
            directories: Arc::from([]),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let old = SparseProjection::new(data.clone(), vec![tile(0), tile(1)]).unwrap();
        let mut next = old.clone();
        next.replace(&[tile(0), tile(1)], vec![(tile(1), file("g", 17))])
            .unwrap();
        let original = geometry();
        let mut updated = original.clone();
        assert!(Arc::ptr_eq(
            &original.partitions[&Some("/disk/a".into())],
            &updated.partitions[&Some("/disk/a".into())]
        ));
        updated.reconcile_retained(&old, &next).unwrap();
        let part = &updated.partitions[&Some("/disk/a".into())];
        assert!(part.get(&Item::Tile(tile(0))).is_none());
        assert_eq!(part.vacancies(), &[rect(0.0, 50.0)]);
        assert_eq!(
            part.get(&Item::Tile(tile(1))).unwrap(),
            Placement {
                rect: rect(50.0, 50.0),
                weight: 17
            }
        );
        assert_eq!(
            original.partitions[&Some("/disk/a".into())]
                .get(&Item::Tile(tile(0)))
                .unwrap()
                .weight,
            50
        );
        let (parent, key) = &updated.parents[&PathBuf::from("/disk/a")];
        assert_eq!(
            updated.partitions[parent]
                .get(&Item::Directory(*key))
                .unwrap()
                .weight,
            17
        );
        // A reused slot is a different identity and must not corrupt the scene.
        let replacement = SparseProjection::new(data, vec![tile(2), tile(1)]).unwrap();
        let mut unchanged = original.clone();
        assert!(matches!(
            unchanged.reconcile_retained(&old, &replacement),
            Err(Error::Rebuild)
        ));
        assert_eq!(
            unchanged.partitions[&Some("/disk/a".into())]
                .get(&Item::Tile(tile(0)))
                .unwrap()
                .weight,
            50
        );
    }
    #[test]
    fn invalid_parent_and_cancellation_leave_geometry_unchanged() {
        let token = AtomicU64::new(1);
        let mut g = geometry();
        let mut invalid = state(0, 1000);
        invalid.parent = "/disk/missing".into();
        let changed = TileDelta {
            before: Some(state(0, 50)),
            after: Some(invalid),
        };
        assert!(matches!(
            g.apply(&[changed], 1.0, 0.0, &token, 1),
            Err(Error::MissingPartition)
        ));
        token.store(2, Ordering::Relaxed);
        assert!(matches!(
            g.apply(
                &[TileDelta {
                    before: Some(state(0, 50)),
                    after: None
                }],
                1.0,
                10000.0,
                &token,
                1
            ),
            Err(Error::Cancelled)
        ));
        // Both failures were transactional: the original tile can still be removed.
        assert!(
            g.apply(
                &[TileDelta {
                    before: Some(state(0, 50)),
                    after: None
                }],
                1.0,
                10000.0,
                &token,
                2
            )
            .is_ok()
        );
    }

    #[test]
    fn stale_before_weight_is_rejected_without_mutation() {
        let token = AtomicU64::new(1);
        let mut g = geometry();
        let stale = TileDelta {
            before: Some(state(0, 42)),
            after: None,
        };
        assert!(matches!(
            g.apply(&[stale], 1.0, 10000.0, &token, 1),
            Err(Error::InvalidDelta)
        ));
        let valid = TileDelta {
            before: Some(state(0, 50)),
            after: None,
        };
        assert!(g.apply(&[valid], 1.0, 10000.0, &token, 1).is_ok());
    }

    #[test]
    fn projection_slots_bind_identity_not_just_sorted_index() {
        let identity = scan::FileIdentity {
            device: 4,
            ..Default::default()
        };
        let parent = scan::FileIdentity {
            device: 2,
            ..Default::default()
        };
        let f = scan::FileEntry {
            path: "/disk/a/f".into(),
            parent: Arc::from(std::path::Path::new("/disk/a")),
            size: 5,
            identity,
            parent_identity: parent,
            parent_is_leaf: true,
            can_trash: true,
        };
        let data = scan::ScanResult {
            root: "/disk".into(),
            files: vec![f],
            directories: vec![scan::DirectoryFact {
                path: "/disk/a".into(),
                identity: parent,
                enumeration_complete: true,
                has_subdirectories: false,
            }]
            .into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        };
        let scene = Scene::from_scan(Arc::new(data));
        let mut projected = scan::ScanResult {
            root: "/disk".into(),
            files: vec![scene.file(scene.source_id(0).unwrap()).unwrap().clone()],
            directories: Arc::from([]),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        };
        let slots = projection_slots(&scene, &projected, Projection::Files).unwrap();
        assert_eq!(slots, vec![TileId::File(scene.source_id(0).unwrap())]);
        projected.files[0].identity.device = 99;
        assert!(matches!(
            projection_slots(&scene, &projected, Projection::Files),
            Err(Error::InvalidInitialLayout)
        ));
        assert!(matches!(
            projection_slots(&scene, &projected, Projection::Hybrid(&[])),
            Err(Error::InvalidInitialLayout)
        ));
    }

    #[test]
    fn ancestor_area_drift_requests_rebuild_atomically() {
        let token = AtomicU64::new(1);
        let parts = vec![
            DirectoryPartition {
                path: None,
                bounds: rect(0.0, 100.0),
                platform: None,
                items: vec![
                    PartitionItem {
                        id: PartitionItemId::Directory("/disk/a".into()),
                        rect: rect(0.0, 50.0),
                        weight: 100,
                    },
                    PartitionItem {
                        id: PartitionItemId::Directory("/disk/b".into()),
                        rect: rect(50.0, 50.0),
                        weight: 100,
                    },
                ],
            },
            DirectoryPartition {
                path: Some("/disk/a".into()),
                bounds: rect(0.0, 50.0),
                platform: None,
                items: vec![
                    PartitionItem {
                        id: PartitionItemId::File(0),
                        rect: rect(0.0, 25.0),
                        weight: 50,
                    },
                    PartitionItem {
                        id: PartitionItemId::File(1),
                        rect: rect(25.0, 25.0),
                        weight: 50,
                    },
                ],
            },
            DirectoryPartition {
                path: Some("/disk/b".into()),
                bounds: rect(50.0, 50.0),
                platform: None,
                items: vec![PartitionItem {
                    id: PartitionItemId::File(2),
                    rect: rect(50.0, 50.0),
                    weight: 100,
                }],
            },
        ];
        let mut g = IncrementalGeometry::from_layout(&parts, &[tile(0), tile(1), tile(2)]).unwrap();
        let added = TileDelta {
            before: None,
            after: Some(state(3, 100)),
        };
        assert!(matches!(
            g.apply(&[added], 1.0, 100.0, &token, 1),
            Err(Error::Rebuild)
        ));
        // Rejected work must not allocate the tile or change the live weights.
        assert!(
            g.apply(
                &[TileDelta {
                    before: Some(state(3, 100)),
                    after: None
                }],
                1.0,
                10000.0,
                &token,
                1
            )
            .is_err()
        );
        assert!(
            g.apply(
                &[TileDelta {
                    before: Some(state(0, 50)),
                    after: None
                }],
                1.0,
                10000.0,
                &token,
                1
            )
            .is_ok()
        );
    }

    #[test]
    fn simulated_delete_and_add_update_render_owner_without_changing_neighbor() {
        use crate::{
            directory_map,
            render::{self, RenderEntry},
        };
        use std::path::Path;

        let token = AtomicU64::new(1);
        let bounds = rect(0.0, 100.0);
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk/a"), vec![(0, 50), (1, 50)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let mut geometry =
            IncrementalGeometry::from_layout(&layout.partitions, &[tile(0), tile(1)]).unwrap();
        let mut entries = layout.entries.clone();
        let platforms = layout.platforms.clone();
        let mut map = render::RenderMap::build_with_platforms(
            entries.clone(),
            platforms.clone(),
            bounds,
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        let old_rect = map.rect(0).unwrap();
        let neighbor = map.rect(1).unwrap();
        let neighbor_hit = (neighbor.x + neighbor.w / 2.0, neighbor.y + neighbor.h / 2.0);
        let old_hit = (old_rect.x + old_rect.w / 2.0, old_rect.y + old_rect.h / 2.0);
        assert_eq!(map.hit_test(neighbor_hit.0, neighbor_hit.1), Some(1));
        assert_eq!(map.hit_test(old_hit.0, old_hit.1), Some(0));
        let first_image = map.image.as_ref().unwrap().clone();

        let deletion = geometry
            .apply(
                &[TileDelta {
                    before: Some(state(0, 50)),
                    after: None,
                }],
                1.0,
                10000.0,
                &token,
                1,
            )
            .unwrap();
        assert_eq!(deletion.tiles[&tile(0)], None);
        entries.retain(|entry| entry.id != 0);
        let dirty = deletion.dirty[0];
        let region = [
            dirty.x.floor().max(0.0) as usize,
            dirty.y.floor().max(0.0) as usize,
            (dirty.x + dirty.w).ceil().min(100.0) as usize,
            (dirty.y + dirty.h).ceil().min(100.0) as usize,
        ];
        let patch =
            render::raster_patch(&entries, &platforms, bounds, [100, 100], region, &token, 1)
                .unwrap();
        assert!(map.apply_scene_patch(&patch, &[(0, None)], None));
        assert_eq!(map.hit_test(old_hit.0, old_hit.1), None);
        assert_eq!(map.hit_test(neighbor_hit.0, neighbor_hit.1), Some(1));

        let add = geometry
            .apply(
                &[TileDelta {
                    before: None,
                    after: Some(state(2, 50)),
                }],
                1.0,
                0.01,
                &token,
                1,
            )
            .unwrap();
        let new_rect = add.tiles[&tile(2)].unwrap();
        assert_eq!(new_rect, old_rect);
        let new_entry = RenderEntry {
            id: 2,
            rect: new_rect,
            color: crate::color(Path::new("/disk/a")),
        };
        entries.push(new_entry);
        let patch =
            render::raster_patch(&entries, &platforms, bounds, [100, 100], region, &token, 1)
                .unwrap();
        assert!(map.apply_scene_patch(&patch, &[(2, Some(new_entry))], None));
        assert_eq!(map.hit_test(old_hit.0, old_hit.1), Some(2));
        assert_eq!(map.hit_test(neighbor_hit.0, neighbor_hit.1), Some(1));
        // Replacing the same tile in the same geometry restores all patched
        // pixels; outside the patch the original texture has never changed.
        for y in region[1]..region[3] {
            for x in region[0]..region[2] {
                assert_eq!(
                    patch.image.pixels[(y - region[1]) * patch.image.size[0] + x - region[0]],
                    first_image.pixels[y * 100 + x]
                );
            }
        }
    }
}
