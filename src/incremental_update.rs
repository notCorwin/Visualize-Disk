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
    pub transition: Option<Transition>,
}

pub struct Transition {
    pub origin: [usize; 2],
    pub atlas: eframe::egui::ColorImage,
    pub background: eframe::egui::ColorImage,
    pub tiles: Vec<TransitionTile>,
}

pub struct TransitionTile {
    pub from: crate::treemap::Rect,
    pub to: crate::treemap::Rect,
    pub uv: eframe::egui::Rect,
}

/// Pack each tile into its own padded atlas region. Only pixels owned by that
/// tile are copied, so touching/subpixel tiles can never sample one another.
fn transition_atlas(
    source: &RasterPatch,
    moves: &[(usize, crate::treemap::Rect, crate::treemap::Rect)],
    bounds: crate::treemap::Rect,
    full: [usize; 2],
) -> Option<(eframe::egui::ColorImage, Vec<TransitionTile>)> {
    use crate::treemap::Rect;
    use eframe::egui::{Color32, ColorImage, Rect as UiRect, pos2};
    let sx = full[0] as f32 / bounds.w;
    let sy = full[1] as f32 / bounds.h;
    let mut sources = Vec::new();
    let mut area = 0usize;
    for &(id, from, to) in moves {
        let ax = (((from.x - bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(full[0]);
        let bx =
            ((((from.x + from.w) - bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(full[0]);
        let ay = (((from.y - bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(full[1]);
        let by =
            ((((from.y + from.h) - bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(full[1]);
        if ax >= bx || ay >= by {
            let tx0 = (((to.x - bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(full[0]);
            let tx1 =
                ((((to.x + to.w) - bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(full[0]);
            let ty0 = (((to.y - bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(full[1]);
            let ty1 =
                ((((to.y + to.h) - bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(full[1]);
            if tx0 >= tx1 || ty0 >= ty1 {
                continue;
            }
            return None;
        }
        if ax < source.origin[0]
            || ay < source.origin[1]
            || bx > source.origin[0] + source.image.size[0]
            || by > source.origin[1] + source.image.size[1]
        {
            // Subpixel tiles cannot provide an independent sprite. Publish the
            // correct final patch without animation instead.
            return None;
        }
        area = area.checked_add((bx - ax + 2).checked_mul(by - ay + 2)?)?;
        if area > 2_097_152 || sources.len() >= 512 {
            return None;
        }
        sources.push((id, from, to, ax, ay, bx, by));
    }
    let max_w = sources.iter().map(|s| s.5 - s.3 + 2).max()?;
    let atlas_w = ((area as f64).sqrt().ceil() as usize).max(max_w).min(2048);
    if max_w > atlas_w {
        return None;
    }
    let mut packed = Vec::new();
    let (mut x, mut y, mut row) = (0usize, 0usize, 0usize);
    for source in &sources {
        let (w, h) = (source.5 - source.3 + 2, source.6 - source.4 + 2);
        if x + w > atlas_w {
            y = y.checked_add(row)?;
            x = 0;
            row = 0;
        }
        if y.checked_add(h)? > 2048 {
            return None;
        }
        packed.push((x + 1, y + 1));
        x += w;
        row = row.max(h);
    }
    let atlas_h = y.checked_add(row)?;
    let mut atlas = ColorImage::new(
        [atlas_w, atlas_h],
        vec![Color32::TRANSPARENT; atlas_w.checked_mul(atlas_h)?],
    );
    let mut tiles = Vec::with_capacity(sources.len());
    for ((id, from, to, ax, ay, bx, by), (dx, dy)) in sources.into_iter().zip(packed) {
        for py in ay..by {
            for px in ax..bx {
                let lx = px - source.origin[0];
                let ly = py - source.origin[1];
                if source.owner_at(lx, ly) == Some(id) {
                    atlas.pixels[(dy + py - ay) * atlas_w + dx + px - ax] =
                        source.image.pixels[ly * source.image.size[0] + lx];
                }
            }
        }
        // Pixel-aligned source bounds map proportionally into the new tile.
        let aligned = Rect {
            x: bounds.x + ax as f32 / sx,
            y: bounds.y + ay as f32 / sy,
            w: (bx - ax) as f32 / sx,
            h: (by - ay) as f32 / sy,
        };
        let target = Rect {
            x: to.x + (aligned.x - from.x) / from.w * to.w,
            y: to.y + (aligned.y - from.y) / from.h * to.h,
            w: aligned.w / from.w * to.w,
            h: aligned.h / from.h * to.h,
        };
        let uv = UiRect::from_min_max(
            pos2(dx as f32 / atlas_w as f32, dy as f32 / atlas_h as f32),
            pos2(
                (dx + bx - ax) as f32 / atlas_w as f32,
                (dy + by - ay) as f32 / atlas_h as f32,
            ),
        );
        tiles.push(TransitionTile {
            from: aligned,
            to: target,
            uv,
        });
    }
    Some((atlas, tiles))
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
        .apply_compacting(
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
            transition: None,
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
    let moves: Vec<_> = render_changes
        .iter()
        .filter_map(|(&slot, entry)| {
            let from = map.rect(slot)?;
            let to = entry.as_ref()?.rect;
            (from != to).then_some((slot, from, to))
        })
        .collect();
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
    // Rasterize the old tile pixels once in the worker, then mask by owner so
    // an interpolated quad never carries its neighbours along with it. The
    // target background excludes moving tiles and is also rendered only once.
    let transition = if deltas.iter().all(|d| match (&d.before, &d.after) {
        (Some(_), None) => true,
        (Some(old), Some(new)) => {
            old.id == new.id && old.parent == new.parent && new.size <= old.size
        }
        _ => false,
    }) && !moves.is_empty()
    {
        let ids: std::collections::HashSet<_> = moves.iter().map(|m| m.0).collect();
        let old_entries: Vec<_> = map
            .entries()
            .iter()
            .copied()
            .filter(|entry| map.rect(entry.id).is_some())
            .collect();
        let background_entries: Vec<_> = entries
            .iter()
            .copied()
            .filter(|entry| !ids.contains(&entry.id))
            .collect();
        let source = render::raster_patch(
            &old_entries,
            map.platforms(),
            bounds,
            [width, height],
            region,
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        let background = render::raster_patch(
            &background_entries,
            map.platforms(),
            bounds,
            [width, height],
            region,
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        transition_atlas(&source, &moves, bounds, [width, height]).map(|(atlas, tiles)| {
            Transition {
                origin: background.origin,
                atlas,
                background: background.image,
                tiles,
            }
        })
    } else {
        None
    };
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
        transition,
    })
}

/// Recompute the directory hierarchy in the background when a change affects
/// terrace topology. Only the changed pixels and their lighting halo are
/// rasterized and uploaded; the previously displayed texture stays live.
/// A region exceeding the caller's budget uses the existing full rebuild.
pub fn prepare_structural(
    previous_geometry: &IncrementalGeometry,
    mut map: RenderMap,
    old_projection: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    requested_scope: &std::path::Path,
    pixels_per_point: f32,
    area_tolerance: f64,
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<PreparedView, Error> {
    use crate::{directory_map, treemap::Rect};
    use std::collections::{BTreeSet, HashMap};
    use std::path::Path;

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
    let mut groups: HashMap<&Path, Vec<(usize, u64)>> = HashMap::new();
    for slot in 0..next.len() {
        if slot % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        if let Some(entry) = next.get(slot) {
            groups
                .entry(entry.parent.as_ref())
                .or_default()
                .push((slot, entry.size.max(1)));
        }
    }
    if groups.is_empty() {
        return Err(Error::Rebuild);
    }
    let bounds = map.bounds;
    let size = map.physical_size();
    let scan_root = &next.base().root;
    if !requested_scope.starts_with(scan_root) {
        return Err(Error::Rebuild);
    }
    let old_parts = previous_geometry
        .export_partitions(old_projection, map.platforms())
        .map_err(Error::Geometry)?;
    let slots: Vec<_> = (0..next.len()).map(|slot| next.id(slot)).collect();
    // Start at the smallest common parent of the changed projection tiles.
    // Only grow to an ancestor when a folder tile changes parent or the fixed
    // allocation would exceed the caller's area-error budget.
    let mut scope = requested_scope.to_path_buf();
    for tile in deltas
        .iter()
        .flat_map(|d| d.before.iter().chain(d.after.iter()))
    {
        while !tile.parent.starts_with(&scope) {
            if scope == *scan_root {
                return Err(Error::Rebuild);
            }
            scope = scope.parent().ok_or(Error::Rebuild)?.to_path_buf();
        }
    }
    let (layout, geometry) = loop {
        let part = old_parts
            .iter()
            .find(|p| p.path.as_deref() == Some(scope.as_path()));
        let subtree: Vec<_> = groups
            .iter()
            .filter(|(path, _)| path.starts_with(&scope))
            .map(|(path, entries)| (*path, entries.clone()))
            .collect();
        if let (Some(part), false) = (part, subtree.is_empty()) {
            let outer = part
                .platform
                .map_or(part.bounds, |platform| platform.bounds);
            let depth = old_parts
                .iter()
                .filter(|p| {
                    p.platform.is_some()
                        && p.path.as_ref().is_some_and(|path| {
                            path != &scope
                                && path != scan_root
                                && path.starts_with(scan_root)
                                && scope.starts_with(path)
                        })
                })
                .count()
                .min(u8::MAX as usize) as u8;
            let sub = directory_map::layout_subtree(
                subtree,
                scan_root,
                &scope,
                depth,
                outer,
                render::logical_pixel_size(bounds, size),
                token,
                generation,
            )
            .ok_or(Error::Cancelled)?;
            let mut parts: Vec<_> = old_parts
                .iter()
                .filter(|p| p.path.as_ref().is_none_or(|p| !p.starts_with(&scope)))
                .cloned()
                .collect();
            parts.extend(sub.partitions.iter().cloned());
            let candidate = IncrementalGeometry::from_layout_optional(&parts, &slots)
                .map_err(Error::Geometry)?;
            if candidate.ancestor_area_error(&scope, pixels_per_point) <= area_tolerance.max(0.0) {
                let mut entries: Vec<_> = map
                    .entries()
                    .iter()
                    .copied()
                    .filter(|e| {
                        map.rect(e.id).is_some()
                            && old_projection
                                .get(e.id)
                                .is_some_and(|entry| !entry.parent.starts_with(&scope))
                    })
                    .collect();
                entries.extend(sub.entries);
                // Platform order is breadth-first. Siblings never overlap, but
                // an ancestor's platform must always precede its descendants.
                parts.sort_by(|a, b| {
                    a.path
                        .as_ref()
                        .map_or(0, |p| p.components().count())
                        .cmp(&b.path.as_ref().map_or(0, |p| p.components().count()))
                        .then_with(|| a.path.cmp(&b.path))
                });
                let platforms = parts.iter().filter_map(|p| p.platform).collect();
                break (
                    directory_map::DirectoryLayout {
                        entries,
                        platforms,
                        partitions: Vec::new(),
                    },
                    candidate,
                );
            }
        }
        if scope == *scan_root {
            return Err(Error::Rebuild);
        }
        scope = scope.parent().ok_or(Error::Rebuild)?.to_path_buf();
    };

    let old: HashMap<_, _> = map
        .entries()
        .iter()
        .filter(|e| map.rect(e.id).is_some())
        .map(|e| (e.id, *e))
        .collect();
    let new: HashMap<_, _> = layout.entries.iter().map(|e| (e.id, *e)).collect();
    let ids: BTreeSet<_> = old.keys().chain(new.keys()).copied().collect();
    let mut changed = Vec::new();
    let mut dirty: Option<Rect> = None;
    let mut add = |r: Rect, halo: f32| {
        let r = Rect {
            x: r.x - halo,
            y: r.y - halo,
            w: r.w + halo * 2.0,
            h: r.h + halo * 2.0,
        };
        dirty = Some(match dirty {
            Some(a) => {
                let x = a.x.min(r.x);
                let y = a.y.min(r.y);
                Rect {
                    x,
                    y,
                    w: (a.x + a.w).max(r.x + r.w) - x,
                    h: (a.y + a.h).max(r.y + r.h) - y,
                }
            }
            None => r,
        });
    };
    for id in ids {
        let a = old.get(&id);
        let b = new.get(&id);
        if a.zip(b)
            .is_some_and(|(a, b)| a.rect == b.rect && a.color == b.color)
        {
            continue;
        }
        if let Some(a) = a {
            add(a.rect, 0.0);
        }
        if let Some(b) = b {
            add(b.rect, 0.0);
        }
        changed.push((id, b.copied()));
    }
    // Platforms paint in order. Comparing corresponding positions is
    // conservative if insertion shifts the order; it can only enlarge the
    // patch, never miss a changed platform or its shadow.
    let n = map.platforms().len().max(layout.platforms.len());
    // HeightField interpolates on a world-space grid. Account for the light
    // reach and two field samples on top of the changed platform's bounds.
    let halo = 8.0
        + 2.0
            * (bounds.w / size[0] as f32)
                .max(bounds.h / size[1] as f32)
                .max(0.25);
    for i in 0..n {
        let a = map.platforms().get(i);
        let b = layout.platforms.get(i);
        if a == b {
            continue;
        }
        if let Some(a) = a {
            add(a.bounds, halo);
        }
        if let Some(b) = b {
            add(b.bounds, halo);
        }
    }
    let Some(dirty) = dirty else {
        return Ok(PreparedView {
            geometry,
            projection: next,
            map,
            patch: None,
            holes: old_holes.to_vec(),
            transition: None,
        });
    };
    let region = [
        (((dirty.x - bounds.x) / bounds.w * size[0] as f32).floor() as isize - 2)
            .clamp(0, size[0] as isize) as usize,
        (((dirty.y - bounds.y) / bounds.h * size[1] as f32).floor() as isize - 2)
            .clamp(0, size[1] as isize) as usize,
        ((((dirty.x + dirty.w) - bounds.x) / bounds.w * size[0] as f32).ceil() as isize + 2)
            .clamp(0, size[0] as isize) as usize,
        ((((dirty.y + dirty.h) - bounds.y) / bounds.h * size[1] as f32).ceil() as isize + 2)
            .clamp(0, size[1] as isize) as usize,
    ];
    if region[0] >= region[2]
        || region[1] >= region[3]
        || region == [0, 0, size[0], size[1]]
        || (region[2] - region[0]).saturating_mul(region[3] - region[1]) > max_patch_pixels
    {
        return Err(Error::Rebuild);
    }
    let patch = render::raster_patch(
        &layout.entries,
        &layout.platforms,
        bounds,
        size,
        region,
        token,
        generation,
    )
    .ok_or(Error::Cancelled)?;
    let moves: Vec<_> = changed
        .iter()
        .filter_map(|(slot, new)| {
            let from = old.get(slot)?.rect;
            let to = new.as_ref()?.rect;
            (from != to).then_some((*slot, from, to))
        })
        .collect();
    let transition = if moves.is_empty() {
        None
    } else {
        let moving: std::collections::HashSet<_> = moves.iter().map(|m| m.0).collect();
        let old_entries: Vec<_> = map
            .entries()
            .iter()
            .copied()
            .filter(|e| map.rect(e.id).is_some())
            .collect();
        let background: Vec<_> = layout
            .entries
            .iter()
            .copied()
            .filter(|e| !moving.contains(&e.id))
            .collect();
        let source = render::raster_patch(
            &old_entries,
            map.platforms(),
            bounds,
            size,
            region,
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        let bg = render::raster_patch(
            &background,
            &layout.platforms,
            bounds,
            size,
            region,
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        transition_atlas(&source, &moves, bounds, size).map(|(atlas, tiles)| Transition {
            origin: bg.origin,
            atlas,
            background: bg.image,
            tiles,
        })
    };
    let holes = clip_holes(old_holes, bounds, size, patch.origin, patch.image.size)
        .ok_or(Error::Rebuild)?;
    if !map.apply_scene_patch(&patch, &changed, Some(layout.platforms)) {
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
        transition,
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
    fn structural_platform_patch_matches_full_raster_without_touching_other_pixels() {
        let token = AtomicU64::new(1);
        let file = |parent: &str, name: &str, size: u64, n: u64| scan::FileEntry {
            path: format!("{parent}/{name}").into(),
            parent: Arc::from(Path::new(parent)),
            size,
            identity: scan::FileIdentity {
                device: n,
                ..Default::default()
            },
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/x", "f", 2, 1),
                file("/disk/a/y", "g", 2, 2),
                file("/disk/b", "h", 100, 3),
            ],
            directories: Vec::new().into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let slots = projection_slots(&scene, &source, Projection::Files).unwrap();
        let projection = SparseProjection::new(source, slots.clone()).unwrap();
        let bounds = Rect {
            x: 17.0,
            y: 21.0,
            w: 400.0,
            h: 200.0,
        };
        let size = [800, 400];
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk/a/x"), vec![(0, 2)]),
                (Path::new("/disk/a/y"), vec![(1, 2)]),
                (Path::new("/disk/b"), vec![(2, 100)]),
            ],
            Path::new("/disk"),
            bounds,
            0.5,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(&layout.partitions, &slots).unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            size,
            &token,
            1,
        )
        .unwrap();
        let old_image = map.image.as_ref().unwrap().clone();
        let old_x = scene.tile(slots[0]).unwrap();
        let unaffected = map.rect(2);
        // Strict proportionality forces the affected ancestor to grow until
        // the whole frame would need redrawing, which must request fallback.
        assert!(matches!(
            prepare_structural(
                &geometry,
                map.snapshot_for_update(),
                &projection,
                &[],
                &[TileDelta {
                    before: Some(old_x.clone()),
                    after: None
                }],
                Path::new("/disk/a"),
                2.0,
                0.0,
                size[0] * size[1],
                &token,
                1,
            ),
            Err(Error::Rebuild)
        ));
        let updated = prepare_structural(
            &geometry,
            map.snapshot_for_update(),
            &projection,
            &[],
            &[TileDelta {
                before: Some(old_x),
                after: None,
            }],
            Path::new("/disk/a"),
            2.0,
            40000.0,
            size[0] * size[1],
            &token,
            1,
        )
        .unwrap();
        let patch = updated.patch.as_ref().unwrap();
        assert!(patch.image.size[0] * patch.image.size[1] < size[0] * size[1]);
        assert_eq!(updated.map.rect(2), unaffected);
        assert!(updated.transition.is_some());
        let covers = |x: usize, y: usize| {
            x >= patch.origin[0]
                && y >= patch.origin[1]
                && x < patch.origin[0] + patch.image.size[0]
                && y < patch.origin[1] + patch.image.size[1]
        };
        let outside = [
            (0, 0),
            (size[0] - 1, 0),
            (0, size[1] - 1),
            (size[0] - 1, size[1] - 1),
        ]
        .into_iter()
        .find(|&(x, y)| !covers(x, y))
        .unwrap();
        let pixel = |(x, y): (usize, usize)| Rect {
            x: bounds.x + x as f32 * bounds.w / size[0] as f32,
            y: bounds.y + y as f32 * bounds.h / size[1] as f32,
            w: bounds.w / size[0] as f32,
            h: bounds.h / size[1] as f32,
        };
        let inside_hole = pixel((patch.origin[0], patch.origin[1]));
        let outside_hole = pixel(outside);
        let masks = prepare_structural(
            &geometry,
            map.snapshot_for_update(),
            &projection,
            &[inside_hole, outside_hole],
            &[TileDelta {
                before: Some(scene.tile(slots[0]).unwrap()),
                after: None,
            }],
            Path::new("/disk/a"),
            2.0,
            40000.0,
            size[0] * size[1],
            &token,
            1,
        )
        .unwrap()
        .holes;
        assert_eq!(masks, vec![outside_hole]);
        let full = render::raster_patch(
            updated.map.entries(),
            updated.map.platforms(),
            bounds,
            size,
            [0, 0, size[0], size[1]],
            &token,
            1,
        )
        .unwrap();
        for y in 0..size[1] {
            for x in 0..size[0] {
                let inside = x >= patch.origin[0]
                    && y >= patch.origin[1]
                    && x < patch.origin[0] + patch.image.size[0]
                    && y < patch.origin[1] + patch.image.size[1];
                let actual = if inside {
                    patch.image.pixels
                        [(y - patch.origin[1]) * patch.image.size[0] + x - patch.origin[0]]
                } else {
                    old_image.pixels[y * size[0] + x]
                };
                assert_eq!(
                    actual,
                    full.image.pixels[y * size[0] + x],
                    "color at {x},{y}"
                );
                assert_eq!(
                    updated.map.hit_test(
                        bounds.x + (x as f32 + 0.5) * bounds.w / size[0] as f32,
                        bounds.y + (y as f32 + 0.5) * bounds.h / size[1] as f32
                    ),
                    full.owner_at(x, y),
                    "owner at {x},{y}"
                );
            }
        }
    }

    #[test]
    fn subtree_platform_depth_matches_independent_full_layout_when_weight_is_unchanged() {
        let token = AtomicU64::new(1);
        let file = |parent: &str, name: &str, size: u64, n: u64| scan::FileEntry {
            path: format!("{parent}/{name}").into(),
            parent: Arc::from(Path::new(parent)),
            size,
            identity: scan::FileIdentity {
                device: n,
                ..Default::default()
            },
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/x", "f", 20, 1),
                file("/disk/a/y", "g", 20, 2),
                file("/disk/b", "h", 100, 3),
            ],
            directories: Vec::new().into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let slots = projection_slots(&scene, &source, Projection::Files).unwrap();
        let projection = SparseProjection::new(source, slots.clone()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 200.0,
        };
        let size = [400, 200];
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk/a/x"), vec![(0, 20)]),
                (Path::new("/disk/a/y"), vec![(1, 20)]),
                (Path::new("/disk/b"), vec![(2, 100)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(&layout.partitions, &slots).unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            size,
            &token,
            1,
        )
        .unwrap();
        let old_x = scene.tile(slots[0]).unwrap();
        let old_y = scene.tile(slots[1]).unwrap();
        let mut new_y = old_y.clone();
        new_y.size = 40;
        let updated = prepare_structural(
            &geometry,
            map,
            &projection,
            &[],
            &[
                TileDelta {
                    before: Some(old_x),
                    after: None,
                },
                TileDelta {
                    before: Some(old_y),
                    after: Some(new_y),
                },
            ],
            Path::new("/disk/a"),
            1.0,
            8192.0,
            size[0] * size[1],
            &token,
            1,
        )
        .unwrap();
        let reference = directory_map::layout_with_partitions(
            [
                (Path::new("/disk/a/y"), vec![(1, 40)]),
                (Path::new("/disk/b"), vec![(2, 100)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let actual = render::raster_patch(
            updated.map.entries(),
            updated.map.platforms(),
            bounds,
            size,
            [0, 0, size[0], size[1]],
            &token,
            1,
        )
        .unwrap();
        let expected = render::raster_patch(
            &reference.entries,
            &reference.platforms,
            bounds,
            size,
            [0, 0, size[0], size[1]],
            &token,
            1,
        )
        .unwrap();
        assert_eq!(actual.image.pixels, expected.image.pixels);
        for y in 0..size[1] {
            for x in 0..size[0] {
                assert_eq!(actual.owner_at(x, y), expected.owner_at(x, y));
            }
        }
    }

    #[test]
    fn moving_tile_atlas_isolates_adjacent_owners_and_skips_invisible_tiles() {
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 10.0,
            h: 4.0,
        };
        let left = Rect {
            x: 0.0,
            y: 0.0,
            w: 3.1,
            h: 4.0,
        };
        let right = Rect {
            x: 3.1,
            y: 0.0,
            w: 6.9,
            h: 4.0,
        };
        let invisible = Rect {
            x: 0.01,
            y: 0.01,
            w: 0.01,
            h: 0.01,
        };
        let entries = [
            RenderEntry {
                id: 0,
                rect: left,
                color: eframe::egui::Color32::RED,
            },
            RenderEntry {
                id: 1,
                rect: right,
                color: eframe::egui::Color32::GREEN,
            },
        ];
        let source =
            render::raster_patch(&entries, &[], bounds, [10, 4], [0, 0, 10, 4], &token, 1).unwrap();
        let (atlas, tiles) = transition_atlas(
            &source,
            &[
                (0, left, left),
                (1, right, right),
                (2, invisible, invisible),
            ],
            bounds,
            [10, 4],
        )
        .unwrap();
        assert_eq!(tiles.len(), 2);
        for (id, tile) in tiles.iter().enumerate() {
            let x0 = (tile.uv.min.x * atlas.size[0] as f32).round() as usize;
            let x1 = (tile.uv.max.x * atlas.size[0] as f32).round() as usize;
            let y0 = (tile.uv.min.y * atlas.size[1] as f32).round() as usize;
            let y1 = (tile.uv.max.y * atlas.size[1] as f32).round() as usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = atlas.pixels[y * atlas.size[0] + x];
                    if p != eframe::egui::Color32::TRANSPARENT {
                        if id == 0 {
                            assert_eq!(p.g(), 0);
                        } else {
                            assert_eq!(p.r(), 0);
                        }
                    }
                }
            }
            assert_eq!(
                atlas.pixels[y0 * atlas.size[0] + x0 - 1],
                eframe::egui::Color32::TRANSPARENT
            );
        }
    }

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
        assert_eq!(out.map.hit_test(at.0, at.1), Some(1));
        assert!(out.projection.get(0).is_none());
        assert!(out.patch.is_some());
        let transition = out.transition.as_ref().expect("survivor transitions");
        assert_eq!(transition.tiles.len(), 1);
        assert_ne!(transition.tiles[0].from, transition.tiles[0].to);

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
        let added = out.map.rect(2).unwrap();
        let added_at = (added.x + added.w / 2.0, added.y + added.h / 2.0);
        assert_eq!(out.map.hit_test(added_at.0, added_at.1), Some(2));
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
        assert_eq!(next.map.hit_test(added_at.0, added_at.1), Some(2));
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
