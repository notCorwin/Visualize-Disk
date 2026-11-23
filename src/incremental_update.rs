//! Background transaction joining projection, geometry, pixels and hit testing.
//! The caller owns version validation and must publish the returned map,
//! projection and texture patches together in the same frame.
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

#[cfg(test)]
pub(crate) static ROOT_FALLBACKS: std::sync::Mutex<BTreeMap<std::path::PathBuf, usize>> =
    std::sync::Mutex::new(BTreeMap::new());

#[cfg(test)]
pub(crate) static GENERAL_FALLBACKS: std::sync::Mutex<BTreeMap<std::path::PathBuf, usize>> =
    std::sync::Mutex::new(BTreeMap::new());

#[cfg(test)]
fn record_general_fallback(root: &std::path::Path) {
    *GENERAL_FALLBACKS
        .lock()
        .expect("fallback counter")
        .entry(root.to_path_buf())
        .or_default() += 1;
}

#[cfg(test)]
fn record_root_fallback(root: &std::path::Path) {
    *ROOT_FALLBACKS
        .lock()
        .expect("fallback counter")
        .entry(root.to_path_buf())
        .or_default() += 1;
}

pub struct PreparedView {
    pub geometry: IncrementalGeometry,
    pub projection: SparseProjection,
    pub map: RenderMap,
    pub patches: Vec<RasterPatch>,
    /// Remaining background masks after the new patch is uploaded.
    pub holes: Vec<crate::treemap::Rect>,
    pub transitions: Vec<Transition>,
}

pub struct Transition {
    pub origin: [usize; 2],
    pub atlas: Option<eframe::egui::ColorImage>,
    /// None leaves the already-uploaded target texture visible underneath.
    pub background: Option<eframe::egui::ColorImage>,
    /// Previous scene without the independently moving tiles. During the
    /// transition it fades over the new background, so tiles outside the
    /// bounded sprite budget still get a smooth, deterministic transition.
    pub previous: eframe::egui::ColorImage,
    pub tiles: Vec<TransitionTile>,
}

#[derive(Clone)]
pub struct TransitionTile {
    pub id: usize,
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
            continue;
        }
        if ax < source.origin[0]
            || ay < source.origin[1]
            || bx > source.origin[0] + source.image.size[0]
            || by > source.origin[1] + source.image.size[1]
        {
            // Subpixel tiles cannot provide an independent sprite. Publish the
            // correct final patch without animation instead.
            continue;
        }
        sources.push((id, from, to, ax, ay, bx, by));
    }
    // Prefer the largest visible motion when the image contains more tiles
    // than can be rendered cheaply. All remaining changes fade rather than
    // disabling the entire transition. Sorting by area also makes selection
    // independent of the source slot order.
    sources.sort_unstable_by_key(|s| std::cmp::Reverse((s.5 - s.3).saturating_mul(s.6 - s.4)));
    let (estimated_area, widest) =
        sources
            .iter()
            .take(512)
            .fold((0usize, 1usize), |(area, widest), item| {
                let w = item.5 - item.3 + 2;
                let h = item.6 - item.4 + 2;
                if w > 2048 || h > 2048 || w.saturating_mul(h) > 2_097_152 {
                    (area, widest)
                } else {
                    (
                        area.saturating_add(w.saturating_mul(h)).min(2_097_152),
                        widest.max(w),
                    )
                }
            });
    let atlas_w = ((estimated_area as f64).sqrt().ceil() as usize)
        .max(widest)
        .min(2048);
    let mut packed = Vec::new();
    let mut selected = Vec::new();
    let (mut x, mut y, mut row) = (0usize, 0usize, 0usize);
    let mut area = 0usize;
    for item in sources {
        let (w, h) = (item.5 - item.3 + 2, item.6 - item.4 + 2);
        let Some(next_area) = area.checked_add(w.checked_mul(h)?) else {
            continue;
        };
        if w > atlas_w || h > 2048 || next_area > 2_097_152 || selected.len() >= 512 {
            continue;
        }
        let owned = (item.4..item.6).any(|py| {
            (item.3..item.5).any(|px| {
                source.owner_at(px - source.origin[0], py - source.origin[1]) == Some(item.0)
            })
        });
        if !owned {
            continue;
        }
        let (mut next_x, mut next_y, mut next_row) = (x, y, row);
        if next_x + w > atlas_w {
            next_y = next_y.checked_add(next_row)?;
            next_x = 0;
            next_row = 0;
        }
        if next_y.checked_add(h)? > 2048 {
            continue;
        }
        packed.push((next_x + 1, next_y + 1));
        selected.push(item);
        area = next_area;
        x = next_x + w;
        y = next_y;
        row = next_row.max(h);
    }
    let atlas_h = y.checked_add(row)?.max(1);
    let atlas_w = if selected.is_empty() { 1 } else { atlas_w };
    let mut atlas = ColorImage::new(
        [atlas_w, atlas_h],
        vec![Color32::TRANSPARENT; atlas_w.checked_mul(atlas_h)?],
    );
    let mut tiles = Vec::with_capacity(selected.len());
    for ((id, from, to, ax, ay, bx, by), (dx, dy)) in selected.into_iter().zip(packed) {
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
            id,
            from: aligned,
            to: target,
            uv,
        });
    }
    Some((atlas, tiles))
}

/// Build a bounded hybrid transition: important visible tiles move as
/// independent sprites; the remaining scene fades to the new layout. Both
/// layers use the same raster grid and the old/new scenes remain immutable.
fn build_transition<'a>(
    map: &RenderMap,
    target: &RasterPatch,
    old_holes: &[crate::treemap::Rect],
    entries: impl Into<render::EntryView<'a>>,
    platforms: &'a [render::Platform],
    moves: &[(usize, crate::treemap::Rect, crate::treemap::Rect)],
    region: [usize; 4],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<Transition, Error> {
    let bounds = map.bounds;
    let size = map.physical_size();
    let target_index =
        render::RasterIndex::new(entries, platforms, bounds, size, token, generation)
            .ok_or(Error::Cancelled)?;
    build_transition_indexed(
        map,
        target,
        old_holes,
        &target_index,
        moves,
        region,
        max_patch_pixels,
        token,
        generation,
    )
}

fn build_transition_indexed(
    map: &RenderMap,
    target: &RasterPatch,
    old_holes: &[crate::treemap::Rect],
    target_index: &render::RasterIndex<'_>,
    moves: &[(usize, crate::treemap::Rect, crate::treemap::Rect)],
    region: [usize; 4],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<Transition, Error> {
    let bounds = map.bounds;
    let size = map.physical_size();
    let old_index = map.raster_index();
    let mut source = raster_index_region(
        &old_index,
        region,
        max_patch_pixels,
        token,
        generation,
        |entry| map.rect(entry.id).is_some(),
    )
    .ok_or(Error::Cancelled)?;
    mask_old_holes(&mut source, old_holes, bounds, size);
    let (atlas, tiles) = transition_atlas(&source, moves, bounds, size).ok_or(Error::Rebuild)?;
    let selected: std::collections::HashSet<_> = tiles.iter().map(|tile| tile.id).collect();
    if selected.is_empty() {
        return Ok(Transition {
            origin: target.origin,
            atlas: Some(atlas),
            background: Some(target.image.clone()),
            previous: source.image,
            tiles,
        });
    }
    drop(source);
    let mut previous = raster_index_region(
        &old_index,
        region,
        max_patch_pixels,
        token,
        generation,
        |entry| map.rect(entry.id).is_some() && !selected.contains(&entry.id),
    )
    .ok_or(Error::Cancelled)?;
    mask_old_holes(&mut previous, old_holes, bounds, size);
    let background = raster_index_region(
        target_index,
        region,
        max_patch_pixels,
        token,
        generation,
        |entry| !selected.contains(&entry.id),
    )
    .ok_or(Error::Cancelled)?;
    Ok(Transition {
        origin: background.origin,
        atlas: Some(atlas),
        background: Some(background.image),
        previous: previous.image,
        tiles,
    })
}

/// Raster each bounded tile using the same global sampling grid. The caller
/// keeps the pieces private until the entire geometry/owner transaction can
/// be published. Texture uploads remain split into bounded tiles.
fn raster_pieces<'a>(
    entries: impl Into<render::EntryView<'a>>,
    platforms: &'a [render::Platform],
    bounds: crate::treemap::Rect,
    size: [usize; 2],
    region: [usize; 4],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<Vec<RasterPatch>, Error> {
    let index = render::RasterIndex::new(entries, platforms, bounds, size, token, generation)
        .ok_or(Error::Cancelled)?;
    raster_pieces_indexed(&index, region, max_patch_pixels, token, generation)
}

fn raster_pieces_indexed(
    index: &render::RasterIndex<'_>,
    region: [usize; 4],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<Vec<RasterPatch>, Error> {
    let mut patches = Vec::new();
    index
        .for_each_patch(
            region,
            max_patch_pixels.max(1),
            token,
            generation,
            |patch| {
                patches.push(patch);
                true
            },
        )
        .ok_or(Error::Cancelled)?;
    Ok(patches)
}

fn raster_index_region(
    index: &render::RasterIndex<'_>,
    region: [usize; 4],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    keep: impl Fn(&RenderEntry) -> bool,
) -> Option<RasterPatch> {
    let mut patches = Vec::new();
    index.for_each_patch_filtered(
        region,
        max_patch_pixels.max(1),
        token,
        generation,
        keep,
        |patch| {
            patches.push(patch);
            true
        },
    )?;
    if patches.len() == 1 {
        patches.pop()
    } else {
        RasterPatch::assemble(region, &patches)
    }
}

/// Large updates fade a bounded number of old-scene tiles over the already
/// uploaded new texture. This keeps staging memory proportional to the
/// changed pixels without assembling another full-area owner/pixel buffer.
/// Small updates retain the more precise per-object position/size animation.
fn transitions_for_patches<'a>(
    map: &RenderMap,
    patches: &[RasterPatch],
    old_holes: &[crate::treemap::Rect],
    entries: impl Into<render::EntryView<'a>>,
    platforms: &'a [render::Platform],
    moves: &[(usize, crate::treemap::Rect, crate::treemap::Rect)],
    region: [usize; 4],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<Vec<Transition>, Error> {
    if patches.len() == 1 {
        return Ok(vec![build_transition(
            map,
            &patches[0],
            old_holes,
            entries,
            platforms,
            moves,
            region,
            max_patch_pixels,
            token,
            generation,
        )?]);
    }
    let bounds = map.bounds;
    let size = map.physical_size();
    let index = map.raster_index();
    // Use strips when needed so the number of additional GPU textures stays
    // bounded even when the caller requests an unusually small upload tile.
    // The raster upload budget itself remains unchanged.
    let width = region[2] - region[0];
    let area = width.saturating_mul(region[3] - region[1]);
    let animation_budget = max_patch_pixels.max(width).max(area.div_ceil(16));
    let mut transitions = Vec::new();
    index
        .for_each_patch_filtered(
            region,
            animation_budget,
            token,
            generation,
            |entry| map.rect(entry.id).is_some(),
            |mut old| {
                mask_old_holes(&mut old, old_holes, bounds, size);
                transitions.push(Transition {
                    origin: old.origin,
                    atlas: None,
                    background: None,
                    previous: old.image,
                    tiles: Vec::new(),
                });
                true
            },
        )
        .ok_or(Error::Cancelled)?;
    Ok(transitions)
}

fn transitions_for_maps(
    old: &RenderMap,
    target: &RenderMap,
    patches: &[RasterPatch],
    holes: &[crate::treemap::Rect],
    moves: &[(usize, crate::treemap::Rect, crate::treemap::Rect)],
    region: [usize; 4],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<Vec<Transition>, Error> {
    if patches.len() == 1 {
        return Ok(vec![build_transition_indexed(
            old,
            &patches[0],
            holes,
            &target.raster_index(),
            moves,
            region,
            max_patch_pixels,
            token,
            generation,
        )?]);
    }
    transitions_for_patches(
        old,
        patches,
        holes,
        target.entries(),
        target.platforms(),
        moves,
        region,
        max_patch_pixels,
        token,
        generation,
    )
}

fn mask_old_holes(
    patch: &mut RasterPatch,
    holes: &[crate::treemap::Rect],
    bounds: crate::treemap::Rect,
    full: [usize; 2],
) {
    let [px, py] = patch.origin;
    let [w, h] = patch.image.size;
    for hole in holes {
        let x0 = (((hole.x - bounds.x) / bounds.w * full[0] as f32).round() as isize)
            .clamp(px as isize, (px + w) as isize) as usize;
        let y0 = (((hole.y - bounds.y) / bounds.h * full[1] as f32).round() as isize)
            .clamp(py as isize, (py + h) as isize) as usize;
        let x1 = ((((hole.x + hole.w) - bounds.x) / bounds.w * full[0] as f32).round() as isize)
            .clamp(px as isize, (px + w) as isize) as usize;
        let y1 = ((((hole.y + hole.h) - bounds.y) / bounds.h * full[1] as f32).round() as isize)
            .clamp(py as isize, (py + h) as isize) as usize;
        patch.mask_background([x0 - px, y0 - py, x1 - px, y1 - py], crate::BG);
    }
}

/// Collect a multi-batch update privately without retaining every historical
/// patch. The grid is fixed to the physical texture: each cell stores the
/// original pixels once and the latest colors/owners once, independent of the
/// number of intermediate batches. Nothing is published until `finish`.
pub struct PatchAccumulator<'a> {
    original: &'a RenderMap,
    holes: &'a [crate::treemap::Rect],
    index: render::RasterIndex<'a>,
    token: &'a AtomicU64,
    generation: u64,
    tile_size: [usize; 2],
    tiles: BTreeMap<(usize, usize), AccumulatedPatch>,
}

struct AccumulatedPatch {
    original: eframe::egui::ColorImage,
    latest: RasterPatch,
}

impl<'a> PatchAccumulator<'a> {
    pub fn new(
        original: &'a RenderMap,
        holes: &'a [crate::treemap::Rect],
        token: &'a AtomicU64,
        generation: u64,
    ) -> Result<Self, Error> {
        let size = original.physical_size();
        if token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        let index = original.raster_index();
        // At most 8x8 additional texture uploads, even for a 2048² view.
        let tile_size = [size[0].div_ceil(8).max(1), size[1].div_ceil(8).max(1)];
        Ok(Self {
            original,
            holes,
            index,
            token,
            generation,
            tile_size,
            tiles: BTreeMap::new(),
        })
    }

    pub fn push(&mut self, patches: &[RasterPatch]) -> Result<(), Error> {
        let full = self.original.physical_size();
        for patch in patches {
            let [x, y] = patch.origin;
            let [w, h] = patch.image.size;
            let x1 = x
                .checked_add(w)
                .filter(|&end| end <= full[0])
                .ok_or(Error::Rebuild)?;
            let y1 = y
                .checked_add(h)
                .filter(|&end| end <= full[1])
                .ok_or(Error::Rebuild)?;
            if w == 0 || h == 0 {
                return Err(Error::Rebuild);
            }
            let first_x = x / self.tile_size[0];
            let first_y = y / self.tile_size[1];
            let last_x = (x1 - 1) / self.tile_size[0];
            let last_y = (y1 - 1) / self.tile_size[1];
            for ty in first_y..=last_y {
                for tx in first_x..=last_x {
                    if self.token.load(Ordering::Relaxed) != self.generation {
                        return Err(Error::Cancelled);
                    }
                    let key = (tx, ty);
                    if !self.tiles.contains_key(&key) {
                        let ox = tx * self.tile_size[0];
                        let oy = ty * self.tile_size[1];
                        let region = [
                            ox,
                            oy,
                            (ox + self.tile_size[0]).min(full[0]),
                            (oy + self.tile_size[1]).min(full[1]),
                        ];
                        let budget = (region[2] - ox) * (region[3] - oy);
                        let mut old = raster_index_region(
                            &self.index,
                            region,
                            budget,
                            self.token,
                            self.generation,
                            |entry| self.original.rect(entry.id).is_some(),
                        )
                        .ok_or(Error::Cancelled)?;
                        mask_old_holes(&mut old, self.holes, self.original.bounds, full);
                        let mut latest =
                            RasterPatch::blank([ox, oy], [region[2] - ox, region[3] - oy], full)
                                .ok_or(Error::Rebuild)?;
                        if !latest.blit_from(&old) {
                            return Err(Error::Rebuild);
                        }
                        self.tiles.insert(
                            key,
                            AccumulatedPatch {
                                original: old.image,
                                latest,
                            },
                        );
                    }
                    if !self
                        .tiles
                        .get_mut(&key)
                        .expect("inserted")
                        .latest
                        .blit_from(patch)
                    {
                        return Err(Error::Rebuild);
                    }
                }
            }
        }
        Ok(())
    }

    pub fn finish_without_animations(self) -> Vec<RasterPatch> {
        self.tiles.into_values().map(|tile| tile.latest).collect()
    }

    /// Prepare bounded sprites for the largest surviving moved objects and
    /// fade the remaining changes. Every cell uses the same selected owners.
    /// The candidate set and atlas are bounded regardless of batch count.
    pub fn finish(self, target: &RenderMap) -> Result<(Vec<RasterPatch>, Vec<Transition>), Error> {
        use eframe::egui::{Color32, ColorImage, Rect as UiRect, pos2};
        use std::collections::{BTreeMap, HashMap, HashSet};
        let bounds = self.original.bounds;
        let full = self.original.physical_size();
        if target.bounds != bounds || target.physical_size() != full {
            return Err(Error::Rebuild);
        }
        let sx = full[0] as f32 / bounds.w;
        let sy = full[1] as f32 / bounds.h;
        let covered = |rect: crate::treemap::Rect| {
            let ax = (((rect.x - bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(full[0]);
            let ay = (((rect.y - bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(full[1]);
            let bx =
                ((((rect.x + rect.w) - bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(full[0]);
            let by =
                ((((rect.y + rect.h) - bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(full[1]);
            ax < bx
                && ay < by
                && (ay / self.tile_size[1]..=(by - 1) / self.tile_size[1]).all(|ty| {
                    (ax / self.tile_size[0]..=(bx - 1) / self.tile_size[0])
                        .all(|tx| self.tiles.contains_key(&(tx, ty)))
                })
        };
        // Sorting by the source pixel area selects visible high-impact motion;
        // candidate memory is bounded independently of file and batch counts.
        let mut candidates = BTreeMap::new();
        for tile in self.tiles.values() {
            let [x, y] = tile.latest.origin;
            let [w, h] = tile.latest.image.size;
            let mut old = raster_index_region(
                &self.index,
                [x, y, x + w, y + h],
                w * h,
                self.token,
                self.generation,
                |entry| self.original.rect(entry.id).is_some(),
            )
            .ok_or(Error::Cancelled)?;
            mask_old_holes(&mut old, self.holes, bounds, full);
            let mut seen = HashSet::new();
            for py in 0..h {
                if self.token.load(Ordering::Relaxed) != self.generation {
                    return Err(Error::Cancelled);
                }
                for px in 0..w {
                    let Some(id) = old.owner_at(px, py) else {
                        continue;
                    };
                    if !seen.insert(id) {
                        continue;
                    }
                    let (Some(from), Some(to)) = (self.original.rect(id), target.rect(id)) else {
                        continue;
                    };
                    if from == to || !covered(from) || !covered(to) {
                        continue;
                    }
                    let ax =
                        (((from.x - bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(full[0]);
                    let ay =
                        (((from.y - bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(full[1]);
                    let bx = ((((from.x + from.w) - bounds.x) * sx - 0.5).ceil().max(0.0) as usize)
                        .min(full[0]);
                    let by = ((((from.y + from.h) - bounds.y) * sy - 0.5).ceil().max(0.0) as usize)
                        .min(full[1]);
                    let (w, h) = (bx.saturating_sub(ax), by.saturating_sub(ay));
                    if w == 0
                        || h == 0
                        || w + 2 > 2048
                        || h + 2 > 2048
                        || (w + 2) * (h + 2) > 2_097_152
                    {
                        continue;
                    }
                    candidates.insert((w * h, id), (from, to, ax, ay, bx, by));
                    if candidates.len() > 512 {
                        candidates.pop_first();
                    }
                }
            }
        }
        let estimated: usize = candidates
            .iter()
            .rev()
            .map(|(_, (_, _, ax, ay, bx, by))| (bx - ax + 2) * (by - ay + 2))
            .sum::<usize>()
            .min(2_097_152);
        let widest = candidates
            .iter()
            .map(|(_, (_, _, ax, _, bx, _))| bx - ax + 2)
            .max()
            .unwrap_or(1);
        let atlas_w = ((estimated as f64).sqrt().ceil() as usize)
            .max(widest)
            .max(1)
            .min(2048);
        let (mut x, mut y, mut row, mut area) = (0usize, 0usize, 0usize, 0usize);
        let mut selected = HashMap::new();
        let mut sprites = Vec::new();
        for ((_, id), (from, to, ax, ay, bx, by)) in candidates.into_iter().rev() {
            let (w, h) = (bx - ax + 2, by - ay + 2);
            if area + w * h > 2_097_152 {
                continue;
            }
            if x + w > atlas_w {
                y += row;
                x = 0;
                row = 0;
            }
            if y + h > 2048 || atlas_w.saturating_mul(y + h) > 2_097_152 {
                continue;
            }
            let (dx, dy) = (x + 1, y + 1);
            selected.insert(id, (ax, ay, bx, by, dx, dy));
            let aligned = crate::treemap::Rect {
                x: bounds.x + ax as f32 / sx,
                y: bounds.y + ay as f32 / sy,
                w: (bx - ax) as f32 / sx,
                h: (by - ay) as f32 / sy,
            };
            let destination = crate::treemap::Rect {
                x: to.x + (aligned.x - from.x) / from.w * to.w,
                y: to.y + (aligned.y - from.y) / from.h * to.h,
                w: aligned.w / from.w * to.w,
                h: aligned.h / from.h * to.h,
            };
            sprites.push((id, aligned, destination, dx, dy, bx - ax, by - ay));
            area += w * h;
            x += w;
            row = row.max(h);
        }
        let atlas_h = (y + row).max(1);
        let mut atlas = ColorImage::new(
            [atlas_w, atlas_h],
            vec![Color32::TRANSPARENT; atlas_w * atlas_h],
        );
        let mut patches = Vec::with_capacity(self.tiles.len());
        let mut transitions = Vec::with_capacity(self.tiles.len());
        for (_, tile) in self.tiles {
            let [x, y] = tile.latest.origin;
            let [w, h] = tile.latest.image.size;
            let (previous, background) = if selected.is_empty() {
                (tile.original, None)
            } else {
                let region = [x, y, x + w, y + h];
                let mut original = raster_index_region(
                    &self.index,
                    region,
                    w * h,
                    self.token,
                    self.generation,
                    |entry| self.original.rect(entry.id).is_some(),
                )
                .ok_or(Error::Cancelled)?;
                mask_old_holes(&mut original, self.holes, bounds, full);
                for py in 0..h {
                    if self.token.load(Ordering::Relaxed) != self.generation {
                        return Err(Error::Cancelled);
                    }
                    for px in 0..w {
                        let Some(&(ax, ay, bx, by, dx, dy)) =
                            original.owner_at(px, py).and_then(|id| selected.get(&id))
                        else {
                            continue;
                        };
                        let (gx, gy) = (x + px, y + py);
                        if gx >= ax && gx < bx && gy >= ay && gy < by {
                            atlas.pixels[(dy + gy - ay) * atlas_w + dx + gx - ax] =
                                original.image.pixels[py * w + px];
                        }
                    }
                }
                let mut previous = raster_index_region(
                    &self.index,
                    region,
                    w * h,
                    self.token,
                    self.generation,
                    |entry| {
                        self.original.rect(entry.id).is_some() && !selected.contains_key(&entry.id)
                    },
                )
                .ok_or(Error::Cancelled)?;
                mask_old_holes(&mut previous, self.holes, bounds, full);
                let background = raster_index_region(
                    &target.raster_index(),
                    region,
                    w * h,
                    self.token,
                    self.generation,
                    |entry| !selected.contains_key(&entry.id),
                )
                .ok_or(Error::Cancelled)?;
                (previous.image, Some(background.image))
            };
            transitions.push(Transition {
                origin: tile.latest.origin,
                atlas: None,
                background,
                previous,
                tiles: Vec::new(),
            });
            patches.push(tile.latest);
        }
        if !sprites.is_empty() {
            let first = transitions.first_mut().ok_or(Error::Rebuild)?;
            first.tiles = sprites
                .into_iter()
                .map(|(id, from, to, x, y, w, h)| TransitionTile {
                    id,
                    from,
                    to,
                    uv: UiRect::from_min_max(
                        pos2(x as f32 / atlas_w as f32, y as f32 / atlas_h as f32),
                        pos2(
                            (x + w) as f32 / atlas_w as f32,
                            (y + h) as f32 / atlas_h as f32,
                        ),
                    ),
                })
                .collect();
            first.atlas = Some(atlas);
        }
        Ok((patches, transitions))
    }
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
    geometry: IncrementalGeometry,
    map: RenderMap,
    old_projection: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    pixels_per_point: f32,
    area_tolerance: f64,
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<PreparedView, Error> {
    prepare_internal(
        geometry,
        map,
        old_projection,
        old_holes,
        deltas,
        pixels_per_point,
        area_tolerance,
        max_patch_pixels,
        token,
        generation,
        true,
    )
}

/// Intermediate batches are never visible. Their old/new fade is generated
/// once by PatchAccumulator when the complete transaction is ready.
pub fn prepare_batched(
    geometry: IncrementalGeometry,
    map: RenderMap,
    old_projection: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    pixels_per_point: f32,
    area_tolerance: f64,
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<PreparedView, Error> {
    prepare_internal(
        geometry,
        map,
        old_projection,
        old_holes,
        deltas,
        pixels_per_point,
        area_tolerance,
        max_patch_pixels,
        token,
        generation,
        false,
    )
}

fn prepare_internal(
    mut geometry: IncrementalGeometry,
    map: RenderMap,
    old_projection: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    pixels_per_point: f32,
    area_tolerance: f64,
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
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
    // The coordinator supplies the actor-validated same-version scene delta.
    next.replace_from_scene(&before, after)
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
            patches: Vec::new(),
            holes: old_holes.to_vec(),
            transitions: Vec::new(),
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
    let commit: Vec<_> = render_changes.into_iter().collect();
    finish_local_regions_with_options(
        &map,
        &next,
        old_holes,
        geometry,
        commit,
        None,
        &update.dirty,
        max_patch_pixels,
        token,
        generation,
        animate,
    )
}

/// Preserve a root child's old allocation when a hybrid aggregate expands to
/// its files or folds back. The root has no own platform, so swapping its
/// child allocation cannot alter sibling platform depths. Other, mixed root
/// changes use the general structural path.
fn fast_root_fold(
    previous_geometry: &IncrementalGeometry,
    map: &RenderMap,
    old_projection: &SparseProjection,
    next: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
) -> Result<Option<PreparedView>, Error> {
    use crate::directory_map;
    use std::collections::HashMap;
    use std::path::Path;
    let root = &old_projection.base().root;
    let mut candidate = None;
    for delta in deltas {
        let change = match (&delta.before, &delta.after) {
            (Some(tile), None) if tile.path.parent() == Some(root) => Some((tile, true)),
            (None, Some(tile)) if tile.path.parent() == Some(root) => Some((tile, false)),
            _ => None,
        };
        if let Some(change) = change {
            if candidate.is_some() {
                return Ok(None);
            }
            candidate = Some(change);
        }
    }
    let Some((folder, expanding)) = candidate else {
        return Ok(None);
    };
    let pivot = folder.path.as_path();
    if deltas
        .iter()
        .flat_map(|delta| delta.before.iter().chain(delta.after.iter()))
        .any(|tile| !tile.path.starts_with(pivot))
    {
        return Ok(None);
    }
    let outer = match previous_geometry.root_fold_frame(root, pivot, expanding.then_some(folder.id))
    {
        Some(rect) => rect,
        None => return Ok(None),
    };
    let size = map.physical_size();
    let bounds = map.bounds;
    let mut removals = BTreeMap::new();
    for tile in deltas.iter().flat_map(|delta| delta.before.iter()) {
        let slot = old_projection
            .slot(tile.id)
            .ok_or(Error::InvalidProjection)?;
        removals.insert(slot, None);
    }
    let mut subparts = Vec::new();
    let mut subplatforms = Vec::new();
    let weight;
    if expanding {
        let mut groups: HashMap<&Path, Vec<(usize, u64)>> = HashMap::new();
        for tile in deltas.iter().flat_map(|delta| delta.after.iter()) {
            let slot = next.slot(tile.id).ok_or(Error::InvalidProjection)?;
            groups
                .entry(&tile.parent)
                .or_default()
                .push((slot, tile.size.max(1)));
        }
        if groups.is_empty() || groups.keys().any(|path| !path.starts_with(pivot)) {
            return Ok(None);
        }
        weight = groups
            .values()
            .flatten()
            .fold(0u64, |sum, (_, value)| sum.saturating_add(*value));
        let sub = directory_map::layout_subtree(
            groups,
            root,
            pivot,
            0,
            outer,
            render::logical_pixel_size(bounds, size),
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        for entry in sub.entries {
            removals.insert(entry.id, Some(entry));
        }
        subparts = sub.partitions;
        subplatforms = sub.platforms;
    } else {
        let slot = next.slot(folder.id).ok_or(Error::InvalidProjection)?;
        weight = folder.size.max(1);
        removals.insert(
            slot,
            Some(RenderEntry {
                id: slot,
                rect: outer,
                color: crate::color(&folder.parent),
            }),
        );
    }
    let geometry = previous_geometry
        .replace_root_fold(
            root,
            pivot,
            expanding.then_some(folder.id),
            (!expanding).then_some((folder.id, weight)),
            &subparts,
            next,
            weight,
        )
        .map_err(Error::Geometry)?;
    let inside = |rect: crate::treemap::Rect| {
        rect.x >= outer.x
            && rect.y >= outer.y
            && rect.x + rect.w <= outer.x + outer.w + 0.01
            && rect.y + rect.h <= outer.y + outer.h + 0.01
    };
    let mut platforms: Vec<_> = map
        .platforms()
        .iter()
        .copied()
        .filter(|platform| !inside(platform.bounds))
        .collect();
    platforms.extend(subplatforms);
    let changes: Vec<_> = removals.into_iter().collect();
    finish_local(
        map,
        next,
        old_holes,
        geometry,
        changes,
        platforms,
        outer,
        max_patch_pixels,
        token,
        generation,
        animate,
    )
    .map(Some)
}

/// Replace a root entry by a different object at the same path without
/// reallocating other children. A formerly expanded directory is removed as
/// one subtree and its old frame becomes the replacement tile's frame.
fn fast_root_replacement(
    previous_geometry: &IncrementalGeometry,
    map: &RenderMap,
    old_projection: &SparseProjection,
    next: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
) -> Result<Option<PreparedView>, Error> {
    let root = &old_projection.base().root;
    let after: Vec<_> = deltas.iter().filter_map(|d| d.after.as_ref()).collect();
    if after.len() != 1 || after[0].path.parent() != Some(root) {
        return Ok(None);
    }
    let tile = after[0];
    let pivot = tile.path.as_path();
    let before: Vec<_> = deltas.iter().filter_map(|d| d.before.as_ref()).collect();
    if before.is_empty() || before.iter().any(|old| !old.path.starts_with(pivot)) {
        return Ok(None);
    }
    let direct = before
        .iter()
        .find(|old| old.path == pivot && old.parent == *root);
    let slot = next.slot(tile.id).ok_or(Error::InvalidProjection)?;
    let (geometry, frame, remove_platforms) = if let Some(old) = direct {
        let (geometry, frame) = previous_geometry
            .replace_root_tile(root, old.id, tile.id, tile.size.max(1))
            .map_err(Error::Geometry)?;
        (geometry, frame, false)
    } else {
        let Some(frame) = previous_geometry.root_fold_frame(root, pivot, None) else {
            return Ok(None);
        };
        let geometry = previous_geometry
            .replace_root_fold(
                root,
                pivot,
                None,
                Some((tile.id, tile.size.max(1))),
                &[],
                next,
                tile.size.max(1),
            )
            .map_err(Error::Geometry)?;
        (geometry, frame, true)
    };
    let mut changes = BTreeMap::new();
    for old in before {
        changes.insert(
            old_projection
                .slot(old.id)
                .ok_or(Error::InvalidProjection)?,
            None,
        );
    }
    changes.insert(
        slot,
        Some(RenderEntry {
            id: slot,
            rect: frame,
            color: crate::color(&tile.parent),
        }),
    );
    let inside = |rect: crate::treemap::Rect| {
        rect.x >= frame.x
            && rect.y >= frame.y
            && rect.x + rect.w <= frame.x + frame.w + 0.01
            && rect.y + rect.h <= frame.y + frame.h + 0.01
    };
    let platforms = map
        .platforms()
        .iter()
        .copied()
        .filter(|platform| !remove_platforms || !inside(platform.bounds))
        .collect();
    finish_local(
        map,
        next,
        old_holes,
        geometry,
        changes.into_iter().collect(),
        platforms,
        frame,
        max_patch_pixels,
        token,
        generation,
        animate,
    )
    .map(Some)
}

/// Delete one complete root child without re-laying out its siblings. The
/// caller must also establish that structural facts outside this child did not
/// change; this function validates the visible tile membership independently.
fn fast_root_removal(
    previous_geometry: &IncrementalGeometry,
    map: &RenderMap,
    old_projection: &SparseProjection,
    next: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
) -> Result<Option<PreparedView>, Error> {
    use std::collections::BTreeSet;
    if deltas.is_empty()
        || deltas
            .iter()
            .any(|d| d.before.is_none() || d.after.is_some())
    {
        return Ok(None);
    }
    let root = &old_projection.base().root;
    let first = &deltas[0].before.as_ref().expect("checked").path;
    let Ok(relative) = first.strip_prefix(root) else {
        return Ok(None);
    };
    let Some(component) = relative.components().next() else {
        return Ok(None);
    };
    let pivot = root.join(component.as_os_str());
    if deltas
        .iter()
        .any(|d| !d.before.as_ref().expect("checked").path.starts_with(&pivot))
    {
        return Ok(None);
    }
    let direct: Vec<_> = deltas
        .iter()
        .filter_map(|d| d.before.as_ref())
        .filter(|tile| tile.path == pivot && tile.parent == *root)
        .collect();
    if direct.len() > 1 {
        return Ok(None);
    }
    let root_tile = direct.first().map(|tile| tile.id);
    let mut expected: BTreeSet<_> = previous_geometry
        .subtree_tiles(&pivot)
        .into_iter()
        .collect();
    if let Some(id) = root_tile {
        expected.insert(id);
    }
    let actual: BTreeSet<_> = deltas
        .iter()
        .filter_map(|d| d.before.as_ref().map(|tile| tile.id))
        .collect();
    if expected != actual || actual.len() != deltas.len() {
        return Ok(None);
    }
    let (geometry, tile_frame, directory_frame) =
        match previous_geometry.remove_root_child(root, &pivot, root_tile) {
            Ok(value) => value,
            Err(
                incremental_geometry::Error::MissingPartition
                | incremental_geometry::Error::InvalidDelta,
            ) => return Ok(None),
            Err(error) => return Err(Error::Geometry(error)),
        };
    let outer = match (tile_frame, directory_frame) {
        (Some(a), Some(b)) => {
            let x = a.x.min(b.x);
            let y = a.y.min(b.y);
            crate::treemap::Rect {
                x,
                y,
                w: (a.x + a.w).max(b.x + b.w) - x,
                h: (a.y + a.h).max(b.y + b.h) - y,
            }
        }
        (Some(frame), None) | (None, Some(frame)) => frame,
        (None, None) => return Ok(None),
    };
    let mut changes = Vec::with_capacity(deltas.len());
    for delta in deltas {
        let tile = delta.before.as_ref().expect("checked");
        changes.push((
            old_projection
                .slot(tile.id)
                .ok_or(Error::InvalidProjection)?,
            None,
        ));
    }
    let platforms = map
        .platforms()
        .iter()
        .copied()
        .filter(|platform| {
            !directory_frame.is_some_and(|frame| {
                let rect = platform.bounds;
                rect.x >= frame.x
                    && rect.y >= frame.y
                    && rect.x + rect.w <= frame.x + frame.w + 0.01
                    && rect.y + rect.h <= frame.y + frame.h + 0.01
            })
        })
        .collect();
    finish_local(
        map,
        next,
        old_holes,
        geometry,
        changes,
        platforms,
        outer,
        max_patch_pixels,
        token,
        generation,
        animate,
    )
    .map(Some)
}

fn fast_root_addition(
    previous_geometry: &IncrementalGeometry,
    map: &RenderMap,
    old_projection: &SparseProjection,
    next: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
) -> Result<Option<PreparedView>, Error> {
    use crate::{directory_map, incremental_geometry::RootDonor};
    use std::collections::HashMap;
    use std::path::Path;
    if deltas.is_empty()
        || deltas
            .iter()
            .any(|d| d.before.is_some() || d.after.is_none())
    {
        return Ok(None);
    }
    let root = &old_projection.base().root;
    let first = &deltas[0].after.as_ref().expect("checked").path;
    let Ok(relative) = first.strip_prefix(root) else {
        return Ok(None);
    };
    let Some(component) = relative.components().next() else {
        return Ok(None);
    };
    let pivot = root.join(component.as_os_str());
    if deltas
        .iter()
        .any(|d| !d.after.as_ref().expect("checked").path.starts_with(&pivot))
    {
        return Ok(None);
    }
    let root_tile = deltas
        .iter()
        .filter_map(|d| d.after.as_ref())
        .find(|tile| tile.path == pivot && tile.parent == *root);
    let mut new_groups: HashMap<&Path, Vec<(usize, u64)>> = HashMap::new();
    for tile in deltas.iter().filter_map(|d| d.after.as_ref()) {
        if tile.path == pivot && tile.parent == *root {
            continue;
        }
        let slot = next.slot(tile.id).ok_or(Error::InvalidProjection)?;
        new_groups
            .entry(&tile.parent)
            .or_default()
            .push((slot, tile.size.max(1)));
    }
    if root_tile.is_some() == !new_groups.is_empty() {
        return Ok(None);
    }
    let weight = root_tile.map_or_else(
        || {
            new_groups
                .values()
                .flatten()
                .fold(0u64, |sum, (_, value)| sum.saturating_add(*value))
        },
        |tile| tile.size.max(1),
    );
    let (mut geometry, frame, donor) = match previous_geometry.allocate_root_child(
        root,
        &pivot,
        root_tile.map(|tile| tile.id),
        weight,
    ) {
        Ok(value) => value,
        Err(
            incremental_geometry::Error::MissingPartition
            | incremental_geometry::Error::InvalidDelta,
        ) => return Ok(None),
        Err(error) => return Err(Error::Geometry(error)),
    };
    let bounds = map.bounds;
    let size = map.physical_size();
    let mut changes = BTreeMap::new();
    let mut platforms = map.platforms().to_vec();
    let mut dirty = frame;
    if let Some(donor) = donor {
        match donor {
            RootDonor::Tile(id, old, new) => {
                dirty = old;
                let slot = old_projection.slot(id).ok_or(Error::InvalidProjection)?;
                let tile = next.get(slot).ok_or(Error::InvalidProjection)?;
                changes.insert(
                    slot,
                    Some(RenderEntry {
                        id: slot,
                        rect: new,
                        color: crate::color(&tile.parent),
                    }),
                );
            }
            RootDonor::Directory(path, old, new) => {
                dirty = old;
                let mut donor_groups: HashMap<&Path, Vec<(usize, u64)>> = HashMap::new();
                for id in previous_geometry.subtree_tiles(&path) {
                    let slot = old_projection.slot(id).ok_or(Error::InvalidProjection)?;
                    let file = next.get(slot).ok_or(Error::InvalidProjection)?;
                    donor_groups
                        .entry(&file.parent)
                        .or_default()
                        .push((slot, file.size.max(1)));
                    changes.insert(slot, None);
                }
                if donor_groups.is_empty() {
                    return Ok(None);
                }
                let sub = directory_map::layout_subtree(
                    donor_groups,
                    root,
                    &path,
                    0,
                    new,
                    render::logical_pixel_size(bounds, size),
                    token,
                    generation,
                )
                .ok_or(Error::Cancelled)?;
                geometry = geometry
                    .install_subtree(&path, &sub.partitions, next)
                    .map_err(Error::Geometry)?;
                for entry in sub.entries {
                    changes.insert(entry.id, Some(entry));
                }
                let inside = |rect: crate::treemap::Rect| {
                    rect.x >= old.x
                        && rect.y >= old.y
                        && rect.x + rect.w <= old.x + old.w + 0.01
                        && rect.y + rect.h <= old.y + old.h + 0.01
                };
                platforms.retain(|platform| !inside(platform.bounds));
                platforms.extend(sub.platforms);
            }
        }
    }
    if let Some(tile) = root_tile {
        let slot = next.slot(tile.id).ok_or(Error::InvalidProjection)?;
        changes.insert(
            slot,
            Some(RenderEntry {
                id: slot,
                rect: frame,
                color: crate::color(&tile.parent),
            }),
        );
    } else {
        let sub = directory_map::layout_subtree(
            new_groups,
            root,
            &pivot,
            0,
            frame,
            render::logical_pixel_size(bounds, size),
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        geometry = geometry
            .install_subtree(&pivot, &sub.partitions, next)
            .map_err(Error::Geometry)?;
        for entry in sub.entries {
            changes.insert(entry.id, Some(entry));
        }
        platforms.extend(sub.platforms);
    }
    finish_local(
        map,
        next,
        old_holes,
        geometry,
        changes.into_iter().collect(),
        platforms,
        dirty,
        max_patch_pixels,
        token,
        generation,
        animate,
    )
    .map(Some)
}

fn finish_local(
    map: &RenderMap,
    next: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    geometry: IncrementalGeometry,
    changes: Vec<(usize, Option<RenderEntry>)>,
    platforms: Vec<render::Platform>,
    outer: crate::treemap::Rect,
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
) -> Result<PreparedView, Error> {
    finish_local_regions_with_animation(
        map,
        next,
        old_holes,
        geometry,
        changes,
        platforms,
        &[outer],
        max_patch_pixels,
        token,
        generation,
        animate,
    )
}

/// Rasterize separate dirty rectangles against one unpublished target. Convert
/// them to the common physical grid and subtract overlaps before generating
/// patches, so separate root subtrees do not require their bounding box.
#[cfg(test)]
pub(crate) fn finish_local_regions(
    map: &RenderMap,
    next: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    geometry: IncrementalGeometry,
    changes: Vec<(usize, Option<RenderEntry>)>,
    platforms: Vec<render::Platform>,
    outers: &[crate::treemap::Rect],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
) -> Result<PreparedView, Error> {
    finish_local_regions_with_animation(
        map,
        next,
        old_holes,
        geometry,
        changes,
        platforms,
        outers,
        max_patch_pixels,
        token,
        generation,
        true,
    )
}

pub(crate) fn finish_local_regions_with_animation(
    map: &RenderMap,
    next: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    geometry: IncrementalGeometry,
    changes: Vec<(usize, Option<RenderEntry>)>,
    platforms: Vec<render::Platform>,
    outers: &[crate::treemap::Rect],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
) -> Result<PreparedView, Error> {
    finish_local_regions_with_options(
        map,
        next,
        old_holes,
        geometry,
        changes,
        Some(platforms),
        outers,
        max_patch_pixels,
        token,
        generation,
        animate,
    )
}

fn finish_local_regions_with_options(
    map: &RenderMap,
    next: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    geometry: IncrementalGeometry,
    changes: Vec<(usize, Option<RenderEntry>)>,
    platforms: Option<Vec<render::Platform>>,
    outers: &[crate::treemap::Rect],
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    animate: bool,
) -> Result<PreparedView, Error> {
    let bounds = map.bounds;
    let size = map.physical_size();
    let halo = 8.0
        + 2.0
            * (bounds.w / size[0] as f32)
                .max(bounds.h / size[1] as f32)
                .max(0.25);
    let mut regions: Vec<[usize; 4]> = Vec::new();
    let grid = outers.len() > 16;
    let cell = [size[0].div_ceil(8).max(1), size[1].div_ceil(8).max(1)];
    let mut marked = [[false; 8]; 8];
    for (index, outer) in outers.iter().enumerate() {
        if index % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        let mut region = [
            (((outer.x - halo - bounds.x) / bounds.w * size[0] as f32).floor() as isize - 2)
                .clamp(0, size[0] as isize) as usize,
            (((outer.y - halo - bounds.y) / bounds.h * size[1] as f32).floor() as isize - 2)
                .clamp(0, size[1] as isize) as usize,
            ((((outer.x + outer.w + halo) - bounds.x) / bounds.w * size[0] as f32).ceil() as isize
                + 2)
            .clamp(0, size[0] as isize) as usize,
            ((((outer.y + outer.h + halo) - bounds.y) / bounds.h * size[1] as f32).ceil() as isize
                + 2)
            .clamp(0, size[1] as isize) as usize,
        ];
        if region[0] >= region[2] || region[1] >= region[3] {
            continue;
        }
        if grid {
            for ty in region[1] / cell[1]..=(region[3] - 1) / cell[1] {
                for tx in region[0] / cell[0]..=(region[2] - 1) / cell[0] {
                    marked[ty][tx] = true;
                }
            }
            continue;
        }
        // Merge overlapping nearby rectangles when their enclosing rectangle
        // does not greatly enlarge the changed area. Keeping a whole source
        // rectangle together preserves per-object motion sprites.
        let mut i = 0;
        while i < regions.len() {
            let old = regions[i];
            let ix = region[2].min(old[2]).saturating_sub(region[0].max(old[0]));
            let iy = region[3].min(old[3]).saturating_sub(region[1].max(old[1]));
            let union = [
                region[0].min(old[0]),
                region[1].min(old[1]),
                region[2].max(old[2]),
                region[3].max(old[3]),
            ];
            let area = |r: [usize; 4]| (r[2] - r[0]).saturating_mul(r[3] - r[1]);
            let covered = area(region)
                .saturating_add(area(old))
                .saturating_sub(ix.saturating_mul(iy));
            if ix > 0 && iy > 0 && area(union) <= covered.saturating_mul(2) {
                region = union;
                regions.swap_remove(i);
                i = 0;
            } else {
                i += 1;
            }
        }
        let mut pieces = vec![region];
        for old in &regions {
            let mut remaining = Vec::new();
            for piece in pieces {
                let x0 = piece[0].max(old[0]);
                let y0 = piece[1].max(old[1]);
                let x1 = piece[2].min(old[2]);
                let y1 = piece[3].min(old[3]);
                if x0 >= x1 || y0 >= y1 {
                    remaining.push(piece);
                    continue;
                }
                for r in [
                    [piece[0], piece[1], piece[2], y0],
                    [piece[0], y1, piece[2], piece[3]],
                    [piece[0], y0, x0, y1],
                    [x1, y0, piece[2], y1],
                ] {
                    if r[0] < r[2] && r[1] < r[3] {
                        remaining.push(r);
                    }
                }
            }
            pieces = remaining;
        }
        regions.extend(pieces);
    }
    if grid {
        for (ty, row) in marked.iter().enumerate() {
            for (tx, &dirty) in row.iter().enumerate() {
                if dirty {
                    regions.push([
                        tx * cell[0],
                        ty * cell[1],
                        ((tx + 1) * cell[0]).min(size[0]),
                        ((ty + 1) * cell[1]).min(size[1]),
                    ]);
                }
            }
        }
    }
    if regions.is_empty() {
        return Err(Error::Rebuild);
    }
    let mut target = map
        .stage_geometry_with_slots(&changes, platforms, next.len())
        .ok_or(Error::Rebuild)?;
    let moves: Vec<_> = changes
        .iter()
        .filter_map(|(slot, entry)| {
            let from = map.rect(*slot)?;
            let to = entry.as_ref()?.rect;
            (from != to).then_some((*slot, from, to))
        })
        .collect();
    let mut patches = Vec::new();
    let mut transitions = Vec::new();
    let mut holes = old_holes.to_vec();
    for region in regions {
        let region_patches = raster_pieces_indexed(
            &target.raster_index(),
            region,
            max_patch_pixels,
            token,
            generation,
        )?;
        if animate && !grid {
            transitions.extend(transitions_for_maps(
                map,
                &target,
                &region_patches,
                old_holes,
                &moves,
                region,
                max_patch_pixels,
                token,
                generation,
            )?);
        }
        holes = clip_holes(
            &holes,
            bounds,
            size,
            [region[0], region[1]],
            [region[2] - region[0], region[3] - region[1]],
        )
        .ok_or(Error::Rebuild)?;
        patches.extend(region_patches);
    }
    if animate && grid {
        // A transaction may touch millions of small rectangles; keep the
        // animation to at most the same 64 grid cells regardless of batch size.
        let mut accumulator = PatchAccumulator::new(map, old_holes, token, generation)?;
        accumulator.push(&patches)?;
        let (_, bounded_transitions) = accumulator.finish(&target)?;
        transitions = bounded_transitions;
    }
    // A sprite selected in one region can move into another. Exclude every
    // selected owner from *all* region backgrounds, and let the UI draw all
    // sprites after every region background. Otherwise the destination would
    // show a second copy or cover the moving sprite.
    if !grid && transitions.len() > 1 {
        let selected: std::collections::HashSet<_> = transitions
            .iter()
            .flat_map(|transition| transition.tiles.iter().map(|tile| tile.id))
            .collect();
        if !selected.is_empty() {
            let old_index = map.raster_index();
            let target_index = target.raster_index();
            for transition in &mut transitions {
                let [x, y] = transition.origin;
                let [w, h] = transition.previous.size;
                let region = [x, y, x + w, y + h];
                let mut previous = raster_index_region(
                    &old_index,
                    region,
                    max_patch_pixels,
                    token,
                    generation,
                    |entry| map.rect(entry.id).is_some() && !selected.contains(&entry.id),
                )
                .ok_or(Error::Cancelled)?;
                mask_old_holes(&mut previous, old_holes, bounds, size);
                let background = raster_index_region(
                    &target_index,
                    region,
                    max_patch_pixels,
                    token,
                    generation,
                    |entry| !selected.contains(&entry.id),
                )
                .ok_or(Error::Cancelled)?;
                transition.previous = previous.image;
                transition.background = Some(background.image);
            }
        }
    }
    if !target.apply_scene_patches(&patches, &[], None) {
        return Err(Error::Rebuild);
    }
    if token.load(Ordering::Relaxed) != generation {
        return Err(Error::Cancelled);
    }
    Ok(PreparedView {
        geometry,
        projection: next.clone(),
        map: target,
        patches,
        holes,
        transitions,
    })
}

fn fixed_root_fold(
    previous_geometry: &IncrementalGeometry,
    old_parts: &[crate::directory_map::DirectoryPartition],
    old_projection: &SparseProjection,
    next: &SparseProjection,
    groups: &std::collections::HashMap<&std::path::Path, Vec<(usize, u64)>>,
    map: &RenderMap,
    scan_root: &std::path::Path,
    deltas: &[TileDelta],
    size: [usize; 2],
    token: &AtomicU64,
    generation: u64,
) -> Result<Option<(crate::directory_map::DirectoryLayout, IncrementalGeometry)>, Error> {
    use crate::{
        directory_map::{self, PartitionItemId},
        scene_model::TileId,
    };
    let mut candidate = None;
    for delta in deltas {
        let change = match (&delta.before, &delta.after) {
            (Some(tile), None)
                if matches!(tile.id, TileId::Directory(_))
                    && tile.path.parent() == Some(scan_root) =>
            {
                Some((tile, true))
            }
            (None, Some(tile))
                if matches!(tile.id, TileId::Directory(_))
                    && tile.path.parent() == Some(scan_root) =>
            {
                Some((tile, false))
            }
            _ => None,
        };
        if let Some(change) = change {
            if candidate.is_some() {
                return Ok(None);
            }
            candidate = Some(change);
        }
    }
    let Some((folder, expanding)) = candidate else {
        return Ok(None);
    };
    let pivot = folder.path.as_path();
    if deltas
        .iter()
        .flat_map(|delta| delta.before.iter().chain(delta.after.iter()))
        .any(|tile| !tile.path.starts_with(pivot))
    {
        return Ok(None);
    }
    let old_slot = expanding.then(|| old_projection.slot(folder.id)).flatten();
    let new_slot = (!expanding).then(|| next.slot(folder.id)).flatten();
    if expanding && old_slot.is_none() || !expanding && new_slot.is_none() {
        return Ok(None);
    }
    let mut parts = old_parts.to_vec();
    let Some((parent_idx, item_idx)) = parts.iter().enumerate().find_map(|(pi, part)| {
        part.items
            .iter()
            .position(|item| match (&item.id, expanding) {
                (PartitionItemId::File(slot), true) => Some(*slot) == old_slot,
                (PartitionItemId::Directory(path), false) => path.as_path() == pivot,
                _ => false,
            })
            .map(|ii| (pi, ii))
    }) else {
        return Ok(None);
    };
    if parts[parent_idx].path.as_deref() != Some(scan_root) {
        return Ok(None);
    }
    let outer = parts[parent_idx].items[item_idx].rect;
    let mut entries: Vec<_> = map
        .entries()
        .iter()
        .copied()
        .filter(|entry| {
            map.rect(entry.id).is_some()
                && Some(entry.id) != old_slot
                && old_projection
                    .get(entry.id)
                    .is_some_and(|file| !file.parent.starts_with(pivot))
        })
        .collect();
    let mut replacement_parts = Vec::new();
    let weight;
    if expanding {
        let subtree: Vec<_> = groups
            .iter()
            .filter(|(path, _)| path.starts_with(pivot))
            .map(|(path, items)| (*path, items.clone()))
            .collect();
        if subtree.is_empty() {
            return Ok(None);
        }
        weight = subtree
            .iter()
            .flat_map(|(_, items)| items.iter())
            .fold(0u64, |sum, (_, value)| sum.saturating_add((*value).max(1)));
        parts[parent_idx].items[item_idx].id =
            PartitionItemId::Directory(Box::new(pivot.to_path_buf()));
        parts[parent_idx].items[item_idx].weight = weight;
        let sub = directory_map::layout_subtree(
            subtree,
            scan_root,
            pivot,
            0,
            outer,
            render::logical_pixel_size(map.bounds, size),
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        entries.extend(sub.entries);
        replacement_parts = sub.partitions.clone();
        parts.extend(sub.partitions);
    } else {
        weight = folder.size.max(1);
        let slot = new_slot.expect("checked above");
        parts[parent_idx].items[item_idx].id = PartitionItemId::File(slot);
        parts[parent_idx].items[item_idx].weight = weight;
        parts.retain(|part| {
            part.path
                .as_ref()
                .is_none_or(|path| !path.starts_with(pivot))
        });
        entries.push(RenderEntry {
            id: slot,
            rect: outer,
            color: crate::color(&folder.parent),
        });
    }
    parts.sort_by(|a, b| {
        a.path
            .as_ref()
            .map_or(0, |p| p.components().count())
            .cmp(&b.path.as_ref().map_or(0, |p| p.components().count()))
            .then_with(|| a.path.cmp(&b.path))
    });
    let geometry = previous_geometry
        .replace_root_fold(
            scan_root,
            pivot,
            expanding.then_some(folder.id),
            (!expanding).then_some((folder.id, weight)),
            &replacement_parts,
            next,
            weight,
        )
        .map_err(Error::Geometry)?;
    let platforms = parts.iter().filter_map(|part| part.platform).collect();
    Ok(Some((
        directory_map::DirectoryLayout {
            entries,
            platforms,
            partitions: Vec::new(),
        },
        geometry,
    )))
}

/// Independent root-child folds can be prepared one subtree at a time while
/// retaining a single published frame. File moves spanning children remain a
/// single root transaction because splitting them would break identity and
/// projection atomicity.
fn split_root_folds(
    deltas: &[TileDelta],
    scan_root: &std::path::Path,
) -> Option<Vec<(std::path::PathBuf, Vec<TileDelta>)>> {
    use crate::scene_model::TileId;
    use std::collections::{BTreeMap, BTreeSet};
    let pivots: BTreeSet<_> = deltas
        .iter()
        .filter_map(|delta| match (&delta.before, &delta.after) {
            (Some(tile), None) | (None, Some(tile))
                if matches!(tile.id, TileId::Directory(_))
                    && tile.path.parent() == Some(scan_root) =>
            {
                Some(tile.path.clone())
            }
            _ => None,
        })
        .collect();
    if pivots.len() < 2 {
        return None;
    }
    let mut groups: BTreeMap<_, Vec<TileDelta>> =
        pivots.iter().cloned().map(|p| (p, Vec::new())).collect();
    for delta in deltas {
        let mut owner = None;
        for tile in delta.before.iter().chain(delta.after.iter()) {
            let pivot = pivots.iter().find(|pivot| tile.path.starts_with(pivot))?;
            if owner.is_some_and(|previous| previous != pivot) {
                return None;
            }
            owner = Some(pivot);
        }
        groups.get_mut(owner?)?.push(delta.clone());
    }
    Some(groups.into_iter().collect())
}

fn split_root_additions(
    deltas: &[TileDelta],
    scan_root: &std::path::Path,
) -> Option<Vec<(std::path::PathBuf, Vec<TileDelta>)>> {
    use std::collections::BTreeMap;
    if deltas
        .iter()
        .any(|delta| delta.before.is_some() || delta.after.is_none())
    {
        return None;
    }
    let mut groups = BTreeMap::<std::path::PathBuf, Vec<TileDelta>>::new();
    for delta in deltas {
        let path = &delta.after.as_ref()?.path;
        let relative = path.strip_prefix(scan_root).ok()?;
        let component = relative.components().next()?;
        let pivot = scan_root.join(component.as_os_str());
        groups.entry(pivot).or_default().push(delta.clone());
    }
    (groups.len() > 1).then(|| groups.into_iter().collect())
}

/// Allocate a new immediate root child by splitting one existing allocation.
/// Only that donor and the new subtree are laid out again; all other root
/// children retain their outer frames. If no prior allocation exists this
/// returns None so the empty-scene path can initialize its first geometry.
fn fixed_root_addition(
    old_parts: &[crate::directory_map::DirectoryPartition],
    old_projection: &SparseProjection,
    next: &SparseProjection,
    groups: &std::collections::HashMap<&std::path::Path, Vec<(usize, u64)>>,
    slots: &[Option<crate::scene_model::TileId>],
    map: &RenderMap,
    scan_root: &std::path::Path,
    deltas: &[TileDelta],
    size: [usize; 2],
    token: &AtomicU64,
    generation: u64,
) -> Result<Option<(crate::directory_map::DirectoryLayout, IncrementalGeometry)>, Error> {
    use crate::directory_map::{self, PartitionItem, PartitionItemId};
    use crate::treemap::Rect;
    if deltas.is_empty()
        || deltas
            .iter()
            .any(|delta| delta.before.is_some() || delta.after.is_none())
    {
        return Ok(None);
    }
    let first = &deltas[0].after.as_ref().expect("checked").path;
    let Ok(relative) = first.strip_prefix(scan_root) else {
        return Ok(None);
    };
    let Some(first_component) = relative.components().next() else {
        return Ok(None);
    };
    let pivot = scan_root.join(first_component.as_os_str());
    if deltas.iter().any(|delta| {
        !delta
            .after
            .as_ref()
            .expect("checked")
            .path
            .starts_with(&pivot)
    }) || old_parts.iter().any(|part| {
        part.path
            .as_ref()
            .is_some_and(|path| path.starts_with(&pivot))
    }) || map.entries().iter().any(|entry| {
        map.rect(entry.id).is_some()
            && old_projection
                .get(entry.id)
                .is_some_and(|file| file.path.starts_with(&pivot))
    }) {
        return Ok(None);
    }
    let mut parts = old_parts.to_vec();
    let Some(root_index) = parts
        .iter()
        .position(|part| part.path.as_deref() == Some(scan_root))
    else {
        return Ok(None);
    };
    let root_tile = deltas
        .iter()
        .filter_map(|delta| delta.after.as_ref())
        .find(|tile| tile.path == pivot && tile.parent == scan_root);
    let subtree: Vec<_> = groups
        .iter()
        .filter(|(path, _)| path.starts_with(&pivot))
        .map(|(path, items)| (*path, items.clone()))
        .collect();
    if root_tile.is_some() == !subtree.is_empty() {
        return Ok(None);
    }
    let new_weight = if let Some(tile) = root_tile {
        tile.size.max(1)
    } else {
        subtree
            .iter()
            .flat_map(|(_, items)| items.iter())
            .fold(0u64, |sum, (_, value)| sum.saturating_add((*value).max(1)))
    };
    let mut entries: Vec<_> = map
        .entries()
        .iter()
        .copied()
        .filter(|entry| map.rect(entry.id).is_some())
        .collect();
    let new_rect = if let Some(vacancy) = parts[root_index].vacancies.pop() {
        vacancy
    } else {
        let Some((donor_idx, donor)) = parts[root_index]
            .items
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| (a.rect.w * a.rect.h).total_cmp(&(b.rect.w * b.rect.h)))
            .map(|(index, item)| (index, item.clone()))
        else {
            return Ok(None);
        };
        let fraction = (new_weight as f64 / (new_weight as f64 + donor.weight.max(1) as f64))
            .clamp(0.05, 0.95) as f32;
        let (donor_rect, new_rect) = if donor.rect.w >= donor.rect.h {
            let keep = donor.rect.w * (1.0 - fraction);
            (
                Rect {
                    w: keep,
                    ..donor.rect
                },
                Rect {
                    x: donor.rect.x + keep,
                    w: donor.rect.w - keep,
                    ..donor.rect
                },
            )
        } else {
            let keep = donor.rect.h * (1.0 - fraction);
            (
                Rect {
                    h: keep,
                    ..donor.rect
                },
                Rect {
                    y: donor.rect.y + keep,
                    h: donor.rect.h - keep,
                    ..donor.rect
                },
            )
        };
        parts[root_index].items[donor_idx].rect = donor_rect;
        match &donor.id {
            PartitionItemId::File(slot) => {
                let Some(entry) = entries.iter_mut().find(|entry| entry.id == *slot) else {
                    return Ok(None);
                };
                entry.rect = donor_rect;
            }
            PartitionItemId::Directory(path) => {
                let donor_path = path.as_path();
                let donor_groups: Vec<_> = groups
                    .iter()
                    .filter(|(path, _)| path.starts_with(donor_path))
                    .map(|(path, items)| (*path, items.clone()))
                    .collect();
                if donor_groups.is_empty() {
                    return Ok(None);
                }
                entries.retain(|entry| {
                    old_projection
                        .get(entry.id)
                        .is_some_and(|file| !file.parent.starts_with(donor_path))
                });
                parts.retain(|part| {
                    part.path
                        .as_ref()
                        .is_none_or(|path| !path.starts_with(donor_path))
                });
                let subtree = directory_map::layout_subtree(
                    donor_groups,
                    scan_root,
                    donor_path,
                    0,
                    donor_rect,
                    render::logical_pixel_size(map.bounds, size),
                    token,
                    generation,
                )
                .ok_or(Error::Cancelled)?;
                entries.extend(subtree.entries);
                parts.extend(subtree.partitions);
            }
        }
        new_rect
    };
    let new_id = if let Some(tile) = root_tile {
        let slot = next.slot(tile.id).ok_or(Error::InvalidProjection)?;
        entries.push(RenderEntry {
            id: slot,
            rect: new_rect,
            color: crate::color(&tile.parent),
        });
        PartitionItemId::File(slot)
    } else {
        let subtree = directory_map::layout_subtree(
            subtree,
            scan_root,
            &pivot,
            0,
            new_rect,
            render::logical_pixel_size(map.bounds, size),
            token,
            generation,
        )
        .ok_or(Error::Cancelled)?;
        entries.extend(subtree.entries);
        parts.extend(subtree.partitions);
        PartitionItemId::Directory(Box::new(pivot))
    };
    let Some(root_index) = parts
        .iter()
        .position(|part| part.path.as_deref() == Some(scan_root))
    else {
        return Ok(None);
    };
    parts[root_index].items.push(PartitionItem {
        id: new_id,
        rect: new_rect,
        weight: new_weight,
    });
    parts.sort_by(|a, b| {
        a.path
            .as_ref()
            .map_or(0, |p| p.components().count())
            .cmp(&b.path.as_ref().map_or(0, |p| p.components().count()))
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut geometry =
        IncrementalGeometry::from_layout_optional(&parts, slots).map_err(Error::Geometry)?;
    geometry
        .restore_tile_layout_parents(&parts, next)
        .map_err(Error::Geometry)?;
    let platforms = parts.iter().filter_map(|part| part.platform).collect();
    Ok(Some((
        directory_map::DirectoryLayout {
            entries,
            platforms,
            partitions: Vec::new(),
        },
        geometry,
    )))
}

/// Recompute the directory hierarchy in the background when a change affects
/// terrace topology. Only the changed pixels and their lighting halo are
/// rasterized and uploaded; the previously displayed texture stays live.
/// Large dirty regions are rasterized as bounded, independently uploaded tiles.
#[cfg(test)]
pub fn prepare_structural(
    previous_geometry: &IncrementalGeometry,
    map: RenderMap,
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
    prepare_structural_impl(
        previous_geometry,
        map,
        old_projection,
        old_holes,
        deltas,
        requested_scope,
        pixels_per_point,
        area_tolerance,
        max_patch_pixels,
        token,
        generation,
        None,
        true,
        true,
    )
}

/// Structural deltas originating in the actor include a proof of every
/// changed directory fact, including empty directories without visible tiles.
pub fn prepare_structural_proven(
    previous_geometry: &IncrementalGeometry,
    map: RenderMap,
    old_projection: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    requested_scope: &std::path::Path,
    pixels_per_point: f32,
    area_tolerance: f64,
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    structural_paths: &[std::path::PathBuf],
    root_facts_touched: bool,
    root_identity_stable: bool,
    visual_noop_paths: &[std::path::PathBuf],
) -> Result<PreparedView, Error> {
    prepare_structural_impl(
        previous_geometry,
        map,
        old_projection,
        old_holes,
        deltas,
        requested_scope,
        pixels_per_point,
        area_tolerance,
        max_patch_pixels,
        token,
        generation,
        Some((
            structural_paths,
            root_facts_touched,
            root_identity_stable,
            visual_noop_paths,
        )),
        true,
        true,
    )
}

/// Private intermediate batches update geometry and pixels without building
/// animation assets; the outer PatchAccumulator animates only the final frame.
pub fn prepare_structural_proven_batched(
    previous_geometry: &IncrementalGeometry,
    map: RenderMap,
    old_projection: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    requested_scope: &std::path::Path,
    pixels_per_point: f32,
    area_tolerance: f64,
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    structural_paths: &[std::path::PathBuf],
    root_facts_touched: bool,
    root_identity_stable: bool,
    visual_noop_paths: &[std::path::PathBuf],
) -> Result<PreparedView, Error> {
    prepare_structural_impl(
        previous_geometry,
        map,
        old_projection,
        old_holes,
        deltas,
        requested_scope,
        pixels_per_point,
        area_tolerance,
        max_patch_pixels,
        token,
        generation,
        Some((
            structural_paths,
            root_facts_touched,
            root_identity_stable,
            visual_noop_paths,
        )),
        true,
        false,
    )
}

/// Process an actor-validated structural update, optionally initializing a
/// replaced scan root when the actor proved its old tree was fully empty.
/// The root identity is not treated as stable; this special case only uses
/// the previous blank canvas and creates the new projection in one step.
pub fn prepare_structural_with_empty_root_proof(
    previous_geometry: &IncrementalGeometry,
    map: RenderMap,
    old_projection: &SparseProjection,
    old_holes: &[crate::treemap::Rect],
    deltas: &[TileDelta],
    requested_scope: &std::path::Path,
    pixels_per_point: f32,
    area_tolerance: f64,
    max_patch_pixels: usize,
    token: &AtomicU64,
    generation: u64,
    structural_paths: &[std::path::PathBuf],
    root_facts_touched: bool,
    root_identity_stable: bool,
    visual_noop_paths: &[std::path::PathBuf],
    empty_root_replacement_safe: bool,
    animate: bool,
) -> Result<PreparedView, Error> {
    let root = &old_projection.base().root;
    if empty_root_replacement_safe
        && old_projection.is_empty()
        && structural_paths.iter().all(|path| path.starts_with(root))
        && !deltas.is_empty()
        && deltas
            .iter()
            .all(|delta| delta.before.is_none() && delta.after.is_some())
    {
        let mut next = old_projection.clone();
        let after = deltas
            .iter()
            .filter_map(|delta| delta.after.as_ref().map(|tile| (tile.id, tile.to_entry())))
            .collect();
        next.replace_from_scene(&[], after)
            .map_err(|_| Error::InvalidProjection)?;
        if let Some(prepared) = crate::scan_root_fold::prepare_empty(
            previous_geometry,
            &map,
            old_projection,
            &next,
            old_holes,
            deltas,
            max_patch_pixels,
            token,
            generation,
            animate,
        )? {
            return Ok(prepared);
        }
    }
    prepare_structural_impl(
        previous_geometry,
        map,
        old_projection,
        old_holes,
        deltas,
        requested_scope,
        pixels_per_point,
        area_tolerance,
        max_patch_pixels,
        token,
        generation,
        Some((
            structural_paths,
            root_facts_touched,
            root_identity_stable,
            visual_noop_paths,
        )),
        true,
        animate,
    )
}

fn prepare_structural_impl(
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
    proof: Option<(&[std::path::PathBuf], bool, bool, &[std::path::PathBuf])>,
    allow_split: bool,
    animate: bool,
) -> Result<PreparedView, Error> {
    use crate::{directory_map, treemap::Rect};
    use std::collections::{BTreeSet, HashMap};
    use std::path::Path;

    if token.load(Ordering::Relaxed) != generation {
        return Err(Error::Cancelled);
    }
    let root = &old_projection.base().root;
    // Replacing an empty scan root changes its filesystem identity but leaves
    // this mode's visible projection empty. Keep the existing blank canvas;
    // the coordinator still atomically commits the actor's new root version.
    if old_projection.is_empty()
        && deltas.is_empty()
        && proof.is_some_and(|(paths, _, _, _)| paths.iter().all(|path| path.starts_with(root)))
    {
        return Ok(PreparedView {
            geometry: previous_geometry.clone(),
            projection: old_projection.clone(),
            map,
            patches: Vec::new(),
            holes: old_holes.to_vec(),
            transitions: Vec::new(),
        });
    }
    let groups = if allow_split {
        split_root_folds(deltas, root)
            .or_else(|| split_root_additions(deltas, root))
            .map(|groups| {
                groups
                    .into_iter()
                    .map(|(scope, deltas)| crate::structural_split::RootChangeGroup {
                        scope,
                        deltas,
                    })
                    .collect::<Vec<_>>()
            })
            .or_else(|| crate::structural_split::independent_root_changes(root, deltas))
            .filter(|groups| {
                proof.is_some_and(|(paths, touched, stable, noops)| {
                    stable
                        && paths.iter().all(|path| path.starts_with(root))
                        && (crate::structural_split::covers_directory_facts(
                            root, groups, paths, touched,
                        ) || crate::structural_split::covers_directory_facts_relaxed(
                            root, groups, paths, stable, noops,
                        ))
                })
            })
    } else {
        None
    };
    if let Some(groups) = groups {
        let original = map.snapshot_for_update();
        let mut accumulator = PatchAccumulator::new(&original, old_holes, token, generation)?;
        let mut working_map = map;
        let mut working_geometry = previous_geometry.clone();
        let mut working_projection = old_projection.clone();
        let mut working_holes = old_holes.to_vec();
        for group in groups {
            let group_paths = proof.map(|(paths, _, _, _)| {
                paths
                    .iter()
                    .filter(|path| {
                        let component = path
                            .strip_prefix(root)
                            .ok()
                            .and_then(|p| p.components().next());
                        group
                            .deltas
                            .iter()
                            .flat_map(|d| d.before.iter().chain(d.after.iter()))
                            .any(|tile| {
                                let tile_component = tile
                                    .path
                                    .strip_prefix(root)
                                    .ok()
                                    .and_then(|p| p.components().next());
                                component.is_some() && component == tile_component
                            })
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            });
            let group_proof = proof.and_then(|(_, _, stable, noops)| {
                group_paths
                    .as_deref()
                    .map(|paths| (paths, false, stable, noops))
            });
            let topology_unchanged = proof.is_some_and(|(paths, _, _, _)| {
                !paths.iter().any(|path| {
                    group
                        .deltas
                        .iter()
                        .flat_map(|d| d.before.iter().chain(d.after.iter()))
                        .any(|tile| {
                            let a = path
                                .strip_prefix(root)
                                .ok()
                                .and_then(|p| p.components().next());
                            let b = tile
                                .path
                                .strip_prefix(root)
                                .ok()
                                .and_then(|p| p.components().next());
                            a.is_some() && a == b
                        })
                })
            });
            let updated = if crate::structural_split::can_prepare_files(
                &working_geometry,
                &group.deltas,
                topology_unchanged,
            ) {
                // Preserve the unpublished base snapshot: compacting can reject a
                // nested move or a tight error budget after inspecting geometry.
                // Such rejection needs a local structural update, not a lost batch.
                match prepare_batched(
                    working_geometry.clone(),
                    working_map.snapshot_for_update(),
                    &working_projection,
                    &working_holes,
                    &group.deltas,
                    pixels_per_point,
                    area_tolerance,
                    max_patch_pixels,
                    token,
                    generation,
                ) {
                    Ok(updated) => updated,
                    Err(
                        Error::Rebuild
                        | Error::Geometry(
                            incremental_geometry::Error::Rebuild
                            | incremental_geometry::Error::MissingPartition,
                        ),
                    ) => prepare_structural_impl(
                        &working_geometry,
                        working_map,
                        &working_projection,
                        &working_holes,
                        &group.deltas,
                        &group.scope,
                        pixels_per_point,
                        area_tolerance,
                        max_patch_pixels,
                        token,
                        generation,
                        group_proof,
                        false,
                        animate,
                    )?,
                    Err(error) => return Err(error),
                }
            } else {
                prepare_structural_impl(
                    &working_geometry,
                    working_map,
                    &working_projection,
                    &working_holes,
                    &group.deltas,
                    &group.scope,
                    pixels_per_point,
                    area_tolerance,
                    max_patch_pixels,
                    token,
                    generation,
                    group_proof,
                    false,
                    animate,
                )?
            };
            accumulator.push(&updated.patches)?;
            working_map = updated.map;
            working_geometry = updated.geometry;
            working_projection = updated.projection;
            working_holes = updated.holes;
        }
        let (patches, transitions) = if animate {
            accumulator.finish(&working_map)?
        } else {
            (accumulator.finish_without_animations(), Vec::new())
        };
        for patch in &patches {
            working_holes = clip_holes(
                &working_holes,
                working_map.bounds,
                working_map.physical_size(),
                patch.origin,
                patch.image.size,
            )
            .ok_or(Error::Rebuild)?;
        }
        return Ok(PreparedView {
            geometry: working_geometry,
            projection: working_projection,
            map: working_map,
            patches,
            holes: working_holes,
            transitions,
        });
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
    if proof.is_some() {
        next.replace_from_scene(&before, after)
    } else {
        next.replace(&before, after)
    }
    .map_err(|_| Error::InvalidProjection)?;
    // A local subtree may consume only the directory facts proved to lie in
    // that subtree. Facts with no projected tile still participate in this
    // check; otherwise they could silently be left stale.
    let fact_pivots = |allowed: &[std::path::PathBuf]| {
        proof.is_some_and(|(paths, touched, stable, noops)| {
            let root_aggregate_unchanged = !deltas
                .iter()
                .flat_map(|d| d.before.iter().chain(d.after.iter()))
                .any(|tile| tile.path == *root);
            let relaxed = stable && root_aggregate_unchanged;
            stable
                && (!touched || relaxed)
                && paths.iter().all(|path| {
                    path.starts_with(root)
                        && (allowed.iter().any(|pivot| path.starts_with(pivot))
                            || (relaxed && (path == root || noops.contains(path))))
                })
        })
    };
    let pivot = |path: &std::path::Path| -> Option<std::path::PathBuf> {
        let component = path.strip_prefix(root).ok()?.components().next()?;
        Some(root.join(component.as_os_str()))
    };
    if proof.is_some_and(|(paths, _, stable, _)| {
        stable && paths.iter().all(|path| path.starts_with(root))
    }) {
        if let Some(prepared) = crate::scan_root_fold::prepare_empty(
            previous_geometry,
            &map,
            old_projection,
            &next,
            old_holes,
            deltas,
            max_patch_pixels,
            token,
            generation,
            animate,
        )? {
            return Ok(prepared);
        }
        if let Some(prepared) = crate::scan_root_fold::prepare(
            previous_geometry,
            &map,
            old_projection,
            &next,
            old_holes,
            deltas,
            max_patch_pixels,
            token,
            generation,
            animate,
        )? {
            return Ok(prepared);
        }
    }
    if let Some((paths, touched, stable, noops)) = proof
        && paths.iter().all(|path| path.starts_with(root))
    {
        if let Some(prepared) = crate::deep_scope::prepare(
            previous_geometry,
            &map,
            old_projection,
            &next,
            old_holes,
            deltas,
            requested_scope,
            paths,
            touched,
            stable,
            noops,
            max_patch_pixels,
            token,
            generation,
            animate,
        )? {
            return Ok(prepared);
        }
    }
    // The actor supplies the complete before/after projection difference for
    // this specific mode at the same scene version. Therefore directory facts
    // in other root branches need no geometry change when they yielded no
    // projected delta; their model state is still committed by the actor.
    // A dedicated transaction above handles root fold/unfold; the generic
    // path also supports root-aggregate updates in mixed transactions.
    let transaction_proven = proof.is_some_and(|(paths, _, stable, _)| {
        stable
            && paths.iter().all(|path| path.starts_with(root))
            && deltas
                .iter()
                .flat_map(|d| d.before.iter().chain(d.after.iter()))
                .all(|tile| tile.path.starts_with(root))
    });
    if transaction_proven {
        if let Some(prepared) = crate::root_transaction::prepare(
            previous_geometry,
            &map,
            old_projection,
            &next,
            old_holes,
            deltas,
            max_patch_pixels,
            token,
            generation,
            animate,
        )? {
            return Ok(prepared);
        }
    }
    if let Some(first) = deltas.first().and_then(|delta| delta.before.as_ref())
        && let Some(source) = pivot(&first.path)
    {
        if deltas.iter().all(|delta| delta.after.is_none())
            && fact_pivots(std::slice::from_ref(&source))
        {
            if let Some(prepared) = fast_root_removal(
                previous_geometry,
                &map,
                old_projection,
                &next,
                old_holes,
                deltas,
                max_patch_pixels,
                token,
                generation,
                animate,
            )? {
                return Ok(prepared);
            }
        }
    }
    let local_pivots: Option<std::collections::BTreeSet<_>> = deltas
        .iter()
        .flat_map(|delta| delta.before.iter().chain(delta.after.iter()))
        .map(|tile| pivot(&tile.path))
        .collect();
    let fast_proven = local_pivots.is_some_and(|pivots| {
        pivots.len() == 1 && fact_pivots(&pivots.into_iter().collect::<Vec<_>>())
    });
    if fast_proven {
        if let Some(prepared) = fast_root_replacement(
            previous_geometry,
            &map,
            old_projection,
            &next,
            old_holes,
            deltas,
            max_patch_pixels,
            token,
            generation,
            animate,
        )? {
            return Ok(prepared);
        }
        if let Some(prepared) = fast_root_fold(
            previous_geometry,
            &map,
            old_projection,
            &next,
            old_holes,
            deltas,
            max_patch_pixels,
            token,
            generation,
            animate,
        )? {
            return Ok(prepared);
        }
        if let Some(prepared) = fast_root_addition(
            previous_geometry,
            &map,
            old_projection,
            &next,
            old_holes,
            deltas,
            max_patch_pixels,
            token,
            generation,
            animate,
        )? {
            return Ok(prepared);
        }
    }
    #[cfg(test)]
    record_general_fallback(root);
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
    let bounds = map.bounds;
    let size = map.physical_size();
    let scan_root = &next.base().root;
    if !requested_scope.starts_with(scan_root) && !scan_root.starts_with(requested_scope) {
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
            // A projected tile for the scan root itself is placed in its
            // parent (for example in folder mode). Permit that shared
            // ancestor, but never silently combine unrelated paths.
            if scope == *scan_root && !scan_root.starts_with(&tile.parent) {
                return Err(Error::Rebuild);
            }
            scope = scope.parent().ok_or(Error::Rebuild)?.to_path_buf();
        }
    }
    // A root-level hybrid aggregate can expand into a child directory (or
    // fold back) without reallocating its siblings. Reuse the aggregate's
    // original allocation as that child's outer frame, and change only the
    // affected subtree. Its temporary area error is preferable to moving the
    // whole root merely to restore exact proportions.
    let fixed_root = if fast_proven {
        let fold = fixed_root_fold(
            previous_geometry,
            &old_parts,
            old_projection,
            &next,
            &groups,
            &map,
            scan_root,
            deltas,
            size,
            token,
            generation,
        )?;
        if fold.is_some() {
            fold
        } else {
            fixed_root_addition(
                &old_parts,
                old_projection,
                &next,
                &groups,
                &slots,
                &map,
                scan_root,
                deltas,
                size,
                token,
                generation,
            )?
        }
    } else {
        None
    };
    let (layout, geometry) = if let Some(fixed) = fixed_root {
        fixed
    } else {
        loop {
            // The scope can be the root's parent when the root itself is a
            // projected folder tile. In that case all projected groups, including
            // those immediately above the scan root, must be kept together.
            if scope != *scan_root && scan_root.starts_with(&scope) {
                #[cfg(test)]
                record_root_fallback(scan_root);
                let root_layout = directory_map::layout_with_partitions(
                    groups
                        .iter()
                        .map(|(path, entries)| (*path, entries.clone())),
                    scan_root,
                    bounds,
                    render::logical_pixel_size(bounds, size),
                    token,
                    generation,
                )
                .ok_or(Error::Cancelled)?;
                let geometry =
                    IncrementalGeometry::from_layout_optional(&root_layout.partitions, &slots)
                        .map_err(Error::Geometry)?;
                break (root_layout, geometry);
            }
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
                let mut candidate = IncrementalGeometry::from_layout_optional(&parts, &slots)
                    .map_err(Error::Geometry)?;
                candidate
                    .restore_tile_layout_parents(&parts, &next)
                    .map_err(Error::Geometry)?;
                if candidate.ancestor_area_error(&scope, pixels_per_point)
                    <= area_tolerance.max(0.0)
                    || scope == *requested_scope && scope != *scan_root
                {
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
                // There may be no exported partition for a newly populated root,
                // or no surviving entries at all after a subtree was removed. In
                // either case the published canvas is still valid: lay out the
                // new root state in its existing bounds and let the patch stage
                // update its pixels and owners. Keeping the synthetic ancestor
                // produced by the regular layout also makes a later insertion into
                // an empty scene representable.
                #[cfg(test)]
                record_root_fallback(scan_root);
                let root_layout = directory_map::layout_with_partitions(
                    groups
                        .iter()
                        .map(|(path, entries)| (*path, entries.clone())),
                    scan_root,
                    bounds,
                    render::logical_pixel_size(bounds, size),
                    token,
                    generation,
                )
                .ok_or(Error::Cancelled)?;
                let geometry =
                    IncrementalGeometry::from_layout_optional(&root_layout.partitions, &slots)
                        .map_err(Error::Geometry)?;
                break (root_layout, geometry);
            }
            scope = scope.parent().ok_or(Error::Rebuild)?.to_path_buf();
        }
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
            patches: Vec::new(),
            holes: old_holes.to_vec(),
            transitions: Vec::new(),
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
    if region[0] >= region[2] || region[1] >= region[3] {
        return Err(Error::Rebuild);
    }
    let patches = raster_pieces(
        &layout.entries,
        &layout.platforms,
        bounds,
        size,
        region,
        max_patch_pixels,
        token,
        generation,
    )?;
    let moves: Vec<_> = changed
        .iter()
        .filter_map(|(slot, new)| {
            let from = old.get(slot)?.rect;
            let to = new.as_ref()?.rect;
            (from != to).then_some((*slot, from, to))
        })
        .collect();
    let transitions = if animate {
        transitions_for_patches(
            &map,
            &patches,
            old_holes,
            &layout.entries,
            &layout.platforms,
            &moves,
            region,
            max_patch_pixels,
            token,
            generation,
        )?
    } else {
        Vec::new()
    };
    let holes = clip_holes(
        old_holes,
        bounds,
        size,
        [region[0], region[1]],
        [region[2] - region[0], region[3] - region[1]],
    )
    .ok_or(Error::Rebuild)?;
    if !map.apply_scene_patches(&patches, &changed, Some(layout.platforms)) {
        return Err(Error::Rebuild);
    }
    if token.load(Ordering::Relaxed) != generation {
        return Err(Error::Cancelled);
    }
    Ok(PreparedView {
        geometry,
        projection: next,
        map,
        patches,
        holes,
        transitions,
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
        let root_expanded = prepare_structural(
            &geometry,
            map.snapshot_for_update(),
            &projection,
            &[],
            &[TileDelta {
                before: Some(old_x.clone()),
                after: None,
            }],
            Path::new("/disk/a"),
            2.0,
            0.0,
            size[0] * size[1],
            &token,
            1,
        )
        .expect("the update can expand to the root without rebuilding the map");
        assert!(!root_expanded.patches.is_empty());
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
        let patch = updated.patches.first().unwrap();
        assert!(patch.image.size[0] * patch.image.size[1] < size[0] * size[1]);
        assert_eq!(updated.map.rect(2), unaffected);
        assert!(!updated.transitions.is_empty());
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
        let all_entries: Vec<_> = updated.map.entries().iter().copied().collect();
        let full = render::raster_patch(
            &all_entries,
            updated.map.platforms(),
            bounds,
            size,
            [0, 0, size[0], size[1]],
            &token,
            1,
        )
        .unwrap();
        // A dirty area larger than the per-patch budget is tiled rather than
        // rejected. Every tile is staged before publication, and the combined
        // result remains identical to an independent full-scene raster.
        let tiled = prepare_structural(
            &geometry,
            map.snapshot_for_update(),
            &projection,
            &[],
            &[TileDelta {
                before: Some(scene.tile(slots[0]).unwrap()),
                after: None,
            }],
            Path::new("/disk/a"),
            2.0,
            40000.0,
            97,
            &token,
            1,
        )
        .unwrap();
        assert!(tiled.patches.len() > 1);
        assert!(
            tiled
                .patches
                .iter()
                .all(|piece| { piece.image.size[0] * piece.image.size[1] <= 97 })
        );
        let combined = RasterPatch::assemble(
            [
                patch.origin[0],
                patch.origin[1],
                patch.origin[0] + patch.image.size[0],
                patch.origin[1] + patch.image.size[1],
            ],
            &tiled.patches,
        )
        .unwrap();
        assert_eq!(combined.image.pixels, patch.image.pixels);
        for y in 0..combined.image.size[1] {
            for x in 0..combined.image.size[0] {
                assert_eq!(combined.owner_at(x, y), patch.owner_at(x, y));
            }
        }
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
        let all_entries: Vec<_> = updated.map.entries().iter().copied().collect();
        let actual = render::raster_patch(
            &all_entries,
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
        for tile in &tiles {
            let x0 = (tile.uv.min.x * atlas.size[0] as f32).round() as usize;
            let x1 = (tile.uv.max.x * atlas.size[0] as f32).round() as usize;
            let y0 = (tile.uv.min.y * atlas.size[1] as f32).round() as usize;
            let y1 = (tile.uv.max.y * atlas.size[1] as f32).round() as usize;
            for y in y0..y1 {
                for x in x0..x1 {
                    let p = atlas.pixels[y * atlas.size[0] + x];
                    if p != eframe::egui::Color32::TRANSPARENT {
                        if tile.id == 0 {
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
    fn large_motion_uses_bounded_sprites_and_fades_remaining_changes() {
        use crate::treemap::Rect;
        use eframe::egui::Color32;
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1200.0,
            h: 2.0,
        };
        let entries: Vec<_> = (0..600)
            .map(|id| RenderEntry {
                id,
                rect: Rect {
                    x: (id * 2) as f32,
                    y: 0.0,
                    w: 2.0,
                    h: 2.0,
                },
                color: Color32::RED,
            })
            .collect();
        let map =
            RenderMap::build_with_size(entries.clone(), bounds, [1200, 2], &token, 1).unwrap();
        let moves: Vec<_> = entries.iter().map(|e| (e.id, e.rect, e.rect)).collect();
        let target =
            render::raster_patch(&[], &[], bounds, [1200, 2], [0, 0, 1200, 2], &token, 1).unwrap();
        let transition = build_transition(
            &map,
            &target,
            &[],
            &[] as &[RenderEntry],
            &[],
            &moves,
            [0, 0, 1200, 2],
            1200,
            &token,
            1,
        )
        .unwrap();
        assert_eq!(transition.tiles.len(), 512);
        let atlas = transition.atlas.as_ref().unwrap();
        assert!(atlas.size[0] <= 2048 && atlas.size[1] <= 2048);
        let selected: std::collections::HashSet<_> =
            transition.tiles.iter().map(|t| t.id).collect();
        let fading = (0..600).find(|id| !selected.contains(id)).unwrap();
        let pixel = fading * 2;
        assert_ne!(
            transition.previous.pixels[pixel],
            transition.background.as_ref().unwrap().pixels[pixel]
        );
        // The old non-sprite remains visible in the fading layer rather than
        // making the entire 600-item transition silently disappear.
        assert_eq!(transition.previous.size, [1200, 2]);
        assert_eq!(transition.background.as_ref().unwrap().size, [1200, 2]);
    }

    #[test]
    fn accumulated_motion_bounds_atlas_and_fades_excess_objects() {
        use eframe::egui::Color32;
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 60.0,
            h: 20.0,
        };
        let old: Vec<_> = (0..600)
            .map(|id| RenderEntry {
                id,
                rect: Rect {
                    x: (id % 60) as f32,
                    y: (id / 60) as f32,
                    w: 1.0,
                    h: 1.0,
                },
                color: Color32::RED,
            })
            .collect();
        let new: Vec<_> = old
            .iter()
            .map(|entry| RenderEntry {
                rect: Rect {
                    y: entry.rect.y + 10.0,
                    ..entry.rect
                },
                ..*entry
            })
            .collect();
        let original = RenderMap::build_with_size(old, bounds, [60, 20], &token, 1).unwrap();
        let changes: Vec<_> = new.iter().map(|entry| (entry.id, Some(*entry))).collect();
        let target = original.stage_geometry(&changes, None).unwrap();
        let patch =
            render::raster_patch(&new, &[], bounds, [60, 20], [0, 0, 60, 20], &token, 1).unwrap();
        let mut accumulator = PatchAccumulator::new(&original, &[], &token, 1).unwrap();
        accumulator.push(&[patch]).unwrap();
        let (_, transitions) = accumulator.finish(&target).unwrap();
        let sprites: Vec<_> = transitions
            .iter()
            .flat_map(|transition| &transition.tiles)
            .collect();
        assert_eq!(sprites.len(), 512);
        let atlas = transitions
            .iter()
            .find_map(|transition| transition.atlas.as_ref())
            .unwrap();
        assert!(atlas.size[0] <= 2048 && atlas.size[1] <= 2048);
        assert!(atlas.size[0] * atlas.size[1] <= 2_097_152);
        let selected: std::collections::HashSet<_> = sprites.iter().map(|tile| tile.id).collect();
        let fading = (0..600).find(|id| !selected.contains(id)).unwrap();
        let (gx, gy) = (fading % 60, fading / 60);
        let transition = transitions
            .iter()
            .find(|transition| {
                let [x, y] = transition.origin;
                let [w, h] = transition.previous.size;
                gx >= x && gx < x + w && gy >= y && gy < y + h
            })
            .unwrap();
        let [x, y] = transition.origin;
        let width = transition.previous.size[0];
        assert_ne!(
            transition.previous.pixels[(gy - y) * width + gx - x],
            crate::BG
        );
    }

    #[test]
    fn accumulated_motion_uses_one_sprite_and_excludes_owner_from_all_cells() {
        use eframe::egui::Color32;
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 64.0,
            h: 16.0,
        };
        let from = Rect {
            x: 1.0,
            y: 2.0,
            w: 7.0,
            h: 8.0,
        };
        let to = Rect {
            x: 49.0,
            y: 3.0,
            w: 8.0,
            h: 7.0,
        };
        let old_entry = RenderEntry {
            id: 0,
            rect: from,
            color: Color32::RED,
        };
        let new_entry = RenderEntry {
            id: 0,
            rect: to,
            color: Color32::RED,
        };
        let original =
            RenderMap::build_with_size(vec![old_entry], bounds, [64, 16], &token, 1).unwrap();
        let target = original
            .stage_geometry(&[(0, Some(new_entry))], None)
            .unwrap();
        let patch = render::raster_patch(
            &[new_entry],
            &[],
            bounds,
            [64, 16],
            [0, 0, 64, 16],
            &token,
            1,
        )
        .unwrap();
        let mut accumulator = PatchAccumulator::new(&original, &[], &token, 1).unwrap();
        accumulator.push(&[patch]).unwrap();
        let (patches, transitions) = accumulator.finish(&target).unwrap();
        assert_eq!(patches.len(), 64);
        let sprites: Vec<_> = transitions
            .iter()
            .flat_map(|transition| &transition.tiles)
            .collect();
        assert_eq!(sprites.len(), 1);
        assert_eq!(sprites[0].id, 0);
        assert_eq!(sprites[0].from, from);
        assert_eq!(sprites[0].to, to);
        let source = transitions
            .iter()
            .find(|transition| transition.origin == [0, 2])
            .unwrap();
        assert_eq!(source.previous.pixels[2], crate::BG);
        let destination = transitions
            .iter()
            .find(|transition| transition.origin == [48, 2])
            .unwrap();
        assert_eq!(
            destination.background.as_ref().unwrap().pixels[1 + 8],
            crate::BG
        );
        let atlas = transitions
            .iter()
            .find_map(|transition| transition.atlas.as_ref())
            .unwrap();
        assert!(atlas.pixels.iter().any(|pixel| pixel.r() > pixel.g()));
    }

    #[test]
    fn root_subtree_removal_preserves_siblings_and_reuses_both_vacancies() {
        let token = AtomicU64::new(1);
        let file = |path: &str, parent: &str, n| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 30,
            identity: scan::FileIdentity {
                device: n,
                ..Default::default()
            },
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: true,
            can_trash: true,
        };
        // A direct tile and a nested partition under the same root child model
        // the two allocations used by the folder projection.
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a", "/disk", 1),
                file("/disk/a/x/f", "/disk/a/x", 2),
                file("/disk/b/g", "/disk/b", 3),
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
            w: 600.0,
            h: 300.0,
        };
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk"), vec![(0, 30)]),
                (Path::new("/disk/a/x"), vec![(1, 30)]),
                (Path::new("/disk/b"), vec![(2, 30)]),
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
            [600, 300],
            &token,
            1,
        )
        .unwrap();
        let sibling = map.rect(2).unwrap();
        let old_direct = map.rect(0).unwrap();
        let old_nested = map.rect(1).unwrap();
        let removed = [scene.tile(slots[0]).unwrap(), scene.tile(slots[1]).unwrap()];
        let deltas: Vec<_> = removed
            .iter()
            .cloned()
            .map(|tile| TileDelta {
                before: Some(tile),
                after: None,
            })
            .collect();
        let mut next = projection.clone();
        next.replace(&slots[..2], Vec::new()).unwrap();
        assert!(
            fast_root_removal(
                &geometry,
                &map,
                &projection,
                &next,
                &[],
                &deltas[..1],
                4096,
                &token,
                1,
                true
            )
            .unwrap()
            .is_none()
        );
        let updated = fast_root_removal(
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
        assert_eq!(updated.map.rect(0), None);
        assert_eq!(updated.map.rect(1), None);
        assert_eq!(updated.map.rect(2), Some(sibling));
        for rect in [old_direct, old_nested] {
            assert_eq!(
                updated
                    .map
                    .hit_test(rect.x + rect.w / 2.0, rect.y + rect.h / 2.0),
                None
            );
        }
        assert_eq!(
            updated
                .map
                .hit_test(sibling.x + sibling.w / 2.0, sibling.y + sibling.h / 2.0),
            Some(2)
        );
        let entries: Vec<_> = updated.map.entries().iter().copied().collect();
        for patch in &updated.patches {
            let [x, y] = patch.origin;
            let [w, h] = patch.image.size;
            let reference = render::raster_patch(
                &entries,
                updated.map.platforms(),
                bounds,
                [600, 300],
                [x, y, x + w, y + h],
                &token,
                1,
            )
            .unwrap();
            assert_eq!(patch.image.pixels, reference.image.pixels);
            for py in 0..h {
                for px in 0..w {
                    assert_eq!(patch.owner_at(px, py), reference.owner_at(px, py));
                }
            }
        }
        let root = Path::new("/disk");
        let a = Path::new("/disk/new1");
        let b = Path::new("/disk/new2");
        let (geometry, _, donor) = updated
            .geometry
            .allocate_root_child(root, a, Some(slots[0]), 30)
            .unwrap();
        assert!(donor.is_none());
        let (_, _, donor) = geometry
            .allocate_root_child(root, b, Some(slots[1]), 30)
            .unwrap();
        assert!(donor.is_none());
    }

    #[test]
    fn fading_previous_frame_keeps_existing_holes_and_never_sprites_them() {
        use crate::treemap::Rect;
        use eframe::egui::Color32;
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 10.0,
            y: 20.0,
            w: 4.0,
            h: 2.0,
        };
        let entry = RenderEntry {
            id: 0,
            rect: bounds,
            color: Color32::RED,
        };
        let map = RenderMap::build_with_size(vec![entry], bounds, [8, 4], &token, 1).unwrap();
        let target =
            render::raster_patch(&[], &[], bounds, [8, 4], [0, 0, 8, 4], &token, 1).unwrap();
        let hole = Rect {
            x: 10.0,
            y: 20.0,
            w: 2.0,
            h: 2.0,
        };
        let transition = build_transition(
            &map,
            &target,
            &[hole],
            &[] as &[RenderEntry],
            &[],
            &[],
            [0, 0, 8, 4],
            16,
            &token,
            1,
        )
        .unwrap();
        assert_eq!(transition.previous.pixels[0], crate::BG);
        assert_ne!(transition.previous.pixels[7], crate::BG);

        let mut source =
            render::raster_patch(&[entry], &[], bounds, [8, 4], [0, 0, 8, 4], &token, 1).unwrap();
        mask_old_holes(&mut source, &[hole], bounds, [8, 4]);
        assert_eq!(source.owner_at(0, 0), None);
        assert_eq!(source.owner_at(7, 0), Some(0));
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
                excluded: false,
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
        assert!(!out.patches.is_empty());
        let transition = out.transitions.first().expect("survivor transitions");
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
        assert!(next.patches.is_empty());
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
                excluded: false,
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

    #[test]
    fn structural_update_can_empty_and_repopulate_the_scanned_root() {
        use crate::scene_model::EntryId;

        let token = AtomicU64::new(1);
        let file = scan::FileEntry {
            path: "/disk/old".into(),
            parent: Arc::from(Path::new("/disk")),
            size: 8,
            identity: scan::FileIdentity::default(),
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file],
            directories: Vec::new().into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let slots = projection_slots(&scene, &source, Projection::Files).unwrap();
        let projection = SparseProjection::new(source, slots.clone()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 40.0,
            h: 20.0,
        };
        let size = [40, 20];
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk"), vec![(0, 8)])],
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
        let old = scene.tile(slots[0]).unwrap();
        let cleared = prepare_structural(
            &geometry,
            map,
            &projection,
            &[],
            &[TileDelta {
                before: Some(old.clone()),
                after: None,
            }],
            Path::new("/disk"),
            1.0,
            0.0,
            100,
            &token,
            1,
        )
        .expect("removing the last item should update the existing scene");
        assert!(cleared.projection.is_empty());
        assert_eq!(cleared.map.hit_test(20.0, 10.0), None);

        let mut added = old;
        added.id = TileId::File(EntryId {
            slot: 0,
            generation: 1,
        });
        added.parent = "/disk/new".into();
        added.path = "/disk/new/item".into();
        let filled = prepare_structural(
            &cleared.geometry,
            cleared.map,
            &cleared.projection,
            &cleared.holes,
            &[TileDelta {
                before: None,
                after: Some(added),
            }],
            Path::new("/disk/new"),
            1.0,
            0.0,
            100,
            &token,
            1,
        )
        .expect("adding below an empty root should update the existing scene");
        assert!(!filled.projection.is_empty());
        assert!(filled.map.hit_test(20.0, 10.0).is_some());
    }

    #[test]
    fn structural_scope_can_include_the_scanned_root_parent() {
        let token = AtomicU64::new(1);
        let file = |path: &str, parent: &str, size| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size,
            identity: scan::FileIdentity::default(),
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk", "/", 10), file("/disk/a/f", "/disk/a", 10)],
            directories: Vec::new().into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source.clone());
        let slots = projection_slots(&scene, &source, Projection::Files).unwrap();
        let projection = SparseProjection::new(source, slots.clone()).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 60.0,
            h: 40.0,
        };
        let size = [60, 40];
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/"), vec![(0, 10)]),
                (Path::new("/disk/a"), vec![(1, 10)]),
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
        let before = scene.tile(slots[0]).unwrap();
        let mut after = before.clone();
        after.size = 20;
        let updated = prepare_structural(
            &geometry,
            map,
            &projection,
            &[],
            &[TileDelta {
                before: Some(before),
                after: Some(after),
            }],
            Path::new("/disk"),
            1.0,
            0.0,
            100,
            &token,
            1,
        )
        .expect("root-parent projection should remain in the existing scene");
        assert_eq!(updated.projection.get(0).unwrap().size, 20);
        assert!(updated.map.rect(0).is_some());
        assert!(updated.map.rect(1).is_some());
    }

    #[test]
    fn root_child_fold_and_unfold_preserve_sibling_allocation() {
        use crate::scene_model::View;
        let token = AtomicU64::new(1);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let fact = |path: &str, n, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        let file = |path: &str, parent: &str, n, parent_n| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: identity(n),
            parent_identity: identity(parent_n),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/f", "/disk/a", 3, 2),
                file("/disk/a/g", "/disk/a", 4, 2),
                file("/disk/b/h", "/disk/b", 6, 5),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, false),
                fact("/disk/b", 5, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source);
        let a = scene
            .project_directory(View::Hybrid, Path::new("/disk/a"))
            .pop()
            .unwrap();
        let b = scene
            .project_directory(View::Hybrid, Path::new("/disk/b"))
            .pop()
            .unwrap();
        let data = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![a.to_entry(), b.to_entry()],
            directories: Vec::new().into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let projection = SparseProjection::new(data, vec![a.id, b.id]).unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 200.0,
        };
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk"), vec![(0, a.size), (1, b.size)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(&layout.partitions, &[a.id, b.id]).unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [400, 200],
            &token,
            1,
        )
        .unwrap();
        let original_b = map.rect(1).unwrap();
        let files: Vec<_> = ["/disk/a/f", "/disk/a/g"]
            .into_iter()
            .map(|path| {
                scene
                    .tile(crate::scene_model::TileId::File(
                        scene.file_id(Path::new(path)).unwrap(),
                    ))
                    .unwrap()
            })
            .collect();
        let mut expand = vec![TileDelta {
            before: Some(a.clone()),
            after: None,
        }];
        expand.extend(files.iter().cloned().map(|file| TileDelta {
            before: None,
            after: Some(file),
        }));
        let unfolded = prepare_structural_proven(
            &geometry,
            map,
            &projection,
            &[],
            &expand,
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[],
            false,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(unfolded.map.rect(1), Some(original_b));
        assert!(
            unfolded
                .patches
                .iter()
                .map(|p| p.image.size[0] * p.image.size[1])
                .sum::<usize>()
                < 400 * 200
        );
        let mut collapse: Vec<_> = files
            .into_iter()
            .map(|file| TileDelta {
                before: Some(file),
                after: None,
            })
            .collect();
        collapse.push(TileDelta {
            before: None,
            after: Some(a),
        });
        let folded = prepare_structural_proven(
            &unfolded.geometry,
            unfolded.map,
            &unfolded.projection,
            &unfolded.holes,
            &collapse,
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[],
            false,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(folded.map.rect(1), Some(original_b));
        assert!(
            folded
                .patches
                .iter()
                .map(|p| p.image.size[0] * p.image.size[1])
                .sum::<usize>()
                < 400 * 200
        );
    }

    #[test]
    fn independent_root_folds_publish_once_without_moving_other_children() {
        use crate::scene_model::{TileId, View};
        let token = AtomicU64::new(1);
        let identity = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let fact = |path: &str, n, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        let file = |path: &str, parent: &str, n, parent_n| scan::FileEntry {
            path: path.into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: identity(n),
            parent_identity: identity(parent_n),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/f", "/disk/a", 3, 2),
                file("/disk/b/f", "/disk/b", 5, 4),
                file("/disk/c/f", "/disk/c", 7, 6),
                file("/disk/d/f", "/disk/d", 9, 8),
                file("/disk/e/f", "/disk/e", 11, 10),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, false),
                fact("/disk/b", 4, false),
                fact("/disk/c", 6, false),
                fact("/disk/d", 8, false),
                fact("/disk/e", 10, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(source);
        let folders: Vec<_> = ["/disk/a", "/disk/b", "/disk/c"]
            .into_iter()
            .map(|path| {
                scene
                    .project_directory(View::Hybrid, Path::new(path))
                    .pop()
                    .unwrap()
            })
            .collect();
        let projection = SparseProjection::new(
            Arc::new(scan::ScanResult {
                root: "/disk".into(),
                files: folders.iter().map(|tile| tile.to_entry()).collect(),
                directories: Vec::new().into(),
                skipped: 0,
                errors: vec![],
                elapsed: Duration::ZERO,
            }),
            folders.iter().map(|tile| tile.id).collect(),
        )
        .unwrap();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 300.0,
        };
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk"), vec![(0, 50), (1, 50), (2, 50)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(
            &layout.partitions,
            &folders.iter().map(|tile| tile.id).collect::<Vec<_>>(),
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
        let original_map = map.snapshot_for_update();
        let unrelated_fact_map = map.snapshot_for_update();
        let replaced_root_map = map.snapshot_for_update();
        let multiple_map = map.snapshot_for_update();
        let vacancy_map = map.snapshot_for_update();
        let replacement_map = map.snapshot_for_update();
        let proven_map = map.snapshot_for_update();
        let batched_map = map.snapshot_for_update();
        let old_rects: Vec<_> = (0..3).map(|id| map.rect(id)).collect();
        let stable = map.rect(2);
        let mut deltas = Vec::new();
        for (i, path) in ["/disk/a/f", "/disk/b/f"].into_iter().enumerate() {
            deltas.push(TileDelta {
                before: Some(folders[i].clone()),
                after: None,
            });
            deltas.push(TileDelta {
                before: None,
                after: Some(
                    scene
                        .tile(TileId::File(scene.file_id(Path::new(path)).unwrap()))
                        .unwrap(),
                ),
            });
        }
        let updated = prepare_structural_proven(
            &geometry,
            map,
            &projection,
            &[],
            &deltas,
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[],
            false,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(updated.map.rect(2), stable);
        assert!(
            updated
                .patches
                .iter()
                .map(|p| p.image.size[0] * p.image.size[1])
                .sum::<usize>()
                < 600 * 300
        );
        assert!(!updated.transitions.is_empty());
        let proven = prepare_structural_proven(
            &geometry,
            proven_map,
            &projection,
            &[],
            &deltas,
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[
                Path::new("/disk/a/empty").into(),
                Path::new("/disk/b/empty").into(),
            ],
            false,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(proven.map.rect(2), stable);
        assert!(
            proven
                .patches
                .iter()
                .map(|p| p.image.size[0] * p.image.size[1])
                .sum::<usize>()
                < 600 * 300
        );
        let batched = prepare_structural_proven_batched(
            &geometry,
            batched_map,
            &projection,
            &[],
            &deltas,
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[
                Path::new("/disk/a/empty").into(),
                Path::new("/disk/b/empty").into(),
            ],
            false,
            true,
            &[],
        )
        .unwrap();
        assert!(batched.transitions.is_empty());
        for slot in 0..batched.projection.len() {
            assert_eq!(batched.map.rect(slot), proven.map.rect(slot));
        }
        for y in 0..300 {
            for x in 0..600 {
                assert_eq!(
                    batched.map.hit_test(x as f32 + 0.5, y as f32 + 0.5),
                    proven.map.hit_test(x as f32 + 0.5, y as f32 + 0.5)
                );
            }
        }

        let added = scene
            .project_directory(View::Hybrid, Path::new("/disk/d"))
            .pop()
            .unwrap();
        // An unrelated directory fact with no mode delta must not move its
        // branch. A replaced scan-root identity cannot reuse the old scene.
        for (snapshot, facts, root_stable) in [
            (
                unrelated_fact_map,
                vec![Path::new("/disk/other").into()],
                true,
            ),
            (replaced_root_map, vec![Path::new("/disk/d").into()], false),
        ] {
            let result = prepare_structural_proven(
                &geometry,
                snapshot,
                &projection,
                &[],
                &[TileDelta {
                    before: None,
                    after: Some(added.clone()),
                }],
                Path::new("/disk"),
                1.0,
                0.0,
                4096,
                &token,
                1,
                &facts,
                false,
                root_stable,
                &[],
            )
            .unwrap();
            if root_stable {
                assert_eq!(
                    (0..3)
                        .filter(|&id| result.map.rect(id) == old_rects[id])
                        .count(),
                    2
                );
            } else {
                assert!((0..3).any(|id| result.map.rect(id) != old_rects[id]));
            }
            assert!(
                result
                    .map
                    .rect(result.projection.slot(added.id).unwrap())
                    .is_some()
            );
            let entries: Vec<_> = result
                .map
                .entries()
                .iter()
                .copied()
                .filter(|entry| result.map.rect(entry.id).is_some())
                .collect();
            let expected = render::raster_patch(
                &entries,
                result.map.platforms(),
                bounds,
                [600, 300],
                [0, 0, 600, 300],
                &token,
                1,
            )
            .unwrap();
            for patch in &result.patches {
                for y in 0..patch.image.size[1] {
                    for x in 0..patch.image.size[0] {
                        let gx = patch.origin[0] + x;
                        let gy = patch.origin[1] + y;
                        assert_eq!(
                            patch.image.pixels[y * patch.image.size[0] + x],
                            expected.image.pixels[gy * 600 + gx]
                        );
                        assert_eq!(patch.owner_at(x, y), expected.owner_at(gx, gy));
                    }
                }
            }
        }
        let inserted = prepare_structural_proven(
            &geometry,
            original_map,
            &projection,
            &[],
            &[TileDelta {
                before: None,
                after: Some(added.clone()),
            }],
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[],
            false,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(
            (0..3)
                .filter(|&id| inserted.map.rect(id) == old_rects[id])
                .count(),
            2
        );
        assert!(
            inserted
                .map
                .rect(inserted.projection.slot(added.id).unwrap())
                .is_some()
        );
        assert!(
            inserted
                .patches
                .iter()
                .map(|p| p.image.size[0] * p.image.size[1])
                .sum::<usize>()
                < 600 * 300
        );
        let added_e = scene
            .project_directory(View::Hybrid, Path::new("/disk/e"))
            .pop()
            .unwrap();
        let inserted_multiple = prepare_structural_proven(
            &geometry,
            multiple_map,
            &projection,
            &[],
            &[
                TileDelta {
                    before: None,
                    after: Some(added),
                },
                TileDelta {
                    before: None,
                    after: Some(added_e.clone()),
                },
            ],
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[Path::new("/disk/d").into(), Path::new("/disk/e").into()],
            false,
            true,
            &[],
        )
        .unwrap();
        assert!((0..3).any(|id| inserted_multiple.map.rect(id) == old_rects[id]));
        assert!(
            inserted_multiple
                .map
                .rect(inserted_multiple.projection.slot(added_e.id).unwrap())
                .is_some()
        );
        assert!(
            inserted_multiple
                .patches
                .iter()
                .map(|p| p.image.size[0] * p.image.size[1])
                .sum::<usize>()
                < 600 * 300
        );

        // A file and a directory may replace one another at the same root
        // path. Keep the old allocation and never reuse its published slot.
        let mut replacement_file = scene
            .tile(TileId::File(scene.file_id(Path::new("/disk/a/f")).unwrap()))
            .unwrap();
        replacement_file.path = "/disk/a".into();
        replacement_file.parent = "/disk".into();
        let replaced = prepare_structural_proven(
            &geometry,
            replacement_map,
            &projection,
            &[],
            &[
                TileDelta {
                    before: Some(folders[0].clone()),
                    after: None,
                },
                TileDelta {
                    before: None,
                    after: Some(replacement_file.clone()),
                },
            ],
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[],
            false,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(replaced.map.rect(1), old_rects[1]);
        let replacement_slot = replaced.projection.slot(replacement_file.id).unwrap();
        assert_ne!(replacement_slot, 0);
        assert_eq!(replaced.map.rect(replacement_slot), old_rects[0]);
        let restored = prepare_structural_proven(
            &replaced.geometry,
            replaced.map,
            &replaced.projection,
            &replaced.holes,
            &[
                TileDelta {
                    before: Some(replacement_file),
                    after: None,
                },
                TileDelta {
                    before: None,
                    after: Some(folders[0].clone()),
                },
            ],
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[],
            false,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(restored.map.rect(1), old_rects[1]);
        assert_eq!(
            restored
                .map
                .rect(restored.projection.slot(folders[0].id).unwrap()),
            old_rects[0]
        );

        // A deletion-created hole survives an unrelated structural fold and
        // can then be reused without moving an unrelated sibling.
        let mut vacancy_geometry = geometry.clone();
        vacancy_geometry
            .apply(
                &[incremental_geometry::TileDelta {
                    before: Some(TileState {
                        id: folders[2].id,
                        parent: "/disk".into(),
                        weight: folders[2].size,
                    }),
                    after: None,
                }],
                1.0,
                1_000_000.0,
                &token,
                1,
            )
            .unwrap();
        let mut vacancy_projection = projection.clone();
        vacancy_projection
            .replace(&[folders[2].id], vec![])
            .unwrap();
        let mut vacancy_map = vacancy_map;
        let old_c = vacancy_map.rect(2).unwrap();
        let holes = vacancy_map.erase(2);
        let survivor = vacancy_map.rect(1);
        let mut fold_a = vec![TileDelta {
            before: Some(folders[0].clone()),
            after: None,
        }];
        fold_a.push(TileDelta {
            before: None,
            after: Some(
                scene
                    .tile(TileId::File(scene.file_id(Path::new("/disk/a/f")).unwrap()))
                    .unwrap(),
            ),
        });
        let folded = prepare_structural_proven(
            &vacancy_geometry,
            vacancy_map,
            &vacancy_projection,
            &holes,
            &fold_a,
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[],
            false,
            true,
            &[],
        )
        .unwrap();
        let added = scene
            .project_directory(View::Hybrid, Path::new("/disk/d"))
            .pop()
            .unwrap();
        let reused = prepare_structural_proven(
            &folded.geometry,
            folded.map,
            &folded.projection,
            &folded.holes,
            &[TileDelta {
                before: None,
                after: Some(added.clone()),
            }],
            Path::new("/disk"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[],
            false,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(reused.map.rect(1), survivor);
        assert_eq!(
            reused.map.rect(reused.projection.slot(added.id).unwrap()),
            Some(old_c)
        );

        // A new root child whose contents are individual file tiles similarly
        // splits only one existing directory allocation, not the entire root.
        let file_tiles: Vec<_> = ["/disk/a/f", "/disk/b/f", "/disk/c/f"]
            .into_iter()
            .map(|path| {
                scene
                    .tile(TileId::File(scene.file_id(Path::new(path)).unwrap()))
                    .unwrap()
            })
            .collect();
        let file_projection = SparseProjection::new(
            Arc::new(scan::ScanResult {
                root: "/disk".into(),
                files: file_tiles.iter().map(|tile| tile.to_entry()).collect(),
                directories: Vec::new().into(),
                skipped: 0,
                errors: vec![],
                elapsed: Duration::ZERO,
            }),
            file_tiles.iter().map(|tile| tile.id).collect(),
        )
        .unwrap();
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
        let file_geometry = IncrementalGeometry::from_layout(
            &layout.partitions,
            &file_tiles.iter().map(|tile| tile.id).collect::<Vec<_>>(),
        )
        .unwrap();
        let file_map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [600, 300],
            &token,
            1,
        )
        .unwrap();
        let old_rects: Vec<_> = (0..3).map(|id| file_map.rect(id)).collect();
        let file_d = scene
            .tile(TileId::File(scene.file_id(Path::new("/disk/d/f")).unwrap()))
            .unwrap();
        let inserted = prepare_structural_proven(
            &file_geometry,
            file_map,
            &file_projection,
            &[],
            &[TileDelta {
                before: None,
                after: Some(file_d.clone()),
            }],
            Path::new("/disk/d"),
            1.0,
            0.0,
            4096,
            &token,
            1,
            &[],
            false,
            true,
            &[],
        )
        .unwrap();
        assert_eq!(
            (0..3)
                .filter(|&id| inserted.map.rect(id) == old_rects[id])
                .count(),
            2
        );
        assert!(
            inserted
                .map
                .rect(inserted.projection.slot(file_d.id).unwrap())
                .is_some()
        );
        assert!(
            inserted
                .patches
                .iter()
                .map(|p| p.image.size[0] * p.image.size[1])
                .sum::<usize>()
                < 600 * 300
        );
        let live: Vec<_> = inserted
            .map
            .entries()
            .iter()
            .copied()
            .filter(|entry| inserted.map.rect(entry.id).is_some())
            .collect();
        let full = render::raster_patch(
            &live,
            inserted.map.platforms(),
            bounds,
            [600, 300],
            [0, 0, 600, 300],
            &token,
            1,
        )
        .unwrap();
        for patch in &inserted.patches {
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
    fn multi_batch_accumulator_keeps_original_fade_and_latest_pixels() {
        use eframe::egui::Color32;
        let token = AtomicU64::new(7);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 64.0,
            h: 64.0,
        };
        let entry = |color| RenderEntry {
            id: 0,
            rect: bounds,
            color,
        };
        let map =
            RenderMap::build_with_size(vec![entry(Color32::RED)], bounds, [64, 64], &token, 7)
                .unwrap();
        let first = render::raster_patch(
            &[entry(Color32::GREEN)],
            &[],
            bounds,
            [64, 64],
            [4, 4, 12, 12],
            &token,
            7,
        )
        .unwrap();
        let second = render::raster_patch(
            &[entry(Color32::BLUE)],
            &[],
            bounds,
            [64, 64],
            [8, 8, 16, 16],
            &token,
            7,
        )
        .unwrap();
        let hole = Rect {
            x: 4.0,
            y: 4.0,
            w: 1.0,
            h: 1.0,
        };
        let holes = [hole];
        let mut accumulator = PatchAccumulator::new(&map, &holes, &token, 7).unwrap();
        accumulator.push(&[first]).unwrap();
        accumulator.push(&[second]).unwrap();
        let (patches, transitions) = accumulator.finish(&map).unwrap();
        assert_eq!(patches.len(), transitions.len());
        assert!(patches.len() <= 64);
        let at = |x, y| {
            patches
                .iter()
                .find_map(|p| {
                    let [px, py] = p.origin;
                    let [w, h] = p.image.size;
                    (x >= px && y >= py && x < px + w && y < py + h).then(|| {
                        (
                            p.image.pixels[(y - py) * w + x - px],
                            p.owner_at(x - px, y - py),
                        )
                    })
                })
                .unwrap()
        };
        let expected_first = render::raster_patch(
            &[entry(Color32::GREEN)],
            &[],
            bounds,
            [64, 64],
            [4, 4, 12, 12],
            &token,
            7,
        )
        .unwrap();
        let expected_second = render::raster_patch(
            &[entry(Color32::BLUE)],
            &[],
            bounds,
            [64, 64],
            [8, 8, 16, 16],
            &token,
            7,
        )
        .unwrap();
        assert_eq!(at(5, 5), (expected_first.image.pixels[9], Some(0)));
        assert_eq!(at(8, 8), (expected_second.image.pixels[0], Some(0)));
        let original_tile = transitions.iter().find(|t| t.origin == [0, 0]).unwrap();
        assert_eq!(original_tile.previous.pixels[4 * 8 + 4], crate::BG);
        assert!(original_tile.background.is_none());
        assert!(original_tile.atlas.is_none());
    }
}
