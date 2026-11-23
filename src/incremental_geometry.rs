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
    /// Published platform ownership is keyed by directory, not inferred from
    /// equal rectangles: unary ancestors can share an identical content frame.
    surfaces: Arc<BTreeMap<Option<PathBuf>, Surface>>,
    /// Directory child -> actual exported parent, including synthetic root.
    parents: Arc<BTreeMap<PathBuf, (Option<PathBuf>, u32)>>,
    /// Rare tiles whose allocation is inside their own directory rather than
    /// their logical parent. The scene projection retains the real path.
    tile_parent_overrides: Arc<BTreeMap<TileId, PathBuf>>,
}

#[derive(Clone, Copy)]
struct Surface {
    outer: Rect,
    platform: Option<crate::render::Platform>,
}

impl Surface {
    fn from_partition(part: &DirectoryPartition) -> Self {
        Self {
            outer: part
                .platform
                .map_or(part.bounds, |platform| platform.bounds),
            platform: part.platform,
        }
    }
}

pub struct GeometryUpdate {
    pub dirty: Vec<Rect>,
    /// Geometry changed for these tiles; removed tiles are None.
    pub tiles: BTreeMap<TileId, Option<Rect>>,
}

pub enum RootDonor {
    Tile(TileId, Rect, Rect),
    Directory(PathBuf, Rect, Rect),
}

impl IncrementalGeometry {
    /// The published allocation and inherited platform depth for a subtree.
    /// Check the parent's allocation as well: stale platform metadata must not
    /// be used to redraw a different branch after an interrupted update.
    pub(crate) fn subtree_surface(
        &self,
        path: &std::path::Path,
        root: &std::path::Path,
    ) -> Option<(Rect, u8)> {
        if path == root || !path.starts_with(root) {
            return None;
        }
        let (parent, id) = self.parents.get(path)?;
        let outer = self
            .partitions
            .get(parent)?
            .get(&Item::Directory(*id))?
            .rect;
        let surface = self.surfaces.get(&Some(path.to_path_buf()))?;
        if surface.outer != outer {
            return None;
        }
        let mut depth = 0u8;
        let mut ancestor = path.parent();
        while let Some(current) = ancestor {
            if current == root || !current.starts_with(root) {
                break;
            }
            if self
                .surfaces
                .get(&Some(current.to_path_buf()))?
                .platform
                .is_some()
            {
                depth = depth.saturating_add(1);
            }
            ancestor = current.parent();
        }
        Some((outer, depth))
    }

    #[cfg(test)]
    pub(crate) fn has_partition(&self, path: &std::path::Path) -> bool {
        self.partitions.contains_key(&Some(path.to_path_buf()))
    }

    /// Missing descendant materializations must also be restored before a
    /// parent is laid out; otherwise their unchanged files would be omitted.
    pub(crate) fn subtree_has_missing_partition(&self, path: &std::path::Path) -> bool {
        self.surfaces
            .range(Some(path.to_path_buf())..)
            .take_while(|(key, _)| key.as_ref().is_some_and(|key| key.starts_with(path)))
            .any(|(key, _)| !self.partitions.contains_key(key))
    }

    #[cfg(test)]
    pub(crate) fn forget_partition_for_test(&mut self, path: &std::path::Path) {
        self.partitions.remove(&Some(path.to_path_buf()));
    }
    /// Return the allocation's persisted layout parent, falling back to the
    /// filesystem parent used by the scene projection.
    pub(crate) fn tile_layout_parent<'a>(
        &'a self,
        id: TileId,
        logical: &'a std::path::Path,
    ) -> &'a std::path::Path {
        self.tile_parent_overrides
            .get(&id)
            .map_or(logical, PathBuf::as_path)
    }
    /// Recover the rare layout-parent overrides when restoring exported
    /// partitions. Only a directory tile allocated inside itself is supported;
    /// other inconsistencies must not silently retarget operations.
    pub(crate) fn restore_tile_layout_parents(
        &mut self,
        parts: &[DirectoryPartition],
        projection: &crate::sparse_projection::SparseProjection,
    ) -> Result<(), Error> {
        let mut overrides = BTreeMap::new();
        for part in parts {
            for item in &part.items {
                let PartitionItemId::File(slot) = &item.id else {
                    continue;
                };
                let Some(id) = projection.id(*slot) else {
                    continue;
                };
                let entry = projection.get(*slot).ok_or(Error::InvalidInitialLayout)?;
                if part.path.as_deref() == Some(entry.parent.as_ref()) {
                    continue;
                }
                let Some(actual) = part.path.as_deref() else {
                    return Err(Error::InvalidInitialLayout);
                };
                if !matches!(id, TileId::Directory(_)) || entry.path != actual {
                    return Err(Error::InvalidInitialLayout);
                }
                if overrides.insert(id, actual.to_path_buf()).is_some() {
                    return Err(Error::InvalidInitialLayout);
                }
            }
        }
        self.tile_parent_overrides = Arc::new(overrides);
        Ok(())
    }
    /// Count direct file/folder tiles without traversing descendant partitions.
    /// None means that the directory has no existing layout allocation.
    pub fn direct_tile_count_up_to(&self, path: &std::path::Path, limit: usize) -> Option<usize> {
        Some(
            self.partitions
                .get(&Some(path.to_path_buf()))?
                .entries()
                .filter(|(item, _)| matches!(item, Item::Tile(_)))
                .take(limit.saturating_add(1))
                .count(),
        )
    }
    pub fn replace_root_tile(
        &self,
        root: &std::path::Path,
        old: TileId,
        new: TileId,
        value: u64,
    ) -> Result<(Self, Rect), Error> {
        let mut result = self.clone();
        let partition = result
            .partitions
            .get_mut(&Some(root.to_path_buf()))
            .ok_or(Error::MissingPartition)?;
        let partition = Arc::make_mut(partition);
        let frame = partition
            .get(&Item::Tile(old))
            .ok_or(Error::InvalidDelta)?
            .rect;
        partition
            .replace_key(&Item::Tile(old), Item::Tile(new), value)
            .map_err(|_| Error::InvalidDelta)?;
        if old != new {
            if let Some(parent) = Arc::make_mut(&mut result.tile_parent_overrides).remove(&old) {
                Arc::make_mut(&mut result.tile_parent_overrides).insert(new, parent);
            }
        }
        let mut path = root.to_path_buf();
        while let Some((parent, id)) = result.parents.get(&path).cloned() {
            let total = result
                .partitions
                .get(&Some(path.clone()))
                .ok_or(Error::MissingPartition)?
                .entries()
                .fold(0u64, |sum, (_, entry)| {
                    sum.saturating_add(entry.weight.max(1))
                });
            let partition = result
                .partitions
                .get_mut(&parent)
                .ok_or(Error::MissingPartition)?;
            Arc::make_mut(partition)
                .set_weight(&Item::Directory(id), total)
                .map_err(|_| Error::InvalidDelta)?;
            match parent {
                Some(parent) => path = parent,
                None => break,
            }
        }
        Ok((result, frame))
    }
    /// Remove a direct root tile without reallocating siblings. Its frame is
    /// retained as a vacancy for another tile or child in the same batch.
    pub(crate) fn remove_root_tile(
        &self,
        root: &std::path::Path,
        tile: TileId,
    ) -> Result<(Self, Rect), Error> {
        let mut result = self.clone();
        let partition = result
            .partitions
            .get_mut(&Some(root.to_path_buf()))
            .ok_or(Error::MissingPartition)?;
        let frame = Arc::make_mut(partition)
            .remove(&Item::Tile(tile))
            .map_err(|_| Error::InvalidDelta)?
            .dirty;
        Arc::make_mut(&mut result.tile_parent_overrides).remove(&tile);
        Ok((result.refresh_ancestor_weights(root)?, frame))
    }
    /// Allocate a root aggregate in its actual parent without moving the scan
    /// root's child allocation. Returning None leaves the snapshot unchanged;
    /// a missing vacancy cannot safely be solved by re-laying out the root.
    pub(crate) fn allocate_scan_root_aggregate_in_vacancy(
        &self,
        root: &std::path::Path,
        tile: TileId,
        value: u64,
    ) -> Result<Option<(Self, Rect)>, Error> {
        let parent = root.parent().unwrap_or(root);
        let Some(partition) = self.partitions.get(&Some(parent.to_path_buf())) else {
            return Ok(None);
        };
        if partition.vacancies().is_empty() {
            return Ok(None);
        }
        let mut result = self.clone();
        let partition = result
            .partitions
            .get_mut(&Some(parent.to_path_buf()))
            .ok_or(Error::MissingPartition)?;
        let (frame, donor) = Arc::make_mut(partition)
            .allocate_local(Item::Tile(tile), value)
            .map_err(|_| Error::InvalidDelta)?;
        if donor.is_some() {
            return Err(Error::InvalidDelta);
        }
        Ok(Some((result.refresh_ancestor_weights(parent)?, frame)))
    }
    /// Create the first root content partition beside an existing aggregate
    /// in its synthetic parent. Only the aggregate can be used as donor; all
    /// changes remain private until the caller publishes the resulting scene.
    pub(crate) fn lone_root_parent_tile(&self, root: &std::path::Path) -> Option<TileId> {
        if self.partitions.contains_key(&Some(root.to_path_buf())) {
            return None;
        }
        let parent = root.parent()?;
        let mut entries = self.partitions.get(&Some(parent.to_path_buf()))?.entries();
        match (entries.next(), entries.next()) {
            (Some((Item::Tile(id @ TileId::Directory(_)), _)), None) => Some(*id),
            _ => None,
        }
    }
    pub(crate) fn bootstrap_root_beside_aggregate(
        &self,
        root: &std::path::Path,
        aggregate: TileId,
        value: u64,
    ) -> Result<Option<(Self, Rect, Option<(Rect, Rect)>)>, Error> {
        if self.partitions.contains_key(&Some(root.to_path_buf())) {
            return Ok(None);
        }
        let Some(parent) = root.parent() else {
            return Ok(None);
        };
        let Some(partition) = self.partitions.get(&Some(parent.to_path_buf())) else {
            return Ok(None);
        };
        let mut live = partition.entries();
        if !matches!(live.next(), Some((Item::Tile(id), _)) if *id == aggregate)
            || live.next().is_some()
        {
            return Ok(None);
        }
        let (mut staged, frame, donor) = self.allocate_root_child(parent, root, None, value)?;
        let donor = match donor {
            None => None,
            Some(RootDonor::Tile(id, old, new)) if id == aggregate => Some((old, new)),
            _ => return Err(Error::InvalidDelta),
        };
        let mut empty = Partition::from_layout(frame, std::iter::empty::<(Item, Placement)>())
            .map_err(|_| Error::InvalidInitialLayout)?;
        empty
            .restore_vacancy(frame)
            .map_err(|_| Error::InvalidInitialLayout)?;
        if staged
            .partitions
            .insert(Some(root.to_path_buf()), Arc::new(empty))
            .is_some()
        {
            return Err(Error::InvalidDelta);
        }
        Arc::make_mut(&mut staged.surfaces).insert(
            Some(root.to_path_buf()),
            Surface {
                outer: frame,
                platform: None,
            },
        );
        Ok(Some((staged, frame, donor)))
    }
    /// Reserve one root child inside an existing hole or by splitting only one
    /// donor frame. The caller re-lays out any donor/new subtree within those
    /// frames before publishing the returned COW scene.
    pub fn allocate_root_child(
        &self,
        root: &std::path::Path,
        pivot: &std::path::Path,
        tile: Option<TileId>,
        value: u64,
    ) -> Result<(Self, Rect, Option<RootDonor>), Error> {
        let root_aggregate = pivot == root && matches!(tile, Some(TileId::Directory(_)));
        if !root_aggregate && pivot.parent() != Some(root) {
            return Err(Error::InvalidDelta);
        }
        let mut result = self.clone();
        let key = if let Some(tile) = tile {
            Item::Tile(tile)
        } else {
            if result.parents.contains_key(pivot) {
                return Err(Error::InvalidDelta);
            }
            let id = result
                .parents
                .values()
                .map(|(_, id)| *id)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(Error::InvalidInitialLayout)?;
            Arc::make_mut(&mut result.parents)
                .insert(pivot.to_path_buf(), (Some(root.to_path_buf()), id));
            Item::Directory(id)
        };
        let partition = result
            .partitions
            .get_mut(&Some(root.to_path_buf()))
            .ok_or(Error::MissingPartition)?;
        let (frame, donor) = Arc::make_mut(partition)
            .allocate_local(key, value)
            .map_err(|_| Error::InvalidDelta)?;
        if root_aggregate {
            Arc::make_mut(&mut result.tile_parent_overrides)
                .insert(tile.ok_or(Error::InvalidDelta)?, root.to_path_buf());
        }
        let donor = match donor {
            None => None,
            Some((Item::Tile(tile), old, new)) => Some(RootDonor::Tile(tile, old, new)),
            Some((Item::Directory(id), old, new)) => Some(
                self.parents
                    .iter()
                    .find_map(|(path, (_, candidate))| {
                        (*candidate == id).then(|| RootDonor::Directory(path.clone(), old, new))
                    })
                    .ok_or(Error::InvalidDelta)?,
            ),
        };
        // A root weight may be referenced by its synthetic ancestor.
        let mut path = root.to_path_buf();
        while let Some((parent, id)) = result.parents.get(&path).cloned() {
            let total = result
                .partitions
                .get(&Some(path.clone()))
                .ok_or(Error::MissingPartition)?
                .entries()
                .fold(0u64, |sum, (_, entry)| {
                    sum.saturating_add(entry.weight.max(1))
                });
            let partition = result
                .partitions
                .get_mut(&parent)
                .ok_or(Error::MissingPartition)?;
            Arc::make_mut(partition)
                .set_weight(&Item::Directory(id), total)
                .map_err(|_| Error::InvalidDelta)?;
            match parent {
                Some(parent) => path = parent,
                None => break,
            }
        }
        Ok((result, frame, donor))
    }

    /// Replace only a single allocated subtree after its frame changes. The
    /// parent's allocation and all unrelated partition pages remain shared.
    pub fn install_subtree(
        &self,
        pivot: &std::path::Path,
        subparts: &[DirectoryPartition],
        projection: &crate::sparse_projection::SparseProjection,
    ) -> Result<Self, Error> {
        if !self.parents.contains_key(pivot) {
            return Err(Error::MissingPartition);
        }
        let mut local = Self::from_layout_mapped(
            subparts,
            |slot| (slot < projection.len()).then(|| projection.id(slot).map(|id| (id, None))),
            true,
        )?;
        local.restore_tile_layout_parents(subparts, projection)?;
        let mut result = self.clone();
        let mut next_id = self
            .parents
            .values()
            .map(|(_, id)| *id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(Error::InvalidInitialLayout)?;
        result
            .partitions
            .retain(|key, _| key.as_ref().is_none_or(|path| !path.starts_with(pivot)));
        Arc::make_mut(&mut result.surfaces)
            .retain(|key, _| key.as_ref().is_none_or(|path| !path.starts_with(pivot)));
        Arc::make_mut(&mut result.surfaces).extend(
            local
                .surfaces
                .iter()
                .map(|(path, surface)| (path.clone(), *surface)),
        );
        Arc::make_mut(&mut result.parents)
            .retain(|path, _| path == pivot || !path.starts_with(pivot));
        let overrides = Arc::make_mut(&mut result.tile_parent_overrides);
        overrides.retain(|_, parent| !parent.starts_with(pivot));
        overrides.extend(
            local
                .tile_parent_overrides
                .iter()
                .map(|(id, path)| (*id, path.clone())),
        );
        let mut translated = BTreeMap::new();
        for (path, (parent, local_id)) in local.parents.iter() {
            let id = next_id;
            next_id = next_id.checked_add(1).ok_or(Error::InvalidInitialLayout)?;
            translated.insert(*local_id, id);
            if Arc::make_mut(&mut result.parents)
                .insert(path.clone(), (parent.clone(), id))
                .is_some()
            {
                return Err(Error::InvalidInitialLayout);
            }
        }
        for (path, partition) in local.partitions {
            if path.as_ref().is_none_or(|path| !path.starts_with(pivot)) {
                return Err(Error::InvalidInitialLayout);
            }
            let mapped = partition
                .map_keys(|item| match item {
                    Item::Tile(id) => Some(Item::Tile(*id)),
                    Item::Directory(id) => translated.get(id).copied().map(Item::Directory),
                })
                .ok_or(Error::InvalidInitialLayout)?;
            if result.partitions.insert(path, Arc::new(mapped)).is_some() {
                return Err(Error::InvalidInitialLayout);
            }
        }
        Ok(result)
    }

    pub fn subtree_tiles(&self, root: &std::path::Path) -> Vec<TileId> {
        // Paths sort by components, so a subtree is a contiguous range. Avoid
        // scanning every unrelated directory for a local update.
        self.partitions
            .range(Some(root.to_path_buf())..)
            .take_while(|(path, _)| path.as_ref().is_some_and(|path| path.starts_with(root)))
            .flat_map(|(_, part)| {
                part.entries().filter_map(|(id, _)| match id {
                    Item::Tile(id) => Some(*id),
                    _ => None,
                })
            })
            .collect()
    }
    /// The existing allocation of the scan root in its persisted parent,
    /// including the synthetic `None` parent used for `/`.
    pub(crate) fn scan_root_frame(&self, root: &std::path::Path) -> Option<Rect> {
        let (parent, id) = self.parents.get(root)?;
        self.partitions
            .get(parent)?
            .get(&Item::Directory(*id))
            .map(|entry| entry.rect)
    }
    /// Install the first nonempty root under the existing synthetic canvas.
    /// Empty directory scaffolding is discarded only after verifying that no
    /// live tile exists anywhere; a previous snapshot remains untouched.
    pub(crate) fn bootstrap_empty_root(
        &self,
        root: &std::path::Path,
        subparts: &[DirectoryPartition],
        projection: &crate::sparse_projection::SparseProjection,
        total_weight: u64,
    ) -> Result<Self, Error> {
        if self
            .partitions
            .values()
            .any(|part| part.entries().any(|(key, _)| matches!(key, Item::Tile(_))))
        {
            return Err(Error::InvalidDelta);
        }
        let frame = self
            .partitions
            .get(&None)
            .ok_or(Error::MissingPartition)?
            .bounds();
        let mut ancestors: Vec<PathBuf> =
            root.ancestors().map(std::path::Path::to_path_buf).collect();
        ancestors.reverse();
        if ancestors.is_empty() {
            return Err(Error::InvalidDelta);
        }
        let mut partitions = BTreeMap::new();
        let mut parents = BTreeMap::new();
        let mut parent = None;
        for (index, path) in ancestors.into_iter().enumerate() {
            let id = u32::try_from(index).map_err(|_| Error::InvalidInitialLayout)?;
            let partition = Partition::from_layout(
                frame,
                [(
                    Item::Directory(id),
                    Placement {
                        rect: frame,
                        weight: total_weight.max(1),
                    },
                )],
            )
            .map_err(|_| Error::InvalidInitialLayout)?;
            partitions.insert(parent.clone(), Arc::new(partition));
            parents.insert(path.clone(), (parent, id));
            parent = Some(path);
        }
        let surfaces = partitions
            .iter()
            .map(|(path, partition)| {
                (
                    path.clone(),
                    Surface {
                        outer: partition.bounds(),
                        platform: None,
                    },
                )
            })
            .collect();
        let shell = Self {
            partitions,
            surfaces: Arc::new(surfaces),
            parents: Arc::new(parents),
            tile_parent_overrides: Arc::new(BTreeMap::new()),
        };
        shell.install_subtree(root, subparts, projection)
    }
    /// Recompute weights along one existing subtree's ancestor chain without
    /// changing any allocated frames.
    pub fn refresh_ancestor_weights(&self, subtree: &std::path::Path) -> Result<Self, Error> {
        let mut result = self.clone();
        let mut path = subtree.to_path_buf();
        while let Some((parent, id)) = result.parents.get(&path).cloned() {
            let total = result
                .partitions
                .get(&Some(path.clone()))
                .ok_or(Error::MissingPartition)?
                .entries()
                .fold(0u64, |sum, (_, entry)| {
                    sum.saturating_add(entry.weight.max(1))
                });
            let partition = result
                .partitions
                .get_mut(&parent)
                .ok_or(Error::MissingPartition)?;
            Arc::make_mut(partition)
                .set_weight(&Item::Directory(id), total)
                .map_err(|_| Error::InvalidDelta)?;
            match parent {
                Some(parent) => path = parent,
                None => break,
            }
        }
        Ok(result)
    }
    /// Remove an entire root child while preserving its allocations as holes.
    /// Folder mode can have both a direct aggregate tile and a descendant
    /// partition for the same directory, so the two are removed independently.
    pub fn remove_root_child(
        &self,
        root: &std::path::Path,
        pivot: &std::path::Path,
        tile: Option<TileId>,
    ) -> Result<(Self, Option<Rect>, Option<Rect>), Error> {
        if pivot.parent() != Some(root) {
            return Err(Error::InvalidDelta);
        }
        let directory = self
            .parents
            .get(pivot)
            .map(|(parent, id)| (parent.clone(), *id));
        if directory
            .as_ref()
            .is_some_and(|(parent, _)| parent.as_deref() != Some(root))
        {
            return Err(Error::InvalidDelta);
        }
        if tile.is_none() && directory.is_none() {
            return Err(Error::MissingPartition);
        }
        let mut result = self.clone();
        let partition = result
            .partitions
            .get_mut(&Some(root.to_path_buf()))
            .ok_or(Error::MissingPartition)?;
        let partition = Arc::make_mut(partition);
        let tile_frame = tile
            .map(|id| {
                partition
                    .remove(&Item::Tile(id))
                    .map(|update| update.dirty)
                    .map_err(|_| Error::InvalidDelta)
            })
            .transpose()?;
        let directory_frame = directory
            .map(|(_, id)| {
                partition
                    .remove(&Item::Directory(id))
                    .map(|update| update.dirty)
                    .map_err(|_| Error::InvalidDelta)
            })
            .transpose()?;
        if directory_frame.is_some() {
            result
                .partitions
                .retain(|key, _| key.as_ref().is_none_or(|path| !path.starts_with(pivot)));
            Arc::make_mut(&mut result.surfaces)
                .retain(|key, _| key.as_ref().is_none_or(|path| !path.starts_with(pivot)));
            Arc::make_mut(&mut result.parents).retain(|path, _| !path.starts_with(pivot));
        }
        Arc::make_mut(&mut result.tile_parent_overrides).retain(|id, parent| {
            Some(*id) != tile && (directory_frame.is_none() || !parent.starts_with(pivot))
        });
        let mut path = root.to_path_buf();
        while let Some((parent, id)) = result.parents.get(&path).cloned() {
            let total = result
                .partitions
                .get(&Some(path.clone()))
                .ok_or(Error::MissingPartition)?
                .entries()
                .fold(0u64, |sum, (_, entry)| {
                    sum.saturating_add(entry.weight.max(1))
                });
            let partition = result
                .partitions
                .get_mut(&parent)
                .ok_or(Error::MissingPartition)?;
            Arc::make_mut(partition)
                .set_weight(&Item::Directory(id), total)
                .map_err(|_| Error::InvalidDelta)?;
            match parent {
                Some(parent) => path = parent,
                None => break,
            }
        }
        Ok((result, tile_frame, directory_frame))
    }
    pub fn root_fold_frame(
        &self,
        root: &std::path::Path,
        pivot: &std::path::Path,
        old_tile: Option<TileId>,
    ) -> Option<Rect> {
        let key = match old_tile {
            Some(id) => Item::Tile(id),
            None => Item::Directory(self.parents.get(pivot)?.1),
        };
        self.partitions
            .get(&Some(root.to_path_buf()))?
            .get(&key)
            .map(|p| p.rect)
    }
    /// Replace a root child tile with its locally laid-out directory subtree,
    /// or fold that subtree back into one tile. Unrelated partition pages and
    /// all parent vacancies are retained; only the root's touched page and the
    /// affected subtree are copied.
    pub fn replace_root_fold(
        &self,
        root: &std::path::Path,
        pivot: &std::path::Path,
        old_tile: Option<TileId>,
        new_tile: Option<(TileId, u64)>,
        subparts: &[DirectoryPartition],
        projection: &crate::sparse_projection::SparseProjection,
        weight: u64,
    ) -> Result<Self, Error> {
        if old_tile.is_some() == new_tile.is_some() || pivot.parent() != Some(root) {
            return Err(Error::InvalidDelta);
        }
        let expanding = old_tile.is_some();
        let mut result = self.clone();
        let old_directory = self.parents.get(pivot).map(|(_, id)| *id);
        if expanding && old_directory.is_some() || !expanding && old_directory.is_none() {
            return Err(Error::InvalidDelta);
        }
        let mut next_id = self
            .parents
            .values()
            .map(|(_, id)| *id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(Error::InvalidInitialLayout)?;
        let new_directory = if expanding {
            let id = next_id;
            next_id = next_id.checked_add(1).ok_or(Error::InvalidInitialLayout)?;
            Arc::make_mut(&mut result.parents)
                .insert(pivot.to_path_buf(), (Some(root.to_path_buf()), id));
            Some(id)
        } else {
            None
        };
        let old_key = old_tile
            .map(Item::Tile)
            .or_else(|| old_directory.map(Item::Directory))
            .ok_or(Error::InvalidDelta)?;
        let new_key = new_tile
            .map(|(id, _)| Item::Tile(id))
            .or_else(|| new_directory.map(Item::Directory))
            .ok_or(Error::InvalidDelta)?;
        let root_part = result
            .partitions
            .get_mut(&Some(root.to_path_buf()))
            .ok_or(Error::MissingPartition)?;
        Arc::make_mut(root_part)
            .replace_key(&old_key, new_key, weight)
            .map_err(|_| Error::InvalidDelta)?;
        if let Some(id) = old_tile {
            Arc::make_mut(&mut result.tile_parent_overrides).remove(&id);
        }
        if expanding {
            let local = Self::from_layout_mapped(
                subparts,
                |slot| (slot < projection.len()).then(|| projection.id(slot).map(|id| (id, None))),
                true,
            )?;
            Arc::make_mut(&mut result.surfaces).extend(
                local
                    .surfaces
                    .iter()
                    .map(|(path, surface)| (path.clone(), *surface)),
            );
            let mut translated = BTreeMap::new();
            for (path, (parent, local_id)) in local.parents.iter() {
                let id = next_id;
                next_id = next_id.checked_add(1).ok_or(Error::InvalidInitialLayout)?;
                translated.insert(*local_id, id);
                if Arc::make_mut(&mut result.parents)
                    .insert(path.clone(), (parent.clone(), id))
                    .is_some()
                {
                    return Err(Error::InvalidInitialLayout);
                }
            }
            for (path, partition) in local.partitions {
                let mapped = partition
                    .map_keys(|item| match item {
                        Item::Tile(id) => Some(Item::Tile(*id)),
                        Item::Directory(id) => translated.get(id).copied().map(Item::Directory),
                    })
                    .ok_or(Error::InvalidInitialLayout)?;
                if result.partitions.insert(path, Arc::new(mapped)).is_some() {
                    return Err(Error::InvalidInitialLayout);
                }
            }
        } else {
            result
                .partitions
                .retain(|key, _| key.as_ref().is_none_or(|path| !path.starts_with(pivot)));
            Arc::make_mut(&mut result.surfaces)
                .retain(|key, _| key.as_ref().is_none_or(|path| !path.starts_with(pivot)));
            Arc::make_mut(&mut result.parents).retain(|path, _| !path.starts_with(pivot));
            Arc::make_mut(&mut result.tile_parent_overrides)
                .retain(|_, parent| !parent.starts_with(pivot));
        }
        // Propagate actual weights without changing any ancestor allocation.
        let mut path = root.to_path_buf();
        while let Some((parent, id)) = result.parents.get(&path).cloned() {
            let total = result
                .partitions
                .get(&Some(path.clone()))
                .ok_or(Error::MissingPartition)?
                .entries()
                .fold(0u64, |sum, (_, value)| {
                    sum.saturating_add(value.weight.max(1))
                });
            let partition = result
                .partitions
                .get_mut(&parent)
                .ok_or(Error::MissingPartition)?;
            Arc::make_mut(partition)
                .set_weight(&Item::Directory(id), total)
                .map_err(|_| Error::InvalidDelta)?;
            match parent {
                Some(parent) => path = parent,
                None => break,
            }
        }
        Ok(result)
    }

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
        // Platform ownership is part of the published geometry. Verify that it
        // still describes the rendered scene instead of guessing by matching
        // content rectangles, which can coincide along unary directory paths.
        let key = |p: &crate::render::Platform| {
            let b = p.bounds;
            let t = p.top;
            [
                b.x.to_bits(),
                b.y.to_bits(),
                b.w.to_bits(),
                b.h.to_bits(),
                t.x.to_bits(),
                t.y.to_bits(),
                t.w.to_bits(),
                t.h.to_bits(),
                u32::from_le_bytes(p.color.to_array()),
            ]
        };
        let mut counts = BTreeMap::<[u32; 9], usize>::new();
        for platform in platforms {
            *counts.entry(key(platform)).or_default() += 1;
        }
        for surface in self.surfaces.values() {
            if let Some(platform) = surface.platform {
                let count = counts.get_mut(&key(&platform)).ok_or(Error::Rebuild)?;
                *count = count.checked_sub(1).ok_or(Error::Rebuild)?;
            }
        }
        if counts.values().any(|count| *count != 0) {
            return Err(Error::Rebuild);
        }
        let mut out = Vec::with_capacity(self.partitions.len());
        for (path, partition) in &self.partitions {
            let platform = self.surfaces.get(path).ok_or(Error::Rebuild)?.platform;
            let mut items = Vec::new();
            for (id, placement) in partition.entries() {
                let id = match id {
                    Item::Directory(id) => PartitionItemId::Directory(Box::new(
                        (*by_id.get(id).ok_or(Error::Rebuild)?).clone(),
                    )),
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
                vacancies: partition.vacancies().to_vec(),
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
        let mut surfaces = BTreeMap::new();
        for part in parts {
            if surfaces
                .insert(part.path.clone(), Surface::from_partition(part))
                .is_some()
            {
                return Err(Error::InvalidInitialLayout);
            }
            let mut items = Vec::with_capacity(part.items.len());
            let mut vacant = part.vacancies.clone();
            for entry in &part.items {
                let (id, value) = match &entry.id {
                    PartitionItemId::Directory(path) => {
                        let index = u32::try_from(parents.len())
                            .map_err(|_| Error::InvalidInitialLayout)?;
                        if parents
                            .insert(path.as_ref().clone(), (part.path.clone(), index))
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
            surfaces: Arc::new(surfaces),
            parents: Arc::new(parents),
            tile_parent_overrides: Arc::new(BTreeMap::new()),
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
                    let layout_parent = self.tile_layout_parent(id, &previous.parent).to_path_buf();
                    let key = Some(layout_parent.clone());
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
                    pending.insert(layout_parent);
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
        Arc::make_mut(&mut self.tile_parent_overrides).retain(|id, _| new.slot(*id).is_some());
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
        let mut overrides = self.tile_parent_overrides.as_ref().clone();
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
                    let key = Some(
                        overrides
                            .get(&old.id)
                            .cloned()
                            .unwrap_or_else(|| old.parent.clone()),
                    );
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
                let key = Some(
                    overrides
                        .get(&old.id)
                        .cloned()
                        .unwrap_or_else(|| old.parent.clone()),
                );
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
                if change
                    .after
                    .as_ref()
                    .is_none_or(|new| new.parent != old.parent)
                {
                    overrides.remove(&old.id);
                }
            }
            if let Some(new) = &change.after {
                let key = Some(
                    overrides
                        .get(&new.id)
                        .cloned()
                        .unwrap_or_else(|| new.parent.clone()),
                );
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
        // Adding/removing the last directly drawn tile can change whether a
        // directory is represented by a platform, even when its child
        // directory allocations themselves have not moved. Likewise, an
        // empty child must be removed from its parent instead of leaving a
        // drawable directory allocation with no live descendants. Both need
        // the structural transaction so geometry, lighting and owners are
        // committed together.
        let unary = |partition: &Partition<Item>| {
            let mut items = partition.entries();
            matches!(
                (items.next(), items.next()),
                (Some((Item::Directory(_), _)), None)
            )
        };
        for (key, partition) in &staged {
            let Some(path) = key else { continue };
            if !self.parents.contains_key(path) {
                continue;
            }
            let old = self.partitions.get(key).ok_or(Error::MissingPartition)?;
            if partition.entries().next().is_none() || unary(old) != unary(partition) {
                return Err(Error::Rebuild);
            }
        }
        // A removal leaves a vacant rectangle. Compact the affected partition
        // in the background so its surviving tiles can transition into place.
        // A nested directory owns its own platform and descendants; moving it
        // requires a structural relayout, so retain the vacancy in that case.
        if deletion_batch {
            for key in &touched {
                let p = staged.get_mut(key).ok_or(Error::MissingPartition)?;
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
        self.tile_parent_overrides = Arc::new(overrides);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        directory_map::PartitionItem,
        scene_model::{DirectoryId, EntryId},
    };
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

    #[test]
    fn root_child_can_allocate_in_empty_existing_partition() {
        let root = std::path::Path::new("/disk");
        let frame = rect(0.0, 100.0);
        let partition =
            Partition::from_layout(frame, std::iter::empty::<(Item, Placement)>()).unwrap();
        let original = IncrementalGeometry {
            partitions: BTreeMap::from([(Some(root.to_path_buf()), Arc::new(partition))]),
            surfaces: Arc::new(BTreeMap::from([(
                Some(root.to_path_buf()),
                Surface {
                    outer: frame,
                    platform: None,
                },
            )])),
            parents: Arc::new(BTreeMap::new()),
            tile_parent_overrides: Arc::new(BTreeMap::new()),
        };
        let (updated, allocated, donor) = original
            .allocate_root_child(root, std::path::Path::new("/disk/new"), None, 42)
            .unwrap();
        assert_eq!(allocated, frame);
        assert!(donor.is_none());
        assert!(
            original
                .parents
                .get(std::path::Path::new("/disk/new"))
                .is_none()
        );
        assert!(
            updated
                .parents
                .get(std::path::Path::new("/disk/new"))
                .is_some()
        );
    }

    #[test]
    fn exported_platform_keeps_directory_ownership_when_content_frames_coincide() {
        let platform = crate::render::Platform::new(
            rect(0.0, 100.0),
            eframe::egui::Color32::from_rgb(32, 48, 64),
            0,
            1.0,
        )
        .unwrap();
        // Unary ancestors can share a content rectangle with a descendant.
        // Ownership cannot be recovered by choosing the shallowest rectangle.
        let parts = [
            DirectoryPartition {
                path: Some("/disk".into()),
                bounds: platform.top,
                platform: None,
                vacancies: vec![],
                items: vec![],
            },
            DirectoryPartition {
                path: Some("/disk/child".into()),
                bounds: platform.top,
                platform: Some(platform),
                vacancies: vec![],
                items: vec![],
            },
        ];
        let geometry = IncrementalGeometry::from_layout(&parts, &[]).unwrap();
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![],
            directories: Arc::from([]),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let projection = crate::sparse_projection::SparseProjection::new(source, vec![]).unwrap();
        let exported = geometry
            .export_partitions(&projection, &[platform])
            .unwrap();
        assert!(
            exported
                .iter()
                .find(|part| part.path.as_deref() == Some(std::path::Path::new("/disk")))
                .unwrap()
                .platform
                .is_none()
        );
        assert_eq!(
            exported
                .iter()
                .find(|part| part.path.as_deref() == Some(std::path::Path::new("/disk/child")))
                .unwrap()
                .platform,
            Some(platform)
        );
    }

    #[test]
    fn root_aggregate_reuses_only_parent_vacancies() {
        let parts = vec![
            DirectoryPartition {
                path: None,
                bounds: rect(0.0, 100.0),
                platform: None,
                vacancies: vec![],
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory(Box::new("/".into())),
                    rect: rect(0.0, 100.0),
                    weight: 100,
                }],
            },
            DirectoryPartition {
                path: Some("/".into()),
                bounds: rect(0.0, 100.0),
                platform: None,
                vacancies: vec![],
                items: vec![
                    PartitionItem {
                        id: PartitionItemId::File(0),
                        rect: rect(0.0, 25.0),
                        weight: 25,
                    },
                    PartitionItem {
                        id: PartitionItemId::Directory(Box::new("/disk".into())),
                        rect: rect(25.0, 75.0),
                        weight: 75,
                    },
                ],
            },
            DirectoryPartition {
                path: Some("/disk".into()),
                bounds: rect(25.0, 75.0),
                platform: None,
                vacancies: vec![],
                items: vec![PartitionItem {
                    id: PartitionItemId::File(1),
                    rect: rect(25.0, 75.0),
                    weight: 75,
                }],
            },
        ];
        let original = IncrementalGeometry::from_layout(&parts, &[tile(0), tile(1)]).unwrap();
        assert!(
            original
                .allocate_scan_root_aggregate_in_vacancy(std::path::Path::new("/disk"), tile(2), 30)
                .unwrap()
                .is_none()
        );
        let (removed, hole) = original
            .remove_root_tile(std::path::Path::new("/"), tile(0))
            .unwrap();
        assert_eq!(hole, rect(0.0, 25.0));
        let (updated, frame) = removed
            .allocate_scan_root_aggregate_in_vacancy(std::path::Path::new("/disk"), tile(2), 30)
            .unwrap()
            .unwrap();
        assert_eq!(frame, hole);
        assert_eq!(
            updated.scan_root_frame(std::path::Path::new("/disk")),
            Some(rect(25.0, 75.0))
        );
        assert_eq!(
            original.scan_root_frame(std::path::Path::new("/disk")),
            updated.scan_root_frame(std::path::Path::new("/disk"))
        );

        // A root aggregate may first be placed in its content partition and
        // later reappear in a newly available synthetic-parent vacancy. Its
        // logical parent and generational identity do not change, but the
        // published layout-parent override must not survive that move.
        let aggregate = TileId::Directory(DirectoryId {
            slot: 42,
            generation: 3,
        });
        let root = std::path::Path::new("/disk");
        let parent = std::path::Path::new("/");
        let (inside, _, _) = original
            .allocate_root_child(root, root, Some(aggregate), 30)
            .unwrap();
        assert_eq!(inside.tile_layout_parent(aggregate, parent), root);
        let (without, _) = inside.remove_root_tile(root, aggregate).unwrap();
        let (with_outer_hole, _) = without.remove_root_tile(parent, tile(0)).unwrap();
        let (reborn, _) = with_outer_hole
            .allocate_scan_root_aggregate_in_vacancy(root, aggregate, 30)
            .unwrap()
            .expect("vacant synthetic parent frame");
        assert_eq!(reborn.tile_layout_parent(aggregate, parent), parent);
        assert_eq!(
            inside.tile_layout_parent(aggregate, parent),
            root,
            "old snapshot unchanged"
        );
    }
    #[test]
    fn root_aggregate_can_use_content_partition_without_changing_logical_parent() {
        let root = std::path::Path::new("/disk/a");
        let logical_parent = std::path::Path::new("/disk");
        let aggregate = TileId::Directory(DirectoryId {
            slot: 42,
            generation: 3,
        });
        let original = geometry();
        let (mut allocated, frame, donor) = original
            .allocate_root_child(root, root, Some(aggregate), 25)
            .unwrap();
        assert!(matches!(donor, Some(RootDonor::Tile(_, _, _))));
        assert!(frame.w > 0.0 && frame.h > 0.0);
        assert_eq!(
            allocated.tile_layout_parent(aggregate, logical_parent),
            root
        );
        assert_eq!(
            original.tile_layout_parent(aggregate, logical_parent),
            logical_parent
        );
        let token = AtomicU64::new(1);
        let state = |weight| TileState {
            id: aggregate,
            parent: logical_parent.into(),
            weight,
        };
        allocated
            .apply_compacting(
                &[TileDelta {
                    before: Some(state(25)),
                    after: Some(state(20)),
                }],
                1.0,
                f64::MAX,
                &token,
                1,
            )
            .unwrap();
        assert_eq!(
            allocated.tile_layout_parent(aggregate, logical_parent),
            root
        );
        let (removed, _) = allocated.remove_root_tile(root, aggregate).unwrap();
        assert_eq!(
            removed.tile_layout_parent(aggregate, logical_parent),
            logical_parent
        );
        assert_eq!(
            allocated.tile_layout_parent(aggregate, logical_parent),
            root
        );
    }
    #[test]
    fn exported_content_aggregate_recovers_layout_parent() {
        let root = std::path::Path::new("/disk/a");
        let logical_parent = std::path::Path::new("/disk");
        let aggregate = TileId::Directory(DirectoryId {
            slot: 42,
            generation: 3,
        });
        let (allocated, _, _) = geometry()
            .allocate_root_child(root, root, Some(aggregate), 25)
            .unwrap();
        let entries = [
            ("/disk/a/one", "/disk/a", 50),
            ("/disk/a/two", "/disk/a", 50),
            ("/disk/a", "/disk", 25),
        ]
        .into_iter()
        .map(|(path, parent, size)| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(std::path::Path::new(parent)),
            size,
            identity: Default::default(),
            parent_identity: Default::default(),
            parent_is_leaf: false,
            can_trash: false,
        })
        .collect();
        let source = Arc::new(scan::ScanResult {
            root: root.into(),
            files: entries,
            directories: Vec::new().into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        });
        let projection = crate::sparse_projection::SparseProjection::new(
            source,
            vec![tile(0), tile(1), aggregate],
        )
        .unwrap();
        let parts = allocated.export_partitions(&projection, &[]).unwrap();
        let ids = vec![Some(tile(0)), Some(tile(1)), Some(aggregate)];
        let mut restored = IncrementalGeometry::from_layout_optional(&parts, &ids).unwrap();
        restored
            .restore_tile_layout_parents(&parts, &projection)
            .unwrap();
        assert_eq!(restored.tile_layout_parent(aggregate, logical_parent), root);
        let (removed, _) = restored.remove_root_tile(root, aggregate).unwrap();
        assert_eq!(
            removed.tile_layout_parent(aggregate, logical_parent),
            logical_parent
        );
    }
    fn geometry() -> IncrementalGeometry {
        let parts = vec![
            DirectoryPartition {
                path: None,
                bounds: rect(0.0, 100.0),
                platform: None,
                vacancies: Vec::new(),
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory(Box::new("/disk/a".into())),
                    rect: rect(0.0, 100.0),
                    weight: 100,
                }],
            },
            DirectoryPartition {
                path: Some("/disk/a".into()),
                bounds: rect(0.0, 100.0),
                platform: None,
                vacancies: Vec::new(),
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
    fn deleting_last_tiles_requests_structural_update_without_mutation() {
        let token = AtomicU64::new(1);
        let mut geometry = geometry();
        let deltas = [
            TileDelta {
                before: Some(state(0, 50)),
                after: None,
            },
            TileDelta {
                before: Some(state(1, 50)),
                after: None,
            },
        ];
        assert!(matches!(
            geometry.apply_compacting(&deltas, 1.0, 1.0e12, &token, 1),
            Err(Error::Rebuild)
        ));
        assert_eq!(
            geometry.direct_tile_count_up_to(std::path::Path::new("/disk/a"), 2),
            Some(2)
        );
    }

    #[test]
    fn large_flat_partition_deletion_moves_only_adjacent_tile() {
        let count = 1024u32;
        let parts = vec![
            DirectoryPartition {
                path: None,
                bounds: rect(0.0, count as f32),
                platform: None,
                vacancies: vec![],
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory(Box::new("/disk/a".into())),
                    rect: rect(0.0, count as f32),
                    weight: count as u64,
                }],
            },
            DirectoryPartition {
                path: Some("/disk/a".into()),
                bounds: rect(0.0, count as f32),
                platform: None,
                vacancies: vec![],
                items: (0..count)
                    .map(|slot| PartitionItem {
                        id: PartitionItemId::File(slot as usize),
                        rect: rect(slot as f32, 1.0),
                        weight: 1,
                    })
                    .collect(),
            },
        ];
        let ids: Vec<_> = (0..count).map(tile).collect();
        let mut geometry = IncrementalGeometry::from_layout(&parts, &ids).unwrap();
        let token = AtomicU64::new(1);
        let update = geometry
            .apply_compacting(
                &[TileDelta {
                    before: Some(state(500, 1)),
                    after: None,
                }],
                1.0,
                1.0e12,
                &token,
                1,
            )
            .unwrap();
        assert_eq!(update.tiles.get(&tile(500)), Some(&None));
        assert!(
            update.tiles.len() <= 2,
            "a single vacancy must not repack unrelated entries"
        );
        assert!(!update.tiles.contains_key(&tile(0)));
        assert!(!update.tiles.contains_key(&tile(count - 1)));
        let inserted = geometry
            .apply(
                &[TileDelta {
                    before: None,
                    after: Some(state(count, 1)),
                }],
                1.0,
                1.0e12,
                &token,
                1,
            )
            .unwrap();
        assert!(
            inserted.tiles.len() <= 2,
            "a single insertion must split at most one donor"
        );
        assert!(
            inserted
                .tiles
                .get(&tile(count))
                .is_some_and(Option::is_some)
        );
    }

    #[test]
    fn mixed_partition_deletion_compacts_adjacent_tile_without_moving_directory() {
        let parts = vec![
            DirectoryPartition {
                path: None,
                bounds: rect(0.0, 100.0),
                platform: None,
                vacancies: Vec::new(),
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory(Box::new("/disk/a".into())),
                    rect: rect(0.0, 100.0),
                    weight: 100,
                }],
            },
            DirectoryPartition {
                path: Some("/disk/a".into()),
                bounds: rect(0.0, 100.0),
                platform: None,
                vacancies: Vec::new(),
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
                        id: PartitionItemId::Directory(Box::new("/disk/a/nested".into())),
                        rect: rect(50.0, 50.0),
                        weight: 50,
                    },
                ],
            },
            DirectoryPartition {
                path: Some("/disk/a/nested".into()),
                bounds: rect(50.0, 50.0),
                platform: None,
                vacancies: Vec::new(),
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
        // The last direct tile disappearing turns this directory into a unary
        // ancestor and removes a platform; the structural path must update
        // its descendants and lighting as one transaction.
        assert!(matches!(
            geometry.apply_compacting(
                &[TileDelta {
                    before: Some(state(1, 25)),
                    after: None
                }],
                1.0,
                1.0e12,
                &token,
                1
            ),
            Err(Error::Rebuild)
        ));
    }

    #[test]
    fn optional_slots_restore_vacancies_and_propagate_live_weights() {
        let parts = vec![
            DirectoryPartition {
                path: None,
                bounds: rect(0.0, 100.0),
                platform: None,
                vacancies: Vec::new(),
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory(Box::new("/disk".into())),
                    rect: rect(0.0, 100.0),
                    weight: 100,
                }],
            },
            DirectoryPartition {
                path: Some("/disk".into()),
                bounds: rect(0.0, 100.0),
                platform: None,
                vacancies: Vec::new(),
                items: vec![PartitionItem {
                    id: PartitionItemId::Directory(Box::new("/disk/a".into())),
                    rect: rect(0.0, 100.0),
                    weight: 100,
                }],
            },
            DirectoryPartition {
                path: Some("/disk/a".into()),
                bounds: rect(0.0, 100.0),
                platform: None,
                vacancies: Vec::new(),
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
                excluded: false,
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
                vacancies: Vec::new(),
                items: vec![
                    PartitionItem {
                        id: PartitionItemId::Directory(Box::new("/disk/a".into())),
                        rect: rect(0.0, 50.0),
                        weight: 100,
                    },
                    PartitionItem {
                        id: PartitionItemId::Directory(Box::new("/disk/b".into())),
                        rect: rect(50.0, 50.0),
                        weight: 100,
                    },
                ],
            },
            DirectoryPartition {
                path: Some("/disk/a".into()),
                bounds: rect(0.0, 50.0),
                platform: None,
                vacancies: Vec::new(),
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
                vacancies: Vec::new(),
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
