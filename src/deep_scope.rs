//! Local structural relayout below an already allocated scan-root branch.
//!
//! The caller must provide the complete same-version actor proof.  We accept a
//! single existing subtree only when every projected change and every relevant
//! directory fact is inside it; otherwise another transaction must handle it.
use crate::{
    directory_map,
    incremental_geometry::IncrementalGeometry,
    incremental_update::{self, Error, PreparedView},
    render::{self, RenderEntry, RenderMap},
    scene_actor::TileDelta,
    sparse_projection::SparseProjection,
    treemap::Rect,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(test)]
static HITS: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn hit_count(scope: &Path) -> usize {
    HITS.lock()
        .unwrap()
        .iter()
        .filter(|hit| hit == &scope)
        .count()
}

fn inside(inner: Rect, outer: Rect) -> bool {
    let e = 0.001;
    inner.x >= outer.x - e
        && inner.y >= outer.y - e
        && inner.x + inner.w <= outer.x + outer.w + e
        && inner.y + inner.h <= outer.y + outer.h + e
}

/// Update an existing, non-root subtree in its fixed outer allocation.
/// `None` means the complete proof does not authorize this optimization.
pub(crate) fn prepare(
    previous: &IncrementalGeometry,
    map: &RenderMap,
    old: &SparseProjection,
    next: &SparseProjection,
    holes: &[Rect],
    deltas: &[TileDelta],
    requested_scope: &Path,
    structural_paths: &[std::path::PathBuf],
    root_facts_touched: bool,
    root_identity_stable: bool,
    noops: &[std::path::PathBuf],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
) -> Result<Option<PreparedView>, Error> {
    let root = old.base().root.as_path();
    if !root_identity_stable
        || root_facts_touched
        || deltas.is_empty()
        || !requested_scope.starts_with(root)
    {
        return Ok(None);
    }
    // The actor's scope is deliberately conservative across all view modes.
    // A file view may update only Alice even when a Hybrid aggregate would
    // require Alice's parent. Narrow per mode only with its complete projected
    // delta and every changed directory fact accounted for.
    let mut scope: Option<std::path::PathBuf> = None;
    for path in deltas
        .iter()
        .flat_map(|d| d.before.iter().chain(d.after.iter()))
        .map(|tile| tile.parent.as_path())
        .chain(
            structural_paths
                .iter()
                .filter(|path| !noops.contains(path))
                .map(|path| path.as_path()),
        )
    {
        if !path.starts_with(requested_scope) {
            return Ok(None);
        }
        match &mut scope {
            None => scope = Some(path.to_path_buf()),
            Some(current) => {
                while !path.starts_with(&*current) {
                    if !current.pop() {
                        return Ok(None);
                    }
                }
            }
        }
    }
    let Some(mut scope) = scope else {
        return Ok(None);
    };
    let (outer, depth) = loop {
        if scope == root || !scope.starts_with(requested_scope) {
            return Ok(None);
        }
        if let Some(surface) = previous.subtree_surface(&scope, root) {
            break surface;
        }
        if !scope.pop() {
            return Ok(None);
        }
    };
    let scope = scope.as_path();

    let mut ids: BTreeSet<_> = previous.subtree_tiles(scope).into_iter().collect();
    if previous.subtree_has_missing_partition(scope) {
        // A retained parent allocation can outlive its materialized child
        // partition. Recover the child's live slots from the published frame;
        // using only the missing partition would silently drop unchanged files.
        for (index, entry) in map.entries().iter().enumerate() {
            if index % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
                return Err(Error::Cancelled);
            }
            if map.rect(entry.id).is_some()
                && old
                    .get(entry.id)
                    .is_some_and(|tile| tile.parent.starts_with(scope))
            {
                ids.insert(old.id(entry.id).ok_or(Error::InvalidProjection)?);
            }
        }
    }
    ids.extend(deltas.iter().filter_map(|d| d.after.as_ref().map(|t| t.id)));
    let mut groups: HashMap<&Path, Vec<(usize, u64)>> = HashMap::new();
    let mut changes = BTreeMap::<usize, Option<RenderEntry>>::new();
    for (index, id) in ids.into_iter().enumerate() {
        if index % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        if let Some(slot) = old.slot(id) {
            changes.insert(slot, None);
        }
        if let Some(slot) = next.slot(id) {
            let entry = next.get(slot).ok_or(Error::InvalidProjection)?;
            if !entry.parent.starts_with(scope) {
                return Ok(None);
            }
            groups
                .entry(entry.parent.as_ref())
                .or_default()
                .push((slot, entry.size.max(1)));
        }
    }
    // Removing an entire subtree also changes its parent's allocation and must
    // be handled at that parent, not by pretending the subtree still exists.
    if groups.is_empty() {
        return Ok(None);
    }

    // Outer frame and inherited depth belong to this exact directory in the
    // published geometry; they cannot be inferred safely from equal rectangles.
    let size = map.physical_size();
    let layout = directory_map::layout_subtree(
        groups,
        root,
        scope,
        depth,
        outer,
        render::logical_pixel_size(map.bounds, size),
        token,
        generation,
    )
    .ok_or(Error::Cancelled)?;
    let geometry = previous
        .install_subtree(scope, &layout.partitions, next)
        .map_err(Error::Geometry)?
        .refresh_ancestor_weights(scope)
        .map_err(Error::Geometry)?;
    for entry in layout.entries {
        changes.insert(entry.id, Some(entry));
    }
    let mut platforms: Vec<_> = map
        .platforms()
        .iter()
        .copied()
        .filter(|platform| !inside(platform.bounds, outer))
        .collect();
    platforms.extend(layout.platforms);
    let prepared = incremental_update::finish_local_regions_with_animation(
        map,
        next,
        holes,
        geometry,
        changes.into_iter().collect(),
        platforms,
        &[outer],
        max_patch_pixels,
        token,
        generation,
        animate,
    )?;
    #[cfg(test)]
    HITS.lock().unwrap().push(scope.to_path_buf());
    Ok(Some(prepared))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        scan,
        scene_model::{Scene, TileId},
    };
    use std::{sync::Arc, time::Duration};

    #[test]
    fn deep_structural_change_preserves_other_users_and_pixel_ownership() {
        let token = AtomicU64::new(1);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let file = |path: &str, parent: &str, id, parent_id| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: identity(id),
            parent_identity: identity(parent_id),
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
                file("/disk/Users/alice/a", "/disk/Users/alice", 3, 2),
                file("/disk/Users/alice/b/f", "/disk/Users/alice/b", 5, 4),
                file("/disk/Users/other/f", "/disk/Users/other", 7, 6),
                file("/disk/System/f", "/disk/System", 9, 8),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/Users", 10, true),
                fact("/disk/Users/alice", 2, true),
                fact("/disk/Users/alice/b", 4, false),
                fact("/disk/Users/other", 6, false),
                fact("/disk/System", 8, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let ids: Vec<_> = source
            .files
            .iter()
            .map(|file| TileId::File(scene.file_id(&file.path).unwrap()))
            .collect();
        let old = SparseProjection::new(source.clone(), ids.clone()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 900.0,
            h: 600.0,
        };
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk/Users/alice"), vec![(0, 50)]),
                (Path::new("/disk/Users/alice/b"), vec![(1, 50)]),
                (Path::new("/disk/Users/other"), vec![(2, 50)]),
                (Path::new("/disk/System"), vec![(3, 50)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(&layout.partitions, &ids).unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [900, 600],
            &token,
            1,
        )
        .unwrap();
        let neighbors = [map.rect(2), map.rect(3)];
        let before = scene.tile(ids[1]).unwrap();
        // A no-op update independently verifies inherited platform depth and
        // painting order against the original, complete scene.
        let unchanged = TileDelta {
            before: Some(before.clone()),
            after: Some(before.clone()),
        };
        let unchanged_view = prepare(
            &geometry,
            &map,
            &old,
            &old,
            &[],
            &[unchanged],
            Path::new("/disk/Users/alice"),
            &[],
            false,
            true,
            &[],
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .expect("unchanged deep subtree can be prepared");
        // Recovery may retain a directory's parent allocation and platform
        // metadata while its materialized partition is unavailable. Rebuild
        // only that subtree from the published frame, including unchanged
        // files that are absent from the missing partition.
        let mut missing = geometry.clone();
        missing.forget_partition_for_test(Path::new("/disk/Users/alice"));
        let mut missing_descendant = geometry.clone();
        missing_descendant.forget_partition_for_test(Path::new("/disk/Users/alice/b"));
        assert!(missing_descendant.subtree_has_missing_partition(Path::new("/disk/Users/alice")));
        assert!(!missing_descendant.subtree_has_missing_partition(Path::new("/disk/Users/other")));
        let restored_view = prepare(
            &missing,
            &map,
            &old,
            &old,
            &[],
            &[TileDelta {
                before: Some(before.clone()),
                after: Some(before.clone()),
            }],
            Path::new("/disk/Users/alice"),
            &[],
            false,
            true,
            &[],
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .expect("the retained parent allocation can recover a missing subtree");
        assert!(
            restored_view
                .geometry
                .has_partition(Path::new("/disk/Users/alice"))
        );
        assert_eq!(restored_view.map.rect(0), unchanged_view.map.rect(0));
        assert_eq!(restored_view.map.rect(1), unchanged_view.map.rect(1));
        assert_eq!(restored_view.map.rect(2), neighbors[0]);
        assert_eq!(restored_view.map.rect(3), neighbors[1]);
        let old_live: Vec<_> = map
            .entries()
            .iter()
            .copied()
            .filter(|entry| map.rect(entry.id).is_some())
            .collect();
        let old_pixels = render::raster_patch(
            &old_live,
            map.platforms(),
            bounds,
            [900, 600],
            [0, 0, 900, 600],
            &token,
            1,
        )
        .unwrap();
        let restored_pixels: usize = restored_view
            .patches
            .iter()
            .map(|patch| patch.image.size[0] * patch.image.size[1])
            .sum();
        assert!(
            restored_pixels < 900 * 600,
            "recovery must not redraw unrelated branches"
        );
        for patch in &restored_view.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        old_pixels.image.pixels[gy * 900 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), old_pixels.owner_at(gx, gy));
                }
            }
        }
        for patch in &unchanged_view.patches {
            for y in 0..patch.image.size[1] {
                for x in 0..patch.image.size[0] {
                    let gx = patch.origin[0] + x;
                    let gy = patch.origin[1] + y;
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        old_pixels.image.pixels[gy * 900 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), old_pixels.owner_at(gx, gy));
                }
            }
        }
        let mut after = before.clone();
        after.size = 200;
        let delta = TileDelta {
            before: Some(before),
            after: Some(after.clone()),
        };
        let mut next = old.clone();
        next.replace(&[after.id], vec![(after.id, after.to_entry())])
            .unwrap();
        assert!(
            prepare(
                &geometry,
                &map,
                &old,
                &next,
                &[],
                std::slice::from_ref(&delta),
                Path::new("/disk/Users/alice"),
                &["/disk/Users/other/new".into()],
                false,
                true,
                &[],
                4096,
                &token,
                1,
                true
            )
            .unwrap()
            .is_none(),
            "unproved changes outside the scope must not be dropped"
        );
        let unanimated = prepare(
            &geometry,
            &map,
            &old,
            &next,
            &[],
            std::slice::from_ref(&delta),
            Path::new("/disk/Users/alice"),
            &[],
            false,
            true,
            &[],
            4096,
            &token,
            1,
            false,
        )
        .unwrap()
        .expect("non-animated batch can update the existing subtree");
        assert!(unanimated.transitions.is_empty());
        let updated = prepare(
            &geometry,
            &map,
            &old,
            &next,
            &[],
            &[delta],
            Path::new("/disk/Users/alice"),
            &[],
            false,
            true,
            &[],
            4096,
            &token,
            1,
            true,
        )
        .unwrap()
        .expect("existing deep subtree can update locally");
        assert_eq!(unanimated.map.rect(0), updated.map.rect(0));
        assert_eq!(unanimated.map.rect(1), updated.map.rect(1));
        assert_eq!(unanimated.patches.len(), updated.patches.len());
        for (plain, animated) in unanimated.patches.iter().zip(&updated.patches) {
            assert_eq!(plain.origin, animated.origin);
            assert_eq!(plain.image.size, animated.image.size);
            assert_eq!(plain.image.pixels, animated.image.pixels);
            for y in 0..plain.image.size[1] {
                for x in 0..plain.image.size[0] {
                    assert_eq!(plain.owner_at(x, y), animated.owner_at(x, y));
                }
            }
        }
        assert_eq!([updated.map.rect(2), updated.map.rect(3)], neighbors);
        let pixels: usize = updated
            .patches
            .iter()
            .map(|p| p.image.size[0] * p.image.size[1])
            .sum();
        assert!(
            pixels < 900 * 600,
            "unrelated branches must not be rasterized"
        );
        let live: Vec<_> = updated
            .map
            .entries()
            .iter()
            .copied()
            .filter(|entry| updated.map.rect(entry.id).is_some())
            .collect();
        let reference = render::raster_patch(
            &live,
            updated.map.platforms(),
            bounds,
            [900, 600],
            [0, 0, 900, 600],
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
                        reference.image.pixels[gy * 900 + gx]
                    );
                    assert_eq!(patch.owner_at(x, y), reference.owner_at(gx, gy));
                }
            }
        }
    }
}
