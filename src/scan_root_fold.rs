//! Swap a scan-root aggregate and its expanded contents inside their existing
//! persisted allocation. The caller must prove the scan-root identity and
//! every affected directory fact against the same scene version.
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
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(test)]
pub(crate) static PREPARED_ROOT_FOLDS: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
pub(crate) static PREPARED_EMPTY_ROOTS: AtomicU64 = AtomicU64::new(0);

/// Materialize the first visible root subtree within the existing empty
/// synthetic canvas. No previous live geometry is reconstructed.
pub(crate) fn prepare_empty(
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
    if !old.is_empty()
        || deltas.is_empty()
        || deltas
            .iter()
            .any(|d| d.before.is_some() || d.after.is_none())
    {
        return Ok(None);
    }
    let root = old.base().root.as_path();
    let parent = root.parent().unwrap_or(root);
    let mut groups: HashMap<&Path, Vec<(usize, u64)>> = HashMap::new();
    let mut weight = 0u64;
    let mut scope = root;
    for tile in deltas.iter().filter_map(|d| d.after.as_ref()) {
        if !tile.parent.starts_with(root) {
            if tile.path != root || tile.parent != parent {
                return Ok(None);
            }
            scope = parent;
        }
        let slot = next.slot(tile.id).ok_or(Error::InvalidProjection)?;
        groups
            .entry(&tile.parent)
            .or_default()
            .push((slot, tile.size.max(1)));
        weight = weight.saturating_add(tile.size.max(1));
    }
    let layout = directory_map::layout_subtree(
        groups,
        root,
        scope,
        0,
        map.bounds,
        render::logical_pixel_size(map.bounds, map.physical_size()),
        token,
        generation,
    )
    .ok_or(Error::Cancelled)?;
    let geometry = previous
        .bootstrap_empty_root(scope, &layout.partitions, next, weight)
        .map_err(Error::Geometry)?;
    let changes = layout
        .entries
        .into_iter()
        .map(|entry| (entry.id, Some(entry)))
        .collect();
    let updated = incremental_update::finish_local_regions_with_animation(
        map,
        next,
        holes,
        geometry,
        changes,
        layout.platforms,
        &[map.bounds],
        max_patch_pixels,
        token,
        generation,
        animate,
    )?;
    #[cfg(test)]
    PREPARED_EMPTY_ROOTS.fetch_add(1, Ordering::Relaxed);
    Ok(Some(updated))
}

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
    if token.load(Ordering::Relaxed) != generation {
        return Err(Error::Cancelled);
    }
    let root = old.base().root.as_path();
    let parent = root.parent().unwrap_or(root);
    let mut aggregate = None;
    for delta in deltas {
        let candidate = match (&delta.before, &delta.after) {
            (Some(tile), None) if tile.path == root && matches!(tile.id, TileId::Directory(_)) => {
                Some((tile, true))
            }
            (None, Some(tile)) if tile.path == root && matches!(tile.id, TileId::Directory(_)) => {
                Some((tile, false))
            }
            _ => None,
        };
        if let Some(candidate) = candidate {
            if aggregate.replace(candidate).is_some() {
                return Ok(None);
            }
        }
    }
    let Some((aggregate, expanding)) = aggregate else {
        return Ok(None);
    };
    let aggregate_in_root = expanding && previous.tile_layout_parent(aggregate.id, parent) == root;
    // A root aggregate denotes the entire root, so all previously displayed
    // root contents must be explicitly removed before it can replace them.
    // Conversely, expansion must not overwrite a coexisting directory tree.
    let previous_ids: BTreeSet<_> = previous.subtree_tiles(root).into_iter().collect();
    let removed_ids: BTreeSet<_> = deltas
        .iter()
        .filter_map(|d| d.before.as_ref().map(|t| t.id))
        .collect();
    if expanding {
        let expected = if aggregate_in_root {
            BTreeSet::from([aggregate.id])
        } else {
            BTreeSet::new()
        };
        if previous_ids != expected || removed_ids != BTreeSet::from([aggregate.id]) {
            return Ok(None);
        }
    } else if previous_ids != removed_ids
        || deltas.iter().filter_map(|d| d.after.as_ref()).count() != 1
    {
        return Ok(None);
    }
    if deltas
        .iter()
        .flat_map(|d| d.before.iter().chain(d.after.iter()))
        .any(|tile| !tile.path.starts_with(root))
    {
        return Ok(None);
    }

    let frame = if parent == root || aggregate_in_root {
        previous.scan_root_frame(root)
    } else {
        previous.root_fold_frame(parent, root, expanding.then_some(aggregate.id))
    };
    let Some(frame) = frame else { return Ok(None) };
    let mut changes = BTreeMap::new();
    for tile in deltas.iter().filter_map(|d| d.before.as_ref()) {
        changes.insert(old.slot(tile.id).ok_or(Error::InvalidProjection)?, None);
    }
    let mut groups: HashMap<&Path, Vec<(usize, u64)>> = HashMap::new();
    for tile in deltas.iter().filter_map(|d| d.after.as_ref()) {
        let slot = next.slot(tile.id).ok_or(Error::InvalidProjection)?;
        if expanding && !tile.parent.starts_with(root) {
            return Ok(None);
        }
        groups
            .entry(tile.parent.as_path())
            .or_default()
            .push((slot, tile.size.max(1)));
    }
    if expanding && groups.is_empty() {
        return Ok(None);
    }

    let mut subparts = Vec::new();
    let mut platforms = Vec::new();
    let weight;
    if expanding || parent == root {
        weight = groups
            .values()
            .flatten()
            .fold(0u64, |sum, (_, n)| sum.saturating_add(*n));
        let sub = directory_map::layout_subtree(
            groups,
            root,
            root,
            0,
            frame,
            render::logical_pixel_size(map.bounds, map.physical_size()),
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        for entry in sub.entries {
            changes.insert(entry.id, Some(entry));
        }
        subparts = sub.partitions;
        platforms = sub.platforms;
    } else {
        let slot = next.slot(aggregate.id).ok_or(Error::InvalidProjection)?;
        weight = aggregate.size.max(1);
        changes.insert(
            slot,
            Some(RenderEntry {
                id: slot,
                rect: frame,
                color: crate::color(&aggregate.parent),
            }),
        );
    }
    let geometry = if parent == root || aggregate_in_root {
        // Removing the relocated aggregate first also removes its explicit
        // layout-parent override. Its slot remains a tombstone until a later
        // root aggregate is allocated in a fresh display slot.
        let base = if aggregate_in_root {
            previous
                .remove_root_tile(root, aggregate.id)
                .map_err(Error::Geometry)?
                .0
        } else {
            previous.clone()
        };
        base.install_subtree(root, &subparts, next)
            .map_err(Error::Geometry)?
            .refresh_ancestor_weights(root)
            .map_err(Error::Geometry)?
    } else {
        previous
            .replace_root_fold(
                parent,
                root,
                expanding.then_some(aggregate.id),
                (!expanding).then_some((aggregate.id, weight)),
                &subparts,
                next,
                weight,
            )
            .map_err(Error::Geometry)?
    };
    // This transaction replaces the entire root allocation; no unrelated
    // platform from outside the scanned root is admitted by the checks above.
    let inside = |r: Rect| {
        r.x >= frame.x
            && r.y >= frame.y
            && r.x + r.w <= frame.x + frame.w + 0.01
            && r.y + r.h <= frame.y + frame.h + 0.01
    };
    let mut retained: Vec<_> = map
        .platforms()
        .iter()
        .copied()
        .filter(|p| !inside(p.bounds))
        .collect();
    retained.extend(platforms);
    let updated = incremental_update::finish_local_regions_with_animation(
        map,
        next,
        holes,
        geometry,
        changes.into_iter().collect(),
        retained,
        &[frame],
        max_patch_pixels,
        token,
        generation,
        animate,
    )?;
    #[cfg(test)]
    PREPARED_ROOT_FOLDS.fetch_add(1, Ordering::Relaxed);
    Ok(Some(updated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        scan,
        scene_model::{Scene, TileId},
    };
    use std::{path::PathBuf, sync::Arc, time::Duration};

    fn assert_quiet_matches_reference(
        animated: &PreparedView,
        quiet: &PreparedView,
        token: &AtomicU64,
    ) {
        assert!(quiet.transitions.is_empty());
        let entries: Vec<_> = quiet
            .map
            .entries()
            .iter()
            .copied()
            .filter(|entry| quiet.map.rect(entry.id).is_some())
            .collect();
        let bounds = quiet.map.bounds;
        let size = quiet.map.physical_size();
        let reference = render::raster_patch(
            &entries,
            quiet.map.platforms(),
            bounds,
            size,
            [0, 0, size[0], size[1]],
            token,
            1,
        )
        .unwrap();
        for patch in &quiet.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        reference.image.pixels[gy * size[0] + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), reference.owner_at(gx, gy));
                    let lx = bounds.x + (gx as f32 + 0.5) * bounds.w / size[0] as f32;
                    let ly = bounds.y + (gy as f32 + 0.5) * bounds.h / size[1] as f32;
                    assert_eq!(quiet.map.hit_test(lx, ly), animated.map.hit_test(lx, ly));
                }
            }
        }
        for entry in &entries {
            assert_eq!(quiet.map.rect(entry.id), animated.map.rect(entry.id));
        }
    }

    fn round_trip(root: &str) {
        let root = PathBuf::from(root);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let file = scan::FileEntry {
            path: root.join("a"),
            parent: Arc::from(root.as_path()),
            size: 32,
            identity: identity(2),
            parent_identity: identity(1),
            parent_is_leaf: true,
            can_trash: true,
        };
        let fact = scan::DirectoryFact {
            path: root.clone(),
            identity: identity(1),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: false,
        };
        let source = Arc::new(scan::ScanResult {
            root: root.clone(),
            files: vec![file],
            directories: vec![fact].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let folder = scene
            .tile(TileId::Directory(scene.directory_id(&root).unwrap()))
            .unwrap();
        let file = scene
            .tile(TileId::File(scene.file_id(&root.join("a")).unwrap()))
            .unwrap();
        let projected = Arc::new(scan::ScanResult {
            root: root.clone(),
            files: vec![folder.to_entry()],
            directories: source.directories.clone(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let old = SparseProjection::new(projected, vec![folder.id]).unwrap();
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 80.0,
            h: 60.0,
        };
        let layout = directory_map::layout_with_partitions(
            [(folder.parent.as_path(), vec![(0, folder.size)])],
            &root,
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(&layout.partitions, &[folder.id]).unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [80, 60],
            &token,
            1,
        )
        .unwrap();
        let mut next = old.clone();
        next.replace(&[folder.id], vec![(file.id, file.to_entry())])
            .unwrap();
        let delta = [
            TileDelta {
                before: Some(folder.clone()),
                after: None,
            },
            TileDelta {
                before: None,
                after: Some(file.clone()),
            },
        ];
        let expanded = prepare(
            &geometry,
            &map,
            &old,
            &next,
            &[],
            &delta,
            128,
            &token,
            1,
            true,
        )
        .unwrap()
        .expect("expand root aggregate");
        let quiet = prepare(
            &geometry,
            &map,
            &old,
            &next,
            &[],
            &delta,
            128,
            &token,
            1,
            false,
        )
        .unwrap()
        .expect("expand without animation");
        assert_quiet_matches_reference(&expanded, &quiet, &token);
        let point = (40.0, 30.0);
        assert_eq!(expanded.map.hit_test(point.0, point.1), next.slot(file.id));
        let mut folded = next.clone();
        folded
            .replace(&[file.id], vec![(folder.id, folder.to_entry())])
            .unwrap();
        let delta = [
            TileDelta {
                before: Some(file),
                after: None,
            },
            TileDelta {
                before: None,
                after: Some(folder.clone()),
            },
        ];
        let result = prepare(
            &expanded.geometry,
            &expanded.map,
            &next,
            &folded,
            &expanded.holes,
            &delta,
            128,
            &token,
            1,
            true,
        )
        .unwrap()
        .expect("fold root aggregate");
        assert_eq!(
            result.map.hit_test(point.0, point.1),
            folded.slot(folder.id)
        );
        assert_ne!(folded.slot(folder.id), old.slot(folder.id));
    }

    #[test]
    fn swaps_scan_root_aggregate_with_expanded_contents() {
        round_trip("/disk");
        round_trip("/");
    }

    #[test]
    fn materializes_empty_synthetic_canvas_for_any_scan_root() {
        for root in ["/", "/disk"] {
            let root = PathBuf::from(root);
            let file = scan::FileEntry {
                path: root.join("a/f"),
                parent: Arc::from(root.join("a")),
                size: 5,
                identity: Default::default(),
                parent_identity: Default::default(),
                parent_is_leaf: true,
                can_trash: true,
            };
            let source = Arc::new(scan::ScanResult {
                root: root.clone(),
                files: vec![file],
                directories: Vec::new().into(),
                skipped: 0,
                errors: vec![],
                elapsed: Duration::ZERO,
            });
            let scene = Scene::from_scan(source.clone());
            let file = scene
                .tile(TileId::File(scene.file_id(&root.join("a/f")).unwrap()))
                .unwrap();
            let old = SparseProjection::new_optional(source, vec![None]).unwrap();
            let token = AtomicU64::new(1);
            let bounds = Rect {
                x: 0.0,
                y: 0.0,
                w: 80.0,
                h: 60.0,
            };
            let layout = directory_map::layout_with_partitions(
                std::iter::empty(),
                &root,
                bounds,
                1.0,
                &token,
                1,
            )
            .unwrap();
            let geometry =
                IncrementalGeometry::from_layout_optional(&layout.partitions, &[None]).unwrap();
            let map = RenderMap::build_with_platforms(
                layout.entries,
                layout.platforms,
                bounds,
                [80, 60],
                &token,
                1,
            )
            .unwrap();
            let mut next = old.clone();
            next.replace(&[], vec![(file.id, file.to_entry())]).unwrap();
            let delta = [TileDelta {
                before: None,
                after: Some(file.clone()),
            }];
            let updated = prepare_empty(
                &geometry,
                &map,
                &old,
                &next,
                &[],
                &delta,
                128,
                &token,
                1,
                true,
            )
            .unwrap()
            .unwrap();
            let quiet = prepare_empty(
                &geometry,
                &map,
                &old,
                &next,
                &[],
                &delta,
                128,
                &token,
                1,
                false,
            )
            .unwrap()
            .unwrap();
            assert_quiet_matches_reference(&updated, &quiet, &token);
            assert_eq!(updated.map.hit_test(40.0, 30.0), next.slot(file.id));
            assert!(updated.geometry.scan_root_frame(&root).is_some());
        }
    }

    #[test]
    fn materializes_root_aggregate_in_empty_synthetic_canvas() {
        for root in ["/", "/disk"] {
            let root = PathBuf::from(root);
            let file = scan::FileEntry {
                path: root.join("f"),
                parent: Arc::from(root.as_path()),
                size: 5,
                identity: Default::default(),
                parent_identity: Default::default(),
                parent_is_leaf: true,
                can_trash: true,
            };
            let source = Arc::new(scan::ScanResult {
                root: root.clone(),
                files: vec![file],
                directories: Vec::new().into(),
                skipped: 0,
                errors: vec![],
                elapsed: Duration::ZERO,
            });
            let scene = Scene::from_scan(source);
            let folder = scene
                .tile(TileId::Directory(scene.directory_id(&root).unwrap()))
                .unwrap();
            let projected = Arc::new(scan::ScanResult {
                root: root.clone(),
                files: vec![folder.to_entry()],
                directories: Vec::new().into(),
                skipped: 0,
                errors: vec![],
                elapsed: Duration::ZERO,
            });
            let old = SparseProjection::new_optional(projected, vec![None]).unwrap();
            let token = AtomicU64::new(1);
            let bounds = Rect {
                x: 0.0,
                y: 0.0,
                w: 80.0,
                h: 60.0,
            };
            let layout = directory_map::layout_with_partitions(
                std::iter::empty(),
                &root,
                bounds,
                1.0,
                &token,
                1,
            )
            .unwrap();
            let geometry =
                IncrementalGeometry::from_layout_optional(&layout.partitions, &[None]).unwrap();
            let map = RenderMap::build_with_platforms(
                layout.entries,
                layout.platforms,
                bounds,
                [80, 60],
                &token,
                1,
            )
            .unwrap();
            let mut next = old.clone();
            next.replace(&[], vec![(folder.id, folder.to_entry())])
                .unwrap();
            let delta = [TileDelta {
                before: None,
                after: Some(folder.clone()),
            }];
            let updated = prepare_empty(
                &geometry,
                &map,
                &old,
                &next,
                &[],
                &delta,
                128,
                &token,
                1,
                true,
            )
            .unwrap()
            .unwrap();
            assert_eq!(updated.map.hit_test(40.0, 30.0), next.slot(folder.id));
        }
    }

    #[test]
    fn expands_an_aggregate_previously_placed_inside_root() {
        let root = Path::new("/disk");
        let file = scan::FileEntry {
            path: root.join("f"),
            parent: Arc::from(root),
            size: 100,
            identity: Default::default(),
            parent_identity: Default::default(),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: root.into(),
            files: vec![file],
            directories: Vec::new().into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let file = scene
            .tile(TileId::File(scene.file_id(&root.join("f")).unwrap()))
            .unwrap();
        let folder = scene
            .tile(TileId::Directory(scene.directory_id(root).unwrap()))
            .unwrap();
        let projected = Arc::new(scan::ScanResult {
            root: root.into(),
            files: vec![folder.to_entry()],
            directories: Vec::new().into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let old = SparseProjection::new(projected, vec![folder.id]).unwrap();
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 80.0,
            h: 60.0,
        };
        let layout = directory_map::layout_with_partitions(
            [(root, vec![(0, 100)])],
            root,
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(&layout.partitions, &[file.id]).unwrap();
        let (geometry, folder_frame, _) = geometry
            .allocate_root_child(root, root, Some(folder.id), 100)
            .unwrap();
        let (geometry, _) = geometry.remove_root_tile(root, file.id).unwrap();
        assert_eq!(geometry.tile_layout_parent(folder.id, Path::new("/")), root);
        let map = RenderMap::build_with_platforms(
            vec![RenderEntry {
                id: 0,
                rect: folder_frame,
                color: crate::color(&folder.parent),
            }],
            vec![],
            bounds,
            [80, 60],
            &token,
            1,
        )
        .unwrap();
        let mut next = old.clone();
        next.replace(&[folder.id], vec![(file.id, file.to_entry())])
            .unwrap();
        let deltas = [
            TileDelta {
                before: Some(folder.clone()),
                after: None,
            },
            TileDelta {
                before: None,
                after: Some(file.clone()),
            },
        ];
        let expanded = prepare(
            &geometry,
            &map,
            &old,
            &next,
            &[],
            &deltas,
            128,
            &token,
            1,
            true,
        )
        .unwrap()
        .expect("expand relocated aggregate");
        assert_eq!(
            expanded
                .geometry
                .tile_layout_parent(folder.id, Path::new("/")),
            Path::new("/")
        );
        assert_eq!(expanded.map.hit_test(40.0, 30.0), next.slot(file.id));
        let mut folded = next.clone();
        folded
            .replace(&[file.id], vec![(folder.id, folder.to_entry())])
            .unwrap();
        let deltas = [
            TileDelta {
                before: Some(file),
                after: None,
            },
            TileDelta {
                before: None,
                after: Some(folder.clone()),
            },
        ];
        let folded = prepare(
            &expanded.geometry,
            &expanded.map,
            &next,
            &folded,
            &expanded.holes,
            &deltas,
            128,
            &token,
            1,
            true,
        )
        .unwrap()
        .expect("fold root again");
        assert_eq!(
            folded
                .geometry
                .tile_layout_parent(folder.id, Path::new("/")),
            Path::new("/")
        );
    }
}
