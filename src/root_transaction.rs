//! Atomic transactions for files and directory tiles across root branches.
//!
//! Existing branches retain their allocations and new branches reuse vacancies
//! or split one donor. The new projection is applied just once, preserving
//! generational identities and the render slots of moved files.
use crate::{
    directory_map,
    incremental_geometry::IncrementalGeometry,
    incremental_update::{self, Error, PreparedView},
    render::{self, RenderEntry, RenderMap},
    scene_actor::TileDelta,
    scene_model::TileId,
    sparse_projection::SparseProjection,
    treemap::Rect,
};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Prepare a mixed transaction across root tiles and child branches.
/// Every branch is computed against the same final projection; a root
/// aggregate is a directory tile and is handled by another structural path.
pub(crate) fn prepare(
    previous: &IncrementalGeometry,
    map: &RenderMap,
    old: &SparseProjection,
    next: &SparseProjection,
    holes: &[Rect],
    deltas: &[TileDelta],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
) -> Result<Option<PreparedView>, Error> {
    use std::collections::{BTreeSet, HashSet};
    let root = old.base().root.as_path();
    let pivot = |parent: &Path| -> Option<PathBuf> {
        let relative = parent.strip_prefix(root).ok()?;
        Some(root.join(relative.components().next()?.as_os_str()))
    };
    let mut branches = BTreeMap::<PathBuf, BTreeSet<TileId>>::new();
    let mut before_ids = HashSet::new();
    let mut after_ids = HashSet::new();
    let mut direct_before = BTreeMap::new();
    let mut direct_after = BTreeMap::new();
    let mut aggregate_before = BTreeMap::new();
    let mut aggregate_after = BTreeMap::new();
    for delta in deltas {
        if delta.before.is_none() && delta.after.is_none() {
            return Err(Error::InvalidProjection);
        }
        if let (Some(before), Some(after)) = (&delta.before, &delta.after) {
            if before.id == after.id && before.identity != after.identity {
                return Err(Error::InvalidProjection);
            }
        }
        for (tile, is_before) in delta
            .before
            .iter()
            .map(|t| (t, true))
            .chain(delta.after.iter().map(|t| (t, false)))
        {
            if tile.path.parent() != Some(tile.parent.as_path())
                && !(tile.path == root && root.parent().is_none() && tile.parent == root)
            {
                return Err(Error::InvalidProjection);
            }
            let is_root_aggregate =
                tile.path == root && tile.parent != root && matches!(tile.id, TileId::Directory(_));
            let path = if tile.parent == root || is_root_aggregate {
                None
            } else {
                let Some(path) = pivot(&tile.parent) else {
                    return Ok(None);
                };
                Some(path)
            };
            let ids = if is_before {
                &mut before_ids
            } else {
                &mut after_ids
            };
            if !ids.insert(tile.id) {
                return Err(Error::InvalidProjection);
            }
            if is_before {
                if old.slot(tile.id).is_none() {
                    return Err(Error::InvalidProjection);
                }
            } else {
                let slot = next.slot(tile.id).ok_or(Error::InvalidProjection)?;
                if old.slot(tile.id).is_some_and(|old_slot| old_slot != slot) {
                    return Err(Error::InvalidProjection);
                }
                let entry = next.get(slot).ok_or(Error::InvalidProjection)?;
                if entry.path != tile.path || entry.identity != tile.identity {
                    return Err(Error::InvalidProjection);
                }
            }
            if let Some(path) = path {
                branches.entry(path).or_default();
            } else if is_root_aggregate && is_before {
                aggregate_before.insert(tile.id, tile);
            } else if is_root_aggregate {
                aggregate_after.insert(tile.id, tile);
            } else if is_before {
                direct_before.insert(tile.id, tile);
            } else {
                direct_after.insert(tile.id, tile);
            }
        }
    }
    // Populate each branch from its previous geometry, then add all final IDs.
    // Filtering by the final parent during layout naturally handles exchanges.
    let mut frames = BTreeMap::new();
    for (path, ids) in &mut branches {
        let frame = previous.root_fold_frame(root, path, None);
        let previous_ids = previous.subtree_tiles(path);
        if frame.is_none() && !previous_ids.is_empty() {
            return Err(Error::InvalidProjection);
        }
        ids.extend(previous_ids);
        frames.insert(path.clone(), frame);
    }
    for tile in deltas.iter().filter_map(|delta| delta.after.as_ref()) {
        if tile.parent != root && !(tile.path == root && matches!(tile.id, TileId::Directory(_))) {
            let path = pivot(&tile.parent).ok_or(Error::InvalidProjection)?;
            branches
                .get_mut(&path)
                .ok_or(Error::InvalidProjection)?
                .insert(tile.id);
        }
    }
    let mut geometry = previous.clone();
    let mut changes = BTreeMap::<usize, Option<RenderEntry>>::new();
    for ids in branches.values() {
        for &id in ids {
            if let Some(slot) = old.slot(id) {
                changes.entry(slot).or_insert(None);
            }
        }
    }
    let mut platforms = map.platforms().to_vec();
    let mut dirty = Vec::new();
    let bounds = map.bounds;
    let size = map.physical_size();
    // A scan-root aggregate may have been laid out in the synthetic parent by
    // the first scan or in the root partition by an incremental transaction.
    // Preserve that physical allocation independently of its logical parent.
    let parent = root.parent().unwrap_or(root);
    for (&id, _before) in &aggregate_before {
        let slot = old.slot(id).ok_or(Error::InvalidProjection)?;
        let actual_parent = geometry.tile_layout_parent(id, parent).to_path_buf();
        if let Some(after) = aggregate_after.get(&id) {
            let (updated, frame) = geometry
                .replace_root_tile(&actual_parent, id, id, after.size.max(1))
                .map_err(Error::Geometry)?;
            geometry = updated;
            changes.insert(
                slot,
                Some(RenderEntry {
                    id: slot,
                    rect: frame,
                    color: crate::color(&after.parent),
                }),
            );
        } else {
            let (updated, frame) = geometry
                .remove_root_tile(&actual_parent, id)
                .map_err(Error::Geometry)?;
            geometry = updated;
            changes.insert(slot, None);
            dirty.push(frame);
        }
    }
    let mut aggregate_pending = Vec::new();
    for (&id, after) in &aggregate_after {
        if aggregate_before.contains_key(&id) {
            continue;
        }
        let Some((updated, frame)) = geometry
            .allocate_scan_root_aggregate_in_vacancy(root, id, after.size.max(1))
            .map_err(Error::Geometry)?
        else {
            aggregate_pending.push((root.to_path_buf(), Some(id), after.size.max(1)));
            continue;
        };
        geometry = updated;
        let slot = next.slot(id).ok_or(Error::InvalidProjection)?;
        changes.insert(
            slot,
            Some(RenderEntry {
                id: slot,
                rect: frame,
                color: crate::color(&after.parent),
            }),
        );
        dirty.push(frame);
    }
    // A folder-only scan may initially contain just the root aggregate in
    // the synthetic parent, so no root content partition exists yet. Split
    // that one retained aggregate once and create an empty root allocation;
    // the ordinary transaction below then fills it using the final projection.
    if geometry.direct_tile_count_up_to(root, 0).is_none() {
        let added_weight = deltas
            .iter()
            .filter_map(|delta| delta.after.as_ref())
            .filter(|tile| tile.path != root && tile.parent.starts_with(root))
            .fold(0u64, |total, tile| total.saturating_add(tile.size.max(1)));
        if added_weight > 0 {
            let Some(id) = geometry.lone_root_parent_tile(root) else {
                return Ok(None);
            };
            let Some(slot) = next.slot(id) else {
                return Ok(None);
            };
            let aggregate = next.get(slot).ok_or(Error::InvalidProjection)?;
            if aggregate.path != root {
                return Err(Error::InvalidProjection);
            }
            let Some((updated, _frame, donor)) = geometry
                .bootstrap_root_beside_aggregate(root, id, added_weight)
                .map_err(Error::Geometry)?
            else {
                return Ok(None);
            };
            geometry = updated;
            if let Some((old_frame, new_frame)) = donor {
                dirty.push(old_frame);
                changes.insert(
                    slot,
                    Some(RenderEntry {
                        id: slot,
                        rect: new_frame,
                        color: crate::color(&aggregate.parent),
                    }),
                );
            }
        }
    }
    // Remove changed root files first so their holes can be reused by newly
    // created root entries. Existing same-ID root files keep their frame.
    for (&id, before) in &direct_before {
        let slot = old.slot(id).ok_or(Error::InvalidProjection)?;
        if let Some(after) = direct_after.get(&id) {
            let (updated, frame) = geometry
                .replace_root_tile(root, id, id, after.size.max(1))
                .map_err(Error::Geometry)?;
            geometry = updated;
            changes.insert(
                slot,
                Some(RenderEntry {
                    id: slot,
                    rect: frame,
                    color: crate::color(&after.parent),
                }),
            );
        } else {
            let (updated, frame) = geometry
                .remove_root_tile(root, id)
                .map_err(Error::Geometry)?;
            geometry = updated;
            changes.insert(slot, None);
            dirty.push(frame);
        }
        let _ = before;
    }
    let remove_platforms = |platforms: &mut Vec<render::Platform>, frame: Rect| {
        platforms.retain(|platform| {
            let r = platform.bounds;
            !(r.x >= frame.x
                && r.y >= frame.y
                && r.x + r.w <= frame.x + frame.w + 0.01
                && r.y + r.h <= frame.y + frame.h + 0.01)
        });
    };
    fn relayout(
        geometry: &mut IncrementalGeometry,
        platforms: &mut Vec<render::Platform>,
        changes: &mut BTreeMap<usize, Option<RenderEntry>>,
        old: &SparseProjection,
        next: &SparseProjection,
        ids: &BTreeSet<TileId>,
        root: &Path,
        path: &Path,
        frame: Rect,
        bounds: Rect,
        size: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Result<bool, Error> {
        let mut groups: HashMap<&Path, Vec<(usize, u64)>> = HashMap::new();
        for (index, &id) in ids.iter().enumerate() {
            if index % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
                return Err(Error::Cancelled);
            }
            if let Some(slot) = old.slot(id) {
                changes.entry(slot).or_insert(None);
            }
            let Some(slot) = next.slot(id) else { continue };
            let entry = next.get(slot).ok_or(Error::InvalidProjection)?;
            if entry.parent.starts_with(path) {
                groups
                    .entry(entry.parent.as_ref())
                    .or_default()
                    .push((slot, entry.size.max(1)));
            }
        }
        if groups.is_empty() {
            return Ok(false);
        }
        let sub = directory_map::layout_subtree(
            groups,
            root,
            path,
            0,
            frame,
            render::logical_pixel_size(bounds, size),
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        *geometry = geometry
            .install_subtree(path, &sub.partitions, next)
            .map_err(Error::Geometry)?;
        *geometry = geometry
            .refresh_ancestor_weights(path)
            .map_err(Error::Geometry)?;
        for entry in sub.entries {
            changes.insert(entry.id, Some(entry));
        }
        platforms.extend(sub.platforms);
        Ok(true)
    }
    // Delete empty branches first: their vacancies are available to newly
    // created branches regardless of path ordering.
    for (path, ids) in &branches {
        let Some(frame) = frames[path] else { continue };
        let is_empty = !ids.iter().any(|id| {
            next.slot(*id)
                .and_then(|slot| next.get(slot))
                .is_some_and(|entry| entry.parent.starts_with(path))
        });
        if !is_empty {
            continue;
        }
        dirty.push(frame);
        remove_platforms(&mut platforms, frame);
        let (updated, tile_frame, directory_frame) = geometry
            .remove_root_child(root, path, None)
            .map_err(Error::Geometry)?;
        if tile_frame.is_some() || directory_frame != Some(frame) {
            return Err(Error::InvalidProjection);
        }
        geometry = updated;
    }
    for (path, ids) in &branches {
        let Some(frame) = frames[path] else { continue };
        if !ids.iter().any(|id| {
            next.slot(*id)
                .and_then(|slot| next.get(slot))
                .is_some_and(|entry| entry.parent.starts_with(path))
        }) {
            continue;
        }
        dirty.push(frame);
        remove_platforms(&mut platforms, frame);
        if !relayout(
            &mut geometry,
            &mut platforms,
            &mut changes,
            old,
            next,
            ids,
            root,
            path,
            frame,
            bounds,
            size,
            token,
            generation,
        )? {
            return Err(Error::InvalidProjection);
        }
    }
    let mut allocations: Vec<(PathBuf, Option<TileId>, u64)> = Vec::new();
    for (path, ids) in &branches {
        if frames[path].is_some() {
            continue;
        }
        let weight = ids
            .iter()
            .filter_map(|id| next.slot(*id).and_then(|slot| next.get(slot)))
            .filter(|entry| entry.parent.starts_with(path))
            .fold(0u64, |sum, entry| sum.saturating_add(entry.size.max(1)));
        if weight != 0 {
            allocations.push((path.clone(), None, weight));
        }
    }
    for (&id, tile) in &direct_after {
        if !direct_before.contains_key(&id) {
            allocations.push((tile.path.clone(), Some(id), tile.size.max(1)));
        }
    }
    allocations.extend(aggregate_pending);
    for (path, direct, weight) in allocations {
        let (updated, frame, donor) =
            match geometry.allocate_root_child(root, &path, direct, weight) {
                Ok(value) => value,
                Err(
                    crate::incremental_geometry::Error::MissingPartition
                    | crate::incremental_geometry::Error::InvalidDelta,
                ) => return Ok(None),
                Err(error) => return Err(Error::Geometry(error)),
            };
        geometry = updated;
        if let Some(donor) = donor {
            use crate::incremental_geometry::RootDonor;
            match donor {
                RootDonor::Tile(id, old_frame, new_frame) => {
                    dirty.push(old_frame);
                    let slot = next.slot(id).ok_or(Error::InvalidProjection)?;
                    let tile = next.get(slot).ok_or(Error::InvalidProjection)?;
                    changes.insert(
                        slot,
                        Some(RenderEntry {
                            id: slot,
                            rect: new_frame,
                            color: crate::color(&tile.parent),
                        }),
                    );
                }
                RootDonor::Directory(donor_path, old_frame, new_frame) => {
                    dirty.push(old_frame);
                    remove_platforms(&mut platforms, old_frame);
                    let donor_ids = branches.get(&donor_path).cloned().unwrap_or_else(|| {
                        previous.subtree_tiles(&donor_path).into_iter().collect()
                    });
                    if !relayout(
                        &mut geometry,
                        &mut platforms,
                        &mut changes,
                        old,
                        next,
                        &donor_ids,
                        root,
                        &donor_path,
                        new_frame,
                        bounds,
                        size,
                        token,
                        generation,
                    )? {
                        return Err(Error::InvalidProjection);
                    }
                }
            }
        }
        dirty.push(frame);
        if let Some(id) = direct {
            let slot = next.slot(id).ok_or(Error::InvalidProjection)?;
            let entry = next.get(slot).ok_or(Error::InvalidProjection)?;
            changes.insert(
                slot,
                Some(RenderEntry {
                    id: slot,
                    rect: frame,
                    color: crate::color(&entry.parent),
                }),
            );
        } else {
            let ids = branches.get(&path).ok_or(Error::InvalidProjection)?;
            if !relayout(
                &mut geometry,
                &mut platforms,
                &mut changes,
                old,
                next,
                ids,
                root,
                &path,
                frame,
                bounds,
                size,
                token,
                generation,
            )? {
                return Err(Error::InvalidProjection);
            }
        }
    }
    if dirty.is_empty() {
        return Ok(Some(PreparedView {
            geometry,
            projection: next.clone(),
            map: map.snapshot_for_update(),
            patches: Vec::new(),
            holes: holes.to_vec(),
            transitions: Vec::new(),
        }));
    }
    incremental_update::finish_local_regions_with_animation(
        map,
        next,
        holes,
        geometry,
        changes.into_iter().collect(),
        platforms,
        &dirty,
        max_patch_pixels,
        token,
        generation,
        animate,
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{scan, scene_model::Scene};
    use std::{sync::Arc, time::Duration};

    #[test]
    fn updates_scan_root_aggregate_and_child_in_one_transaction() {
        let token = AtomicU64::new(1);
        let ident = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let file = |path: &str, parent: &str, id, parent_id| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: ident(id),
            parent_identity: ident(parent_id),
            parent_is_leaf: true,
            can_trash: true,
        };
        let fact = |path: &str, id, children| scan::DirectoryFact {
            path: path.into(),
            identity: ident(id),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/root", "/disk", 2, 1),
                file("/disk/a/f", "/disk/a", 4, 3),
                file("/disk/b/f", "/disk/b", 6, 5),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 3, false),
                fact("/disk/b", 5, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let folder = scene
            .tile(TileId::Directory(
                scene.directory_id(Path::new("/disk")).unwrap(),
            ))
            .unwrap();
        let a = scene
            .tile(TileId::File(scene.file_id(Path::new("/disk/a/f")).unwrap()))
            .unwrap();
        let b = scene
            .tile(TileId::File(scene.file_id(Path::new("/disk/b/f")).unwrap()))
            .unwrap();
        let tiles = [folder.clone(), a.clone(), b.clone()];
        let projected = Arc::new(scan::ScanResult {
            root: source.root.clone(),
            files: tiles.iter().map(|t| t.to_entry()).collect(),
            directories: source.directories.clone(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let old = SparseProjection::new(projected, tiles.iter().map(|t| t.id).collect()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 300.0,
            h: 200.0,
        };
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/"), vec![(0, folder.size)]),
                (Path::new("/disk/a"), vec![(1, a.size)]),
                (Path::new("/disk/b"), vec![(2, b.size)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(
            &layout.partitions,
            &tiles.iter().map(|t| t.id).collect::<Vec<_>>(),
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [300, 200],
            &token,
            1,
        )
        .unwrap();
        let neighbor = map.rect(old.slot(b.id).unwrap());
        let mut folder_after = folder.clone();
        folder_after.size += 30;
        let mut a_after = a.clone();
        a_after.size += 30;
        let mut next = old.clone();
        next.replace(
            &[folder.id, a.id],
            vec![
                (folder.id, folder_after.to_entry()),
                (a.id, a_after.to_entry()),
            ],
        )
        .unwrap();
        let deltas = [
            TileDelta {
                before: Some(folder),
                after: Some(folder_after),
            },
            TileDelta {
                before: Some(a),
                after: Some(a_after),
            },
        ];
        let updated = prepare(
            &geometry,
            &map,
            &old,
            &next,
            &[],
            &deltas,
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .expect("mixed root aggregate");
        let batched = prepare(
            &geometry,
            &map,
            &old,
            &next,
            &[],
            &deltas,
            4096,
            &token,
            1,
            false,
        )
        .unwrap()
        .expect("batched root aggregate");
        assert!(batched.transitions.is_empty());
        assert_eq!(
            batched.map.rect(next.slot(b.id).unwrap()),
            updated.map.rect(next.slot(b.id).unwrap())
        );
        assert_eq!(updated.map.rect(next.slot(b.id).unwrap()), neighbor);
        let live: Vec<_> = updated
            .map
            .entries()
            .iter()
            .copied()
            .filter(|entry| updated.map.rect(entry.id).is_some())
            .collect();
        let full = render::raster_patch(
            &live,
            updated.map.platforms(),
            bounds,
            [300, 200],
            [0, 0, 300, 200],
            &token,
            1,
        )
        .unwrap();
        for patch in &updated.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        full.image.pixels[gy * 300 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), full.owner_at(gx, gy));
                }
            }
        }
        for patch in &batched.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        full.image.pixels[gy * 300 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), full.owner_at(gx, gy));
                }
            }
        }
    }

    #[test]
    fn mixes_multiple_directory_folds_expansions_and_root_tiles_atomically() {
        let token = AtomicU64::new(1);
        let ident = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let fact = |path: &str, id, children| scan::DirectoryFact {
            path: path.into(),
            identity: ident(id),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        let file = |path: &str, parent: &str, id, parent_id| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: ident(id),
            parent_identity: ident(parent_id),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/a", "/disk/a", 3, 2),
                file("/disk/b/b", "/disk/b", 5, 4),
                file("/disk/c/c", "/disk/c", 7, 6),
                file("/disk/d/d", "/disk/d", 9, 8),
                file("/disk/r", "/disk", 10, 1),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, false),
                fact("/disk/b", 4, false),
                fact("/disk/c", 6, false),
                fact("/disk/d", 8, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let f = |p: &str| {
            scene
                .tile(TileId::File(scene.file_id(Path::new(p)).unwrap()))
                .unwrap()
        };
        let d = |p: &str| {
            scene
                .tile(TileId::Directory(scene.directory_id(Path::new(p)).unwrap()))
                .unwrap()
        };
        let (a, b, c, neighbor, root_file) = (
            d("/disk/a"),
            f("/disk/b/b"),
            d("/disk/c"),
            f("/disk/d/d"),
            f("/disk/r"),
        );
        let (new_a, new_b) = (f("/disk/a/a"), d("/disk/b"));
        let tiles = [
            a.clone(),
            b.clone(),
            c.clone(),
            neighbor.clone(),
            root_file.clone(),
        ];
        let projected = Arc::new(scan::ScanResult {
            root: source.root.clone(),
            files: tiles.iter().map(|t| t.to_entry()).collect(),
            directories: source.directories.clone(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let old = SparseProjection::new(projected, tiles.iter().map(|t| t.id).collect()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 300.0,
        };
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk"), vec![(0, 50), (2, 50), (4, 50)]),
                (Path::new("/disk/b"), vec![(1, 50)]),
                (Path::new("/disk/d"), vec![(3, 50)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(
            &layout.partitions,
            &tiles.iter().map(|t| t.id).collect::<Vec<_>>(),
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [600, 300],
            &token,
            1,
        )
        .unwrap();
        let neighbor_rect = map.rect(old.slot(neighbor.id).unwrap());
        let mut next = old.clone();
        next.replace(
            &[a.id, b.id, root_file.id],
            vec![(new_a.id, new_a.to_entry()), (new_b.id, new_b.to_entry())],
        )
        .unwrap();
        let deltas = [
            TileDelta {
                before: Some(a),
                after: None,
            },
            TileDelta {
                before: Some(b),
                after: None,
            },
            TileDelta {
                before: Some(root_file),
                after: None,
            },
            TileDelta {
                before: None,
                after: Some(new_a.clone()),
            },
            TileDelta {
                before: None,
                after: Some(new_b.clone()),
            },
        ];
        let updated = prepare(
            &geometry,
            &map,
            &old,
            &next,
            &[],
            &deltas,
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .expect("generic transaction");
        assert_eq!(
            updated.map.rect(next.slot(neighbor.id).unwrap()),
            neighbor_rect
        );
        assert!(updated.map.rect(next.slot(new_a.id).unwrap()).is_some());
        assert!(updated.map.rect(next.slot(new_b.id).unwrap()).is_some());
        let live: Vec<_> = updated
            .map
            .entries()
            .iter()
            .copied()
            .filter(|entry| updated.map.rect(entry.id).is_some())
            .collect();
        let full = render::raster_patch(
            &live,
            updated.map.platforms(),
            bounds,
            [600, 300],
            [0, 0, 600, 300],
            &token,
            1,
        )
        .unwrap();
        for patch in &updated.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        full.image.pixels[gy * 600 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), full.owner_at(gx, gy));
                }
            }
        }
    }
}

#[cfg(test)]
mod previous_move_tests {
    use super::*;
    use crate::{
        scan,
        scene_model::{Scene, TileId},
        treemap::Rect,
    };
    use std::{sync::Arc, time::Duration};

    #[test]
    fn moving_last_file_preserves_slot_neighbor_and_pixels() {
        let token = AtomicU64::new(1);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let file = |path: &str, parent: &str, id, dir| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: identity(id),
            parent_identity: identity(dir),
            parent_is_leaf: true,
            can_trash: true,
        };
        let fact = |path: &str, id, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/f", "/disk/a", 3, 2),
                file("/disk/b/g", "/disk/b", 5, 4),
                file("/disk/c/h", "/disk/c", 7, 6),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, false),
                fact("/disk/b", 4, false),
                fact("/disk/c", 6, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let tiles: Vec<_> = source
            .files
            .iter()
            .map(|file| {
                scene
                    .tile(TileId::File(scene.file_id(&file.path).unwrap()))
                    .unwrap()
            })
            .collect();
        let projection =
            SparseProjection::new(source, tiles.iter().map(|tile| tile.id).collect()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 300.0,
        };
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk/a"), vec![(0, 50)]),
                (Path::new("/disk/b"), vec![(1, 50)]),
                (Path::new("/disk/c"), vec![(2, 50)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(
            &layout.partitions,
            &tiles.iter().map(|t| t.id).collect::<Vec<_>>(),
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [600, 300],
            &token,
            1,
        )
        .unwrap();
        let stable = map.rect(2);
        let mut moved = tiles[0].clone();
        moved.path = "/disk/b/f".into();
        moved.parent = "/disk/b".into();
        moved.parent_identity = identity(4);
        let mut next = projection.clone();
        next.replace(&[tiles[0].id], vec![(moved.id, moved.to_entry())])
            .unwrap();
        let updated = prepare(
            &geometry,
            &map,
            &projection,
            &next,
            &[],
            &[TileDelta {
                before: Some(tiles[0].clone()),
                after: Some(moved.clone()),
            }],
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated.projection.slot(moved.id), Some(0));
        assert_eq!(updated.map.rect(2), stable);
        assert!(
            updated
                .geometry
                .root_fold_frame(Path::new("/disk"), Path::new("/disk/a"), None)
                .is_none()
        );
        assert!(updated.map.rect(0).is_some());
        // With a large enough patch budget, moving objects use sprites. Every
        // dirty region must exclude the same sprites from both old and new
        // backgrounds, so a sprite crossing regions is painted exactly once.
        let animated = prepare(
            &geometry,
            &map,
            &projection,
            &next,
            &[],
            &[TileDelta {
                before: Some(tiles[0].clone()),
                after: Some(moved.clone()),
            }],
            1_000_000,
            &token,
            1,
            true,
        )
        .unwrap()
        .unwrap();
        let selected: std::collections::HashSet<_> = animated
            .transitions
            .iter()
            .flat_map(|t| t.tiles.iter().map(|tile| tile.id))
            .collect();
        assert!(!animated.transitions.is_empty());
        assert!(selected.contains(&0));
        assert_eq!(
            animated
                .transitions
                .iter()
                .flat_map(|t| &t.tiles)
                .filter(|tile| tile.id == 0)
                .count(),
            1
        );
        for transition in &animated.transitions {
            let [x, y] = transition.origin;
            let [w, h] = transition.previous.size;
            let region = [x, y, x + w, y + h];
            let mut old_patch = None;
            map.raster_index()
                .for_each_patch_filtered(
                    region,
                    1_000_000,
                    &token,
                    1,
                    |entry| map.rect(entry.id).is_some() && !selected.contains(&entry.id),
                    |patch| {
                        old_patch = Some(patch);
                        true
                    },
                )
                .unwrap();
            assert_eq!(transition.previous.pixels, old_patch.unwrap().image.pixels);
            let mut new_patch = None;
            animated
                .map
                .raster_index()
                .for_each_patch_filtered(
                    region,
                    1_000_000,
                    &token,
                    1,
                    |entry| !selected.contains(&entry.id),
                    |patch| {
                        new_patch = Some(patch);
                        true
                    },
                )
                .unwrap();
            assert_eq!(
                transition.background.as_ref().unwrap().pixels,
                new_patch.unwrap().image.pixels
            );
        }
        let live: Vec<_> = updated
            .map
            .entries()
            .iter()
            .copied()
            .filter(|entry| updated.map.rect(entry.id).is_some())
            .collect();
        let full = render::raster_patch(
            &live,
            updated.map.platforms(),
            bounds,
            [600, 300],
            [0, 0, 600, 300],
            &token,
            1,
        )
        .unwrap();
        for patch in &updated.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        full.image.pixels[gy * 600 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), full.owner_at(gx, gy));
                }
            }
        }
    }

    #[test]
    fn moving_into_new_root_child_reuses_hole_or_splits_one_donor() {
        let token = AtomicU64::new(1);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let file = |path: &str, parent: &str, id, dir| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: identity(id),
            parent_identity: identity(dir),
            parent_is_leaf: true,
            can_trash: true,
        };
        let fact = |path: &str, id, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        for source_has_other_file in [false, true] {
            let mut files = vec![
                file("/disk/a/f", "/disk/a", 3, 2),
                file("/disk/b/g", "/disk/b", 5, 4),
                file("/disk/c/h", "/disk/c", 7, 6),
            ];
            if source_has_other_file {
                files.push(file("/disk/a/other", "/disk/a", 9, 2));
            }
            let source = Arc::new(scan::ScanResult {
                root: "/disk".into(),
                files,
                directories: vec![
                    fact("/disk", 1, true),
                    fact("/disk/a", 2, false),
                    fact("/disk/b", 4, false),
                    fact("/disk/c", 6, false),
                    fact("/disk/d", 8, false),
                ]
                .into(),
                skipped: 0,
                errors: vec![],
                elapsed: Duration::ZERO,
            });
            let scene = Scene::from_scan(source.clone());
            let tiles: Vec<_> = source
                .files
                .iter()
                .map(|file| {
                    scene
                        .tile(TileId::File(scene.file_id(&file.path).unwrap()))
                        .unwrap()
                })
                .collect();
            let projection =
                SparseProjection::new(source, tiles.iter().map(|tile| tile.id).collect()).unwrap();
            let bounds = Rect {
                x: 0.0,
                y: 0.0,
                w: 600.0,
                h: 300.0,
            };
            let mut a = vec![(0, 50)];
            if source_has_other_file {
                a.push((3, 50));
            }
            let layout = directory_map::layout_with_partitions(
                [
                    (Path::new("/disk/a"), a),
                    (Path::new("/disk/b"), vec![(1, 50)]),
                    (Path::new("/disk/c"), vec![(2, 50)]),
                ],
                Path::new("/disk"),
                bounds,
                1.0,
                &token,
                1,
            )
            .unwrap();
            let geometry = IncrementalGeometry::from_layout(
                &layout.partitions,
                &tiles.iter().map(|t| t.id).collect::<Vec<_>>(),
            )
            .unwrap();
            let map = RenderMap::build_with_platforms(
                layout.entries,
                layout.platforms,
                bounds,
                [600, 300],
                &token,
                1,
            )
            .unwrap();
            let old_frames: Vec<_> = (0..3).map(|i| map.rect(i)).collect();
            let mut moved = tiles[0].clone();
            moved.path = "/disk/d/f".into();
            moved.parent = "/disk/d".into();
            moved.parent_identity = identity(8);
            let mut next = projection.clone();
            next.replace(&[moved.id], vec![(moved.id, moved.to_entry())])
                .unwrap();
            let updated = prepare(
                &geometry,
                &map,
                &projection,
                &next,
                &[],
                &[TileDelta {
                    before: Some(tiles[0].clone()),
                    after: Some(moved.clone()),
                }],
                4096,
                &token,
                1,
                true,
            )
            .unwrap()
            .unwrap();
            assert_eq!(updated.projection.slot(moved.id), Some(0));
            assert!(updated.map.rect(0).is_some());
            if !source_has_other_file {
                assert_eq!(updated.map.rect(1), old_frames[1]);
                assert_eq!(updated.map.rect(2), old_frames[2]);
            } else {
                assert!((1..3).any(|i| updated.map.rect(i) == old_frames[i]));
            }
            let live: Vec<_> = updated
                .map
                .entries()
                .iter()
                .copied()
                .filter(|entry| updated.map.rect(entry.id).is_some())
                .collect();
            let full = render::raster_patch(
                &live,
                updated.map.platforms(),
                bounds,
                [600, 300],
                [0, 0, 600, 300],
                &token,
                1,
            )
            .unwrap();
            for patch in &updated.patches {
                for y in 0..patch.image.size[1] {
                    for x in 0..patch.image.size[0] {
                        let gx = patch.origin[0] + x;
                        let gy = patch.origin[1] + y;
                        assert_eq!(
                            patch.image.pixels[y * patch.image.size[0] + x],
                            full.image.pixels[gy * 600 + gx]
                        );
                        assert_eq!(patch.owner_at(x, y), full.owner_at(gx, gy));
                    }
                }
            }
        }
    }

    #[test]
    fn batch_exchanges_and_new_branches_keep_final_owners_and_slots() {
        let token = AtomicU64::new(1);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let file = |path: &str, parent: &str, id, dir, size| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size,
            identity: identity(id),
            parent_identity: identity(dir),
            parent_is_leaf: true,
            can_trash: true,
        };
        let fact = |path: &str, id, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/f", "/disk/a", 3, 2, 1000),
                file("/disk/a/other", "/disk/a", 4, 2, 5000),
                file("/disk/b/g", "/disk/b", 6, 5, 20),
                file("/disk/c/h", "/disk/c", 8, 7, 20),
                file("/disk/z/k", "/disk/z", 10, 9, 20),
                file("/disk/b/old", "/disk/b", 13, 5, 10),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, false),
                fact("/disk/b", 5, false),
                fact("/disk/c", 7, false),
                fact("/disk/z", 9, false),
                fact("/disk/d", 11, false),
                fact("/disk/e", 12, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let tiles: Vec<_> = source
            .files
            .iter()
            .map(|file| {
                scene
                    .tile(TileId::File(scene.file_id(&file.path).unwrap()))
                    .unwrap()
            })
            .collect();
        let projection =
            SparseProjection::new(source, tiles.iter().map(|tile| tile.id).collect()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 300.0,
        };
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk/a"), vec![(0, 1000), (1, 5000)]),
                (Path::new("/disk/b"), vec![(2, 20), (5, 10)]),
                (Path::new("/disk/c"), vec![(3, 20)]),
                (Path::new("/disk/z"), vec![(4, 20)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(
            &layout.partitions,
            &tiles.iter().map(|t| t.id).collect::<Vec<_>>(),
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [600, 300],
            &token,
            1,
        )
        .unwrap();
        let stable = map.rect(4);
        let old_a = geometry
            .root_fold_frame(Path::new("/disk"), Path::new("/disk/a"), None)
            .unwrap();
        let changes = [
            (0, "/disk/b/f", "/disk/b", 5),
            (2, "/disk/a/g", "/disk/a", 2),
            (1, "/disk/d/other", "/disk/d", 11),
            (3, "/disk/e/h", "/disk/e", 12),
        ];
        let mut deltas: Vec<_> = changes
            .iter()
            .map(|&(i, path, parent, parent_id)| {
                let mut new = tiles[i].clone();
                new.path = path.into();
                new.parent = parent.into();
                new.parent_identity = identity(parent_id);
                TileDelta {
                    before: Some(tiles[i].clone()),
                    after: Some(new),
                }
            })
            .collect();
        let mut added = tiles[2].clone();
        added.id = TileId::File(crate::scene_model::EntryId {
            slot: 123,
            generation: 0,
        });
        added.path = "/disk/b/new".into();
        added.identity = identity(14);
        deltas.push(TileDelta {
            before: Some(tiles[5].clone()),
            after: None,
        });
        deltas.push(TileDelta {
            before: None,
            after: Some(added.clone()),
        });
        let mut next = projection.clone();
        next.replace(
            &deltas
                .iter()
                .filter_map(|delta| delta.before.as_ref().map(|tile| tile.id))
                .collect::<Vec<_>>(),
            deltas
                .iter()
                .filter_map(|delta| delta.after.as_ref().map(|tile| (tile.id, tile.to_entry())))
                .collect(),
        )
        .unwrap();
        let updated = prepare(
            &geometry,
            &map,
            &projection,
            &next,
            &[],
            &deltas,
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .unwrap();
        for &(index, _, _, _) in &changes {
            assert_eq!(updated.projection.slot(tiles[index].id), Some(index));
            assert!(updated.map.rect(index).is_some());
        }
        assert_eq!(updated.map.rect(4), stable);
        assert!(updated.projection.slot(tiles[5].id).is_none());
        assert!(updated.map.rect(5).is_none());
        assert!(
            updated
                .map
                .rect(updated.projection.slot(added.id).unwrap())
                .is_some()
        );
        let new_a = updated
            .geometry
            .root_fold_frame(Path::new("/disk"), Path::new("/disk/a"), None)
            .unwrap();
        assert_ne!(
            new_a, old_a,
            "an already updated large branch must be the split donor"
        );
        assert!(
            updated
                .geometry
                .root_fold_frame(Path::new("/disk"), Path::new("/disk/c"), None)
                .is_none()
        );
        let live: Vec<_> = updated
            .map
            .entries()
            .iter()
            .copied()
            .filter(|entry| updated.map.rect(entry.id).is_some())
            .collect();
        let full = render::raster_patch(
            &live,
            updated.map.platforms(),
            bounds,
            [600, 300],
            [0, 0, 600, 300],
            &token,
            1,
        )
        .unwrap();
        for patch in &updated.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        full.image.pixels[gy * 600 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), full.owner_at(gx, gy));
                }
            }
        }
    }

    #[test]
    fn mixed_root_files_and_subtrees_are_one_local_transaction() {
        let token = AtomicU64::new(1);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let file = |path: &str, parent: &str, id, dir| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: identity(id),
            parent_identity: identity(dir),
            parent_is_leaf: true,
            can_trash: true,
        };
        let fact = |path: &str, id, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/r", "/disk", 2, 1),
                file("/disk/a/f", "/disk/a", 4, 3),
                file("/disk/b/g", "/disk/b", 6, 5),
                file("/disk/c/h", "/disk/c", 8, 7),
                file("/disk/old", "/disk", 9, 1),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 3, false),
                fact("/disk/b", 5, false),
                fact("/disk/c", 7, false),
                fact("/disk/d", 10, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let tiles: Vec<_> = source
            .files
            .iter()
            .map(|file| {
                scene
                    .tile(TileId::File(scene.file_id(&file.path).unwrap()))
                    .unwrap()
            })
            .collect();
        let projection =
            SparseProjection::new(source, tiles.iter().map(|tile| tile.id).collect()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 300.0,
        };
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk"), vec![(0, 50), (4, 50)]),
                (Path::new("/disk/a"), vec![(1, 50)]),
                (Path::new("/disk/b"), vec![(2, 50)]),
                (Path::new("/disk/c"), vec![(3, 50)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(
            &layout.partitions,
            &tiles.iter().map(|t| t.id).collect::<Vec<_>>(),
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [600, 300],
            &token,
            1,
        )
        .unwrap();
        let stable = map.rect(3);
        let mut a = tiles[0].clone();
        a.path = "/disk/a/r".into();
        a.parent = "/disk/a".into();
        a.parent_identity = identity(3);
        let mut r = tiles[1].clone();
        r.path = "/disk/f".into();
        r.parent = "/disk".into();
        r.parent_identity = identity(1);
        let mut d = tiles[2].clone();
        d.path = "/disk/d/g".into();
        d.parent = "/disk/d".into();
        d.parent_identity = identity(10);
        let mut added = tiles[4].clone();
        added.id = TileId::File(crate::scene_model::EntryId {
            slot: 123,
            generation: 0,
        });
        added.path = "/disk/new".into();
        added.identity = identity(11);
        let deltas = vec![
            TileDelta {
                before: Some(tiles[0].clone()),
                after: Some(a),
            },
            TileDelta {
                before: Some(tiles[1].clone()),
                after: Some(r),
            },
            TileDelta {
                before: Some(tiles[2].clone()),
                after: Some(d),
            },
            TileDelta {
                before: Some(tiles[4].clone()),
                after: None,
            },
            TileDelta {
                before: None,
                after: Some(added.clone()),
            },
        ];
        let mut next = projection.clone();
        next.replace(
            &deltas
                .iter()
                .filter_map(|delta| delta.before.as_ref().map(|tile| tile.id))
                .collect::<Vec<_>>(),
            deltas
                .iter()
                .filter_map(|delta| delta.after.as_ref().map(|tile| (tile.id, tile.to_entry())))
                .collect(),
        )
        .unwrap();
        let updated = prepare(
            &geometry,
            &map,
            &projection,
            &next,
            &[],
            &deltas,
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .unwrap();
        for i in 0..3 {
            assert_eq!(updated.projection.slot(tiles[i].id), Some(i));
            assert!(updated.map.rect(i).is_some());
        }
        assert_eq!(updated.map.rect(3), stable);
        assert!(updated.projection.slot(tiles[4].id).is_none());
        assert!(updated.map.rect(4).is_none());
        assert!(
            updated
                .map
                .rect(updated.projection.slot(added.id).unwrap())
                .is_some()
        );
        let live: Vec<_> = updated
            .map
            .entries()
            .iter()
            .copied()
            .filter(|entry| updated.map.rect(entry.id).is_some())
            .collect();
        let full = render::raster_patch(
            &live,
            updated.map.platforms(),
            bounds,
            [600, 300],
            [0, 0, 600, 300],
            &token,
            1,
        )
        .unwrap();
        for patch in &updated.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        full.image.pixels[gy * 600 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), full.owner_at(gx, gy));
                }
            }
        }
    }

    #[test]
    fn disjoint_regions_share_one_sprite_and_exclude_it_from_all_backgrounds() {
        let token = AtomicU64::new(1);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let file = |path: &str, id| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new("/disk")),
            size: 50,
            identity: identity(id),
            parent_identity: identity(1),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/a", 2), file("/disk/b", 3)],
            directories: vec![scan::DirectoryFact {
                path: "/disk".into(),
                identity: identity(1),
                enumeration_complete: true,
                excluded: false,
                has_subdirectories: false,
            }]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let tiles: Vec<_> = source
            .files
            .iter()
            .map(|file| {
                scene
                    .tile(TileId::File(scene.file_id(&file.path).unwrap()))
                    .unwrap()
            })
            .collect();
        let projection =
            SparseProjection::new(source, tiles.iter().map(|tile| tile.id).collect()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 300.0,
        };
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk"), vec![(0, 50), (1, 50)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(
            &layout.partitions,
            &tiles.iter().map(|tile| tile.id).collect::<Vec<_>>(),
        )
        .unwrap();
        let from = Rect {
            x: 20.0,
            y: 20.0,
            w: 60.0,
            h: 60.0,
        };
        let to = Rect {
            x: 460.0,
            y: 20.0,
            w: 60.0,
            h: 60.0,
        };
        let stationary = Rect {
            x: 270.0,
            y: 20.0,
            w: 60.0,
            h: 60.0,
        };
        let color = crate::color(Path::new("/disk"));
        let map = RenderMap::build_with_platforms(
            vec![
                RenderEntry {
                    id: 0,
                    rect: from,
                    color,
                },
                RenderEntry {
                    id: 1,
                    rect: stationary,
                    color,
                },
            ],
            vec![],
            bounds,
            [600, 300],
            &token,
            1,
        )
        .unwrap();
        let updated = incremental_update::finish_local_regions(
            &map,
            &projection,
            &[],
            geometry,
            vec![(
                0,
                Some(RenderEntry {
                    id: 0,
                    rect: to,
                    color,
                }),
            )],
            vec![],
            &[from, to],
            1_000_000,
            &token,
            1,
        )
        .unwrap();
        assert_eq!(updated.transitions.len(), 2);
        let selected: std::collections::HashSet<_> = updated
            .transitions
            .iter()
            .flat_map(|transition| transition.tiles.iter().map(|tile| tile.id))
            .collect();
        assert!(selected.contains(&0));
        assert_eq!(
            updated
                .transitions
                .iter()
                .flat_map(|transition| &transition.tiles)
                .filter(|tile| tile.id == 0)
                .count(),
            1
        );
        for transition in &updated.transitions {
            let [x, y] = transition.origin;
            let [w, h] = transition.previous.size;
            let region = [x, y, x + w, y + h];
            let mut before = None;
            map.raster_index()
                .for_each_patch_filtered(
                    region,
                    1_000_000,
                    &token,
                    1,
                    |entry| !selected.contains(&entry.id),
                    |patch| {
                        before = Some(patch);
                        true
                    },
                )
                .unwrap();
            assert_eq!(transition.previous.pixels, before.unwrap().image.pixels);
            let mut after = None;
            updated
                .map
                .raster_index()
                .for_each_patch_filtered(
                    region,
                    1_000_000,
                    &token,
                    1,
                    |entry| !selected.contains(&entry.id),
                    |patch| {
                        after = Some(patch);
                        true
                    },
                )
                .unwrap();
            assert_eq!(
                transition.background.as_ref().unwrap().pixels,
                after.unwrap().image.pixels
            );
        }
    }
}

#[cfg(test)]
mod previous_subtree_tests {
    use super::*;
    use crate::{
        scan,
        scene_model::{Scene, TileId},
    };
    use std::{sync::Arc, time::Duration};

    #[test]
    fn adds_nested_file_within_original_branch_and_preserves_neighbor() {
        let token = AtomicU64::new(1);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let file = |path: &str, parent: &str, id, dir| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: identity(id),
            parent_identity: identity(dir),
            parent_is_leaf: true,
            can_trash: true,
        };
        let fact = |path: &str, id, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/f", "/disk/a", 3, 2),
                file("/disk/b/g", "/disk/b", 5, 4),
                file("/disk/a/n/h", "/disk/a/n", 7, 6),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, true),
                fact("/disk/b", 4, false),
                fact("/disk/a/n", 6, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let tiles: Vec<_> = source
            .files
            .iter()
            .map(|file| {
                scene
                    .tile(TileId::File(scene.file_id(&file.path).unwrap()))
                    .unwrap()
            })
            .collect();
        let projection = SparseProjection::new_optional(
            source,
            vec![Some(tiles[0].id), Some(tiles[1].id), None],
        )
        .unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 300.0,
        };
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk/a"), vec![(0, 50)]),
                (Path::new("/disk/b"), vec![(1, 50)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry =
            IncrementalGeometry::from_layout(&layout.partitions, &[tiles[0].id, tiles[1].id])
                .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [600, 300],
            &token,
            1,
        )
        .unwrap();
        let neighbor = map.rect(1);
        let mut next = projection.clone();
        next.replace(&[], vec![(tiles[2].id, tiles[2].to_entry())])
            .unwrap();
        let updated = prepare(
            &geometry,
            &map,
            &projection,
            &next,
            &[],
            &[TileDelta {
                before: None,
                after: Some(tiles[2].clone()),
            }],
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .unwrap();
        assert_eq!(updated.map.rect(1), neighbor);
        assert!(updated.map.rect(next.slot(tiles[2].id).unwrap()).is_some());
        assert!(
            updated
                .patches
                .iter()
                .map(|p| p.image.size[0] * p.image.size[1])
                .sum::<usize>()
                < 600 * 300
        );
        let live: Vec<_> = updated
            .map
            .entries()
            .iter()
            .copied()
            .filter(|entry| updated.map.rect(entry.id).is_some())
            .collect();
        let full = render::raster_patch(
            &live,
            updated.map.platforms(),
            bounds,
            [600, 300],
            [0, 0, 600, 300],
            &token,
            1,
        )
        .unwrap();
        let previous_live: Vec<_> = map
            .entries()
            .iter()
            .copied()
            .filter(|entry| map.rect(entry.id).is_some())
            .collect();
        let previous_full = render::raster_patch(
            &previous_live,
            map.platforms(),
            bounds,
            [600, 300],
            [0, 0, 600, 300],
            &token,
            1,
        )
        .unwrap();
        let mut covered = vec![false; 600 * 300];
        for patch in &updated.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    covered[gy * 600 + gx] = true;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        full.image.pixels[gy * 600 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), full.owner_at(gx, gy));
                }
            }
        }
        for (index, &is_covered) in covered.iter().enumerate() {
            if !is_covered {
                assert_eq!(previous_full.image.pixels[index], full.image.pixels[index]);
                assert_eq!(
                    previous_full.owner_at(index % 600, index / 600),
                    full.owner_at(index % 600, index / 600)
                );
            }
        }
        // Folder/Hybrid aggregates are also tiles and must remain within the
        // same fixed branch while their projected membership changes.
        let folder = scene
            .tile(TileId::Directory(
                scene.directory_id(Path::new("/disk/a/n")).unwrap(),
            ))
            .unwrap();
        let mut folder_projection = projection.clone();
        folder_projection
            .replace(&[], vec![(folder.id, folder.to_entry())])
            .unwrap();
        let folder_updated = prepare(
            &geometry,
            &map,
            &projection,
            &folder_projection,
            &[],
            &[TileDelta {
                before: None,
                after: Some(folder.clone()),
            }],
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .unwrap();
        assert!(
            folder_updated
                .map
                .rect(folder_projection.slot(folder.id).unwrap())
                .is_some()
        );
        assert_eq!(folder_updated.map.rect(1), neighbor);
    }
}
