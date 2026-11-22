//! Background transaction joining projection, geometry, pixels and hit testing.
//! The caller owns version validation and must publish the returned map,
//! projection and optional texture patch together in the same frame.
use crate::{
    incremental_geometry::{self, IncrementalGeometry, TileState},
    render::{self, RasterPatch, RenderEntry, RenderMap},
    scene_actor::TileDelta,
    sparse_projection::SparseProjection,
};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

pub struct PreparedView {
    pub geometry: IncrementalGeometry,
    pub projection: SparseProjection,
    pub map: RenderMap,
    pub patch: Option<RasterPatch>,
    /// Remaining background masks after the new patch is uploaded.
    pub holes: Vec<crate::treemap::Rect>,
}

#[derive(Debug)]
pub enum Error {
    InvalidProjection,
    Geometry(incremental_geometry::Error),
    Cancelled,
    /// The caller should rebuild in the background and retain the old frame.
    Rebuild,
}

/// Remove only the part of an existing background mask covered by an uploaded
/// physical-pixel patch. The remaining pieces continue to hide deleted owners.
/// Returning None signals invalid patch dimensions; the caller must not publish.
pub fn clip_holes(
    holes: &[crate::treemap::Rect],
    bounds: crate::treemap::Rect,
    physical: [usize; 2],
    origin: [usize; 2],
    size: [usize; 2],
) -> Option<Vec<crate::treemap::Rect>> {
    use crate::treemap::Rect;
    if physical.contains(&0)
        || size.contains(&0)
        || !bounds.x.is_finite()
        || !bounds.y.is_finite()
        || !bounds.w.is_finite()
        || !bounds.h.is_finite()
        || bounds.w <= 0.0
        || bounds.h <= 0.0
        || origin[0]
            .checked_add(size[0])
            .is_none_or(|x| x > physical[0])
        || origin[1]
            .checked_add(size[1])
            .is_none_or(|y| y > physical[1])
    {
        return None;
    }
    // Match RenderMap::erase_many's conversion exactly, including f32
    // rounding, so a mask edge and an uploaded patch share the same grid.
    let sx = physical[0] as f32 / bounds.w;
    let sy = physical[1] as f32 / bounds.h;
    let px = |x: usize| bounds.x + x as f32 / sx;
    let py = |y: usize| bounds.y + y as f32 / sy;
    let (cx0, cy0) = (px(origin[0]), py(origin[1]));
    let (cx1, cy1) = (px(origin[0] + size[0]), py(origin[1] + size[1]));
    let mut result = Vec::new();
    for &h in holes {
        let (hx1, hy1) = (h.x + h.w, h.y + h.h);
        let (ix0, iy0) = (h.x.max(cx0), h.y.max(cy0));
        let (ix1, iy1) = (hx1.min(cx1), hy1.min(cy1));
        if ix0 >= ix1 || iy0 >= iy1 {
            result.push(h);
            continue;
        }
        let mut push = |x0: f32, y0: f32, x1: f32, y1: f32| {
            if x1 > x0 && y1 > y0 {
                result.push(Rect {
                    x: x0,
                    y: y0,
                    w: x1 - x0,
                    h: y1 - y0,
                });
            }
        };
        push(h.x, h.y, hx1, iy0);
        push(h.x, iy1, hx1, hy1);
        push(h.x, iy0, ix0, iy1);
        push(ix1, iy0, hx1, iy1);
    }
    Some(result)
}

/// Takes owned worker-local state. The UI keeps the previous map and sparse
/// projection until it checks generation/scene version and publishes the result.
/// Platform geometry changes are deliberately rejected by IncrementalGeometry.
pub fn prepare(
    mut geometry: IncrementalGeometry,
    mut map: RenderMap,
    old_projection: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    pixels_per_point: f32,
    area_tolerance: f64,
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<PreparedView, Error> {
    if token.load(Ordering::Relaxed) != generation {
        return Err(Error::Cancelled);
    }
    let mut next = old_projection.clone();
    let before: Vec<_> = deltas
        .iter()
        .filter_map(|d| d.before.as_ref().map(|t| t.id))
        .collect();
    let after = deltas
        .iter()
        .filter_map(|d| d.after.as_ref().map(|t| (t.id, t.to_entry())))
        .collect();
    next.replace(&before, after)
        .map_err(|_| Error::InvalidProjection)?;
    let state = |tile: &crate::scene_model::Tile| TileState {
        id: tile.id,
        parent: tile.parent.clone(),
        weight: tile.size,
    };
    let mut changes = Vec::new();
    for delta in deltas {
        match (&delta.before, &delta.after) {
            (Some(old), Some(new)) if old.id == new.id => {
                if old.parent != new.parent || old.size != new.size {
                    changes.push(incremental_geometry::TileDelta {
                        before: Some(state(old)),
                        after: Some(state(new)),
                    });
                }
            }
            (Some(old), Some(new)) => {
                changes.push(incremental_geometry::TileDelta {
                    before: Some(state(old)),
                    after: None,
                });
                changes.push(incremental_geometry::TileDelta {
                    before: None,
                    after: Some(state(new)),
                });
            }
            (Some(old), None) => changes.push(incremental_geometry::TileDelta {
                before: Some(state(old)),
                after: None,
            }),
            (None, Some(new)) => changes.push(incremental_geometry::TileDelta {
                before: None,
                after: Some(state(new)),
            }),
            (None, None) => return Err(Error::InvalidProjection),
        }
    }
    let update = geometry
        .apply(
            &changes,
            pixels_per_point,
            area_tolerance,
            token,
            generation,
        )
        .map_err(Error::Geometry)?;
    if update.dirty.is_empty() {
        return Ok(PreparedView {
            geometry,
            projection: next,
            map,
            patch: None,
            holes: old_holes.to_vec(),
        });
    }
    let mut render_changes: BTreeMap<usize, Option<RenderEntry>> = BTreeMap::new();
    for (id, rect) in update.tiles {
        let slot = match rect {
            Some(_) => next.slot(id),
            None => old_projection.slot(id),
        }
        .ok_or(Error::InvalidProjection)?;
        let entry = match rect {
            Some(rect) => {
                let tile = next.get(slot).ok_or(Error::InvalidProjection)?;
                Some(RenderEntry {
                    id: slot,
                    rect,
                    color: crate::color(&tile.parent),
                })
            }
            None => None,
        };
        render_changes.insert(slot, entry);
    }
    let commit: Vec<_> = render_changes
        .iter()
        .map(|(&slot, &entry)| (slot, entry))
        .collect();
    let mut entries = Vec::with_capacity(map.entries().len().saturating_add(render_changes.len()));
    for entry in map.entries() {
        if let Some(value) = render_changes.remove(&entry.id) {
            if let Some(value) = value {
                entries.push(value);
            }
        } else if map.rect(entry.id).is_some() && entry.rect.w > 0.0 && entry.rect.h > 0.0 {
            entries.push(*entry);
        }
    }
    entries.extend(render_changes.into_values().flatten());
    let [width, height] = map.physical_size();
    let bounds = map.bounds;
    let mut region = [width, height, 0, 0];
    for r in update.dirty {
        let x0 = (((r.x - bounds.x) / bounds.w * width as f32).floor() as isize - 1)
            .clamp(0, width as isize) as usize;
        let y0 = (((r.y - bounds.y) / bounds.h * height as f32).floor() as isize - 1)
            .clamp(0, height as isize) as usize;
        let x1 = ((((r.x + r.w) - bounds.x) / bounds.w * width as f32).ceil() as isize + 1)
            .clamp(0, width as isize) as usize;
        let y1 = ((((r.y + r.h) - bounds.y) / bounds.h * height as f32).ceil() as isize + 1)
            .clamp(0, height as isize) as usize;
        region[0] = region[0].min(x0);
        region[1] = region[1].min(y0);
        region[2] = region[2].max(x1);
        region[3] = region[3].max(y1);
    }
    if region[0] >= region[2] || region[1] >= region[3] {
        return Err(Error::Rebuild);
    }
    if (region[2] - region[0]).saturating_mul(region[3] - region[1]) > max_patch_pixels {
        return Err(Error::Rebuild);
    }
    let patch = render::raster_patch(
        &entries,
        map.platforms(),
        bounds,
        [width, height],
        region,
        token,
        generation,
    )
    .ok_or(Error::Cancelled)?;
    let holes = clip_holes(
        old_holes,
        bounds,
        [width, height],
        patch.origin,
        patch.image.size,
    )
    .ok_or(Error::Rebuild)?;
    if !map.apply_scene_patch(&patch, &commit, None) {
        return Err(Error::Rebuild);
    }
    if token.load(Ordering::Relaxed) != generation {
        return Err(Error::Cancelled);
    }
    Ok(PreparedView {
        geometry,
        projection: next,
        map,
        patch: Some(patch),
        holes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        directory_map,
        incremental_geometry::{Projection, projection_slots},
        scan,
        scene_model::{Scene, TileId},
        treemap::Rect,
    };
    use std::{path::Path, sync::Arc, time::Duration};

    #[test]
    fn prepared_transaction_keeps_old_view_and_updates_pixels_and_owners() {
        let token = AtomicU64::new(1);
        let identity = |n| scan::FileIdentity {
            device: n,
            ..Default::default()
        };
        let file = |name: &str, n| scan::FileEntry {
            path: format!("/disk/a/{name}").into(),
            parent: Arc::from(Path::new("/disk/a")),
            size: 50,
            identity: identity(n),
            parent_identity: identity(1),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("f", 2), file("g", 3)],
            directories: vec![scan::DirectoryFact {
                path: "/disk/a".into(),
                identity: identity(1),
                enumeration_complete: true,
                has_subdirectories: false,
            }]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let mut scene = Scene::from_scan(source.clone());
        let before = scene
            .tile(TileId::File(scene.source_id(0).unwrap()))
            .unwrap();
        let slots = projection_slots(&scene, &source, Projection::Files).unwrap();
        let projection = SparseProjection::new(source.clone(), slots.clone()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk/a"), vec![(0, 50), (1, 50)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geo = IncrementalGeometry::from_layout(&layout.partitions, &slots).unwrap();
        let original = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        let r = original.rect(0).unwrap();
        let at = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let delta = TileDelta {
            before: Some(before),
            after: None,
        };
        let out = prepare(
            geo,
            original.snapshot_for_update(),
            &projection,
            &[],
            &[delta],
            1.0,
            10000.0,
            10000,
            &token,
            1,
        )
        .unwrap();
        assert_eq!(original.hit_test(at.0, at.1), Some(0));
        assert!(projection.get(0).is_some());
        assert_eq!(out.map.hit_test(at.0, at.1), None);
        assert!(out.projection.get(0).is_none());
        assert!(out.patch.is_some());

        let impact = scene
            .apply(crate::scene_model::Change::UpsertFile(file("new", 4)))
            .unwrap();
        let new = scene
            .tile(TileId::File(*impact.files.iter().next().unwrap()))
            .unwrap();
        let out = prepare(
            out.geometry,
            out.map,
            &out.projection,
            &out.holes,
            &[TileDelta {
                before: None,
                after: Some(new.clone()),
            }],
            1.0,
            0.01,
            10000,
            &token,
            1,
        )
        .unwrap();
        assert_eq!(out.map.hit_test(at.0, at.1), Some(2));
        assert_eq!(
            out.projection.get(2).unwrap().path,
            Path::new("/disk/a/new")
        );
        // A metadata-only rename must not redraw or change hit ownership.
        let mut renamed = new.clone();
        renamed.path = "/disk/a/renamed".into();
        let next = prepare(
            out.geometry,
            out.map,
            &out.projection,
            &out.holes,
            &[TileDelta {
                before: Some(new),
                after: Some(renamed),
            }],
            1.0,
            0.01,
            10000,
            &token,
            1,
        )
        .unwrap();
        assert!(next.patch.is_none());
        assert_eq!(next.map.hit_test(at.0, at.1), Some(2));
        assert_eq!(
            next.projection.get(2).unwrap().path,
            Path::new("/disk/a/renamed")
        );
    }

    #[test]
    fn patch_removes_only_covered_mask_in_physical_pixel_coordinates() {
        let bounds = Rect {
            x: 10.0,
            y: 20.0,
            w: 200.0,
            h: 100.0,
        };
        let holes = [Rect {
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 80.0,
        }];
        let remaining = clip_holes(&holes, bounds, [100, 50], [10, 10], [10, 10]).unwrap();
        assert_eq!(remaining.len(), 4);
        let area: f32 = remaining.iter().map(|r| r.w * r.h).sum();
        assert_eq!(area, 100.0 * 80.0 - 20.0 * 20.0);
        for r in remaining {
            assert!(r.x + r.w <= 30.0 || r.x >= 50.0 || r.y + r.h <= 40.0 || r.y >= 60.0);
        }
        assert!(clip_holes(&holes, bounds, [100, 50], [99, 0], [2, 1]).is_none());
    }

    #[test]
    fn old_erased_entries_are_not_resurrected_when_a_hole_is_reused() {
        let token = AtomicU64::new(1);
        let identity = |n| scan::FileIdentity {
            device: n,
            ..Default::default()
        };
        let file = |name: &str, n| scan::FileEntry {
            path: format!("/disk/a/{name}").into(),
            parent: Arc::from(Path::new("/disk/a")),
            size: 50,
            identity: identity(n),
            parent_identity: identity(1),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("f", 2), file("g", 3)],
            directories: vec![scan::DirectoryFact {
                path: "/disk/a".into(),
                identity: identity(1),
                enumeration_complete: true,
                has_subdirectories: false,
            }]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let mut scene = Scene::from_scan(source.clone());
        let old_id = TileId::File(scene.source_id(0).unwrap());
        let slots = projection_slots(&scene, &source, Projection::Files).unwrap();
        let mut projection = SparseProjection::new(source, slots.clone()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk/a"), vec![(0, 50), (1, 50)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let mut geo = IncrementalGeometry::from_layout(&layout.partitions, &slots).unwrap();
        let mut map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        let old = map.rect(0).unwrap();
        let point = (old.x + old.w / 2.0, old.y + old.h / 2.0);
        let holes = map.erase_many(&[0], &token, 1).unwrap();
        projection.replace(&[old_id], vec![]).unwrap();
        geo.apply(
            &[incremental_geometry::TileDelta {
                before: Some(TileState {
                    id: old_id,
                    parent: "/disk/a".into(),
                    weight: 50,
                }),
                after: None,
            }],
            1.0,
            10000.0,
            &token,
            1,
        )
        .unwrap();
        scene
            .apply(crate::scene_model::Change::RemoveFile {
                path: "/disk/a/f".into(),
                identity: identity(2),
            })
            .unwrap();
        let impact = scene
            .apply(crate::scene_model::Change::UpsertFile(file("new", 4)))
            .unwrap();
        let new = scene
            .tile(TileId::File(*impact.files.iter().next().unwrap()))
            .unwrap();
        let out = prepare(
            geo,
            map,
            &projection,
            &holes,
            &[TileDelta {
                before: None,
                after: Some(new),
            }],
            1.0,
            0.01,
            10000,
            &token,
            1,
        )
        .unwrap();
        assert_eq!(out.map.hit_test(point.0, point.1), Some(2));
        assert!(out.holes.is_empty());
    }
}
