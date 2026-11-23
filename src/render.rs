//! Cached file raster and pixel ownership. Work in this module runs off the UI
//! thread; a frame draws one texture and queries one ownership pixel.
use crate::treemap::Rect;
use eframe::egui::{Color32, ColorImage};
use rayon::prelude::*;
use std::ops::{Index, IndexMut};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const MAX_SIDE: usize = 2048;
const BAND_ROWS: usize = 128;
const NONE: u32 = u32::MAX;
const PAGE_LEN: usize = 1024;

/// A snapshot-friendly sequence. A write copies only its page, never all the
/// geometry or slot lookup entries held by an older displayed scene.
#[derive(Clone)]
pub struct PagedVec<T: Clone> {
    pages: Arc<Vec<Arc<Vec<T>>>>,
    len: usize,
}

impl<T: Clone> PagedVec<T> {
    fn from_vec(items: Vec<T>) -> Self {
        let len = items.len();
        let mut iter = items.into_iter();
        let mut pages = Vec::with_capacity(len.div_ceil(PAGE_LEN));
        loop {
            let page: Vec<_> = iter.by_ref().take(PAGE_LEN).collect();
            if page.is_empty() {
                break;
            }
            pages.push(Arc::new(page));
        }
        Self {
            pages: Arc::new(pages),
            len,
        }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn get(&self, i: usize) -> Option<&T> {
        if i >= self.len {
            return None;
        }
        self.pages.get(i / PAGE_LEN)?.get(i % PAGE_LEN)
    }
    fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        if i >= self.len {
            return None;
        }
        Arc::make_mut(Arc::make_mut(&mut self.pages).get_mut(i / PAGE_LEN)?).get_mut(i % PAGE_LEN)
    }
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.pages.iter().flat_map(|page| page.iter())
    }
    /// Contiguous storage pages, in the original painting order.
    pub fn pages(&self) -> impl Iterator<Item = &[T]> {
        self.pages.iter().map(|page| page.as_slice())
    }
    fn push(&mut self, item: T) {
        let pages = Arc::make_mut(&mut self.pages);
        if self.len % PAGE_LEN == 0 {
            pages.push(Arc::new(Vec::with_capacity(PAGE_LEN)));
        }
        Arc::make_mut(pages.last_mut().unwrap()).push(item);
        self.len += 1;
    }
    fn resize(&mut self, len: usize, value: T) {
        assert!(len >= self.len);
        while self.len < len {
            self.push(value.clone());
        }
    }
}
impl<T: Clone> Index<usize> for PagedVec<T> {
    type Output = T;
    fn index(&self, i: usize) -> &T {
        self.get(i).expect("page index out of bounds")
    }
}
impl<T: Clone> IndexMut<usize> for PagedVec<T> {
    fn index_mut(&mut self, i: usize) -> &mut T {
        self.get_mut(i).expect("page index out of bounds")
    }
}
impl<'a, T: Clone> IntoIterator for &'a PagedVec<T> {
    type Item = &'a T;
    type IntoIter = std::iter::FlatMap<
        std::slice::Iter<'a, Arc<Vec<T>>>,
        std::slice::Iter<'a, T>,
        fn(&'a Arc<Vec<T>>) -> std::slice::Iter<'a, T>,
    >;
    fn into_iter(self) -> Self::IntoIter {
        fn page_iter<T>(page: &Arc<Vec<T>>) -> std::slice::Iter<'_, T> {
            page.iter()
        }
        self.pages.iter().flat_map(page_iter::<T>)
    }
}
const BACKGROUND: Color32 = Color32::from_rgb(19, 23, 29);
// Logical-point design tokens for the shared platform material.
const MIN_PLATFORM_SIDE: f32 = 28.0;
const MIN_PLATFORM_AREA: f32 = 1800.0;
// Per-edge decoration is a convergent series along every directory path.
// The largest (bottom) inset is (gap + shoulder + rise)/(1-decay) = 11.5pt.
const PLATFORM_GAP: f32 = 0.1;
const PLATFORM_SHOULDER: f32 = 0.35;
const PLATFORM_RISE: f32 = 0.7;
// Material bands preserve readable light/shadow without reserving empty area.
const BEVEL_WIDTH: f32 = 3.0;
const LAYER_HEIGHT: f32 = 4.0;
const LIGHT_REACH: f32 = 6.0;
const DECORATION_DECAY: f32 = 0.9;
const FILE_SHADE_LIGHT: f64 = 1.20;
const FILE_SHADE_X: f64 = 0.18;
const FILE_SHADE_Y: f64 = 0.12;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Platform {
    pub bounds: Rect,
    pub top: Rect,
    pub color: Color32,
    rise: f32,
}

impl Platform {
    pub fn new(bounds: Rect, color: Color32, depth: u8, pixel_size: f32) -> Option<Self> {
        let side = bounds.w.min(bounds.h);
        if !valid(bounds) || side < MIN_PLATFORM_SIDE || bounds.w * bounds.h < MIN_PLATFORM_AREA {
            return None;
        }
        let scale = DECORATION_DECAY.powi(depth as i32);
        let rise = PLATFORM_RISE * scale;
        // Subpixel layers merge visually; geometry and original file IDs still
        // descend through the full tree without consuming more decoration.
        if (PLATFORM_GAP + PLATFORM_SHOULDER + PLATFORM_RISE) * scale < pixel_size * 0.5 {
            return None;
        }
        let gap = (PLATFORM_GAP + PLATFORM_SHOULDER) * scale;
        let top = Rect {
            x: bounds.x + gap,
            y: bounds.y + gap,
            w: bounds.w - gap * 2.0 - rise * 0.25,
            h: bounds.h - gap * 2.0 - rise,
        };
        Some(Self {
            bounds,
            top,
            color,
            rise,
        })
    }

    // Paint only this allocation's material. Shared lighting follows file
    // painting; every exposed shoulder/side keeps a NONE owner.
    fn sample(self, x: f32, y: f32) -> Option<Color32> {
        let r = self.top;
        if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
            let left = x - r.x;
            let upper = y - r.y;
            let material = 1.055 - 0.07 * upper / r.h - 0.025 * left / r.w;
            return Some(shaded(self.color, material));
        }
        let dx = self.rise * 0.25;
        if y >= r.y + r.h && y < r.y + r.h + self.rise {
            let f = (y - r.y - r.h) / self.rise;
            if x >= r.x + dx * f && x < r.x + r.w + dx * f {
                return Some(self.color);
            }
        }
        if x >= r.x + r.w && x < r.x + r.w + dx {
            let f = (x - r.x - r.w) / dx;
            if y >= r.y + self.rise * f && y < r.y + r.h + self.rise * f {
                return Some(self.color);
            }
        }
        None
    }
}

// One temporary shared elevation field: every actual terrace contributes its
// own narrow bevel, including beneath descendant materials. The world-space
// halo keeps directional light continuous across crops and worker bands.
struct HeightField {
    values: Vec<f32>,
    size: [usize; 2],
    origin: [f32; 2],
    step: [f32; 2],
    offset: [usize; 2],
    full_size: [usize; 2],
}

impl HeightField {
    fn build(
        platforms: &[Platform],
        view: Rect,
        size: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        Self::build_region(
            platforms,
            view,
            size,
            [0, 0, size[0], size[1]],
            token,
            generation,
        )
    }

    // Build the same world-space height samples as a full frame, retaining only
    // samples needed by the patch and its lighting/interpolation halo.
    fn build_region(
        platforms: &[Platform],
        view: Rect,
        size: [usize; 2],
        region: [usize; 4],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        // A quarter-point floor resolves the narrowest bevel while bounding
        // the halo even when the viewport is zoomed to a fraction of a point.
        let step = [view.w / size[0] as f32, view.h / size[1] as f32].map(|step| step.max(0.25));
        let low = [view.x, view.y];
        let high = [view.x + view.w, view.y + view.h];
        let origin = std::array::from_fn::<_, 2, _>(|axis| {
            ((low[axis] - LIGHT_REACH) / step[axis]).floor() * step[axis] - step[axis]
        });
        let full_size = std::array::from_fn::<_, 2, _>(|axis| {
            ((high[axis] + LIGHT_REACH - origin[axis]) / step[axis]).ceil() as usize + 1
        });
        let patch_low = [region[0], region[1]];
        let patch_high = [region[2], region[3]];
        let offset = std::array::from_fn::<_, 2, _>(|axis| {
            let world = low[axis]
                + (patch_low[axis] as f32 + 0.5) * (high[axis] - low[axis]) / size[axis] as f32;
            (((world - LIGHT_REACH) - origin[axis]) / step[axis])
                .floor()
                .max(0.0) as usize
        });
        let end = std::array::from_fn::<_, 2, _>(|axis| {
            let world = low[axis]
                + (patch_high[axis].saturating_sub(1) as f32 + 0.5) * (high[axis] - low[axis])
                    / size[axis] as f32;
            ((((world + LIGHT_REACH) - origin[axis]) / step[axis]).ceil() as usize + 2)
                .min(full_size[axis])
        });
        // Preserve the original full-frame field, including boundary samples.
        let (offset, end) = if region == [0, 0, size[0], size[1]] {
            ([0, 0], full_size)
        } else {
            (offset, end)
        };
        let field_size = [end[0] - offset[0], end[1] - offset[1]];
        let mut values = vec![0.0; field_size[0] * field_size[1]];
        let mut bands: Vec<Vec<(Platform, [usize; 4])>> = (0..field_size[1].div_ceil(BAND_ROWS))
            .map(|_| Vec::new())
            .collect();
        for (index, &platform) in platforms.iter().enumerate() {
            if index % 1024 == 0 && cancelled(token, generation) {
                return None;
            }
            let r = platform.bounds;
            let low = [r.x, r.y];
            let high = [r.x + r.w, r.y + r.h];
            let start = std::array::from_fn::<_, 2, _>(|axis| {
                (((low[axis] - origin[axis]) / step[axis] - 0.5)
                    .ceil()
                    .max(0.0) as usize)
                    .saturating_sub(offset[axis])
                    .min(field_size[axis])
            });
            let end = std::array::from_fn::<_, 2, _>(|axis| {
                (((high[axis] - origin[axis]) / step[axis] - 0.5)
                    .ceil()
                    .max(0.0) as usize)
                    .saturating_sub(offset[axis])
                    .min(field_size[axis])
            });
            if start[0] >= end[0] || start[1] >= end[1] {
                continue;
            }
            for band in start[1] / BAND_ROWS..=(end[1] - 1) / BAND_ROWS {
                bands[band].push((
                    platform,
                    [
                        start[0],
                        end[0],
                        start[1].max(band * BAND_ROWS),
                        end[1].min((band + 1) * BAND_ROWS),
                    ],
                ));
            }
        }
        let width = field_size[0];
        values
            .par_chunks_mut(width * BAND_ROWS)
            .zip(bands.into_par_iter())
            .enumerate()
            .for_each(|(band, (values, platforms))| {
                for (platform, [x0, x1, y0, y1]) in platforms {
                    if cancelled(token, generation) {
                        return;
                    }
                    let r = platform.bounds;
                    for y in y0..y1 {
                        let wy = origin[1] + ((y + offset[1]) as f32 + 0.5) * step[1];
                        let dy = (wy - r.y).min(r.y + r.h - wy);
                        for x in x0..x1 {
                            let wx = origin[0] + ((x + offset[0]) as f32 + 0.5) * step[0];
                            let distance = dy.min(wx - r.x).min(r.x + r.w - wx);
                            let t = (distance / BEVEL_WIDTH).clamp(0.0, 1.0);
                            values[(y - band * BAND_ROWS) * width + x] +=
                                LAYER_HEIGHT * t * t * (3.0 - 2.0 * t);
                        }
                    }
                }
            });
        (!cancelled(token, generation)).then_some(Self {
            values,
            size: field_size,
            origin,
            step,
            offset,
            full_size,
        })
    }

    fn sample(&self, x: f32, y: f32) -> f32 {
        let p = [x, y];
        let uv = std::array::from_fn::<_, 2, _>(|axis| {
            (((p[axis] - self.origin[axis]) / self.step[axis] - 0.5)
                .clamp(0.0, (self.full_size[axis] - 1) as f32)
                - self.offset[axis] as f32)
                .clamp(0.0, (self.size[axis] - 1) as f32)
        });
        let [x0, y0] = uv.map(|v| v.floor() as usize);
        let x1 = (x0 + 1).min(self.size[0] - 1);
        let y1 = (y0 + 1).min(self.size[1] - 1);
        let [u, v] = [uv[0] - x0 as f32, uv[1] - y0 as f32];
        let at = |x, y| self.values[y * self.size[0] + x];
        let a = at(x0, y0) * (1.0 - u) + at(x1, y0) * u;
        let b = at(x0, y1) * (1.0 - u) + at(x1, y1) * u;
        a * (1.0 - v) + b * v
    }
}

fn illuminate(color: Color32, field: &HeightField, x: f32, y: f32) -> Color32 {
    const NORMAL_STEP: f32 = 0.6;
    let gx =
        (field.sample(x + NORMAL_STEP, y) - field.sample(x - NORMAL_STEP, y)) / (2.0 * NORMAL_STEP);
    let gy =
        (field.sample(x, y + NORMAL_STEP) - field.sample(x, y - NORMAL_STEP)) / (2.0 * NORMAL_STEP);
    let normal = (0.38 * (gx + gy) / (1.0 + (gx * gx + gy * gy).sqrt())).clamp(-0.32, 0.30);
    let occlusion = contact_occlusion(field, x, y);
    let light = normal - 0.38 * occlusion;
    let alpha = color.a();
    let channel = |value: u8| {
        let value = value as f32;
        if light >= 0.0 {
            (value + (alpha as f32 - value) * light).round() as u8
        } else {
            (value * (1.0 + light)).round() as u8
        }
    };
    Color32::from_rgba_premultiplied(
        channel(color.r()),
        channel(color.g()),
        channel(color.b()),
        alpha,
    )
}

fn contact_occlusion(field: &HeightField, x: f32, y: f32) -> f32 {
    let height = field.sample(x, y);
    let mut occlusion = 0.0f32;
    for distance in [1.5, 3.0, 5.0] {
        let blocker = field.sample(x - distance * 0.70710677, y - distance * 0.70710677);
        let obstruction = ((blocker - height - distance * 0.45) / LAYER_HEIGHT).clamp(0.0, 1.0);
        occlusion = occlusion.max(obstruction * (1.0 - distance / 8.0));
    }
    occlusion
}

fn shaded(color: Color32, shade: f32) -> Color32 {
    let alpha = color.a();
    let channel = |v: u8| (v as f32 * shade).round().min(alpha as f32) as u8;
    Color32::from_rgba_premultiplied(
        channel(color.r()),
        channel(color.g()),
        channel(color.b()),
        alpha,
    )
}

#[derive(Clone, Copy)]
pub struct RenderEntry {
    pub id: usize,
    pub rect: Rect,
    pub color: Color32,
}

pub struct RenderRaster {
    pub image: Option<ColorImage>,
    pub bounds: Rect,
    size: [usize; 2],
    owners: Arc<Vec<u32>>,
}

impl RenderRaster {
    /// Hit the file actually painted at the pointer's displayed pixel. This is
    /// constant-time even when millions of subpixel files share the map.
    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        if !x.is_finite()
            || !y.is_finite()
            || x < self.bounds.x
            || y < self.bounds.y
            || x >= self.bounds.x + self.bounds.w
            || y >= self.bounds.y + self.bounds.h
        {
            return None;
        }
        let px = (((x - self.bounds.x) / self.bounds.w) * self.size[0] as f32) as usize;
        let py = (((y - self.bounds.y) / self.bounds.h) * self.size[1] as f32) as usize;
        let id = *self.owners.get(py * self.size[0] + px)?;
        (id != NONE).then_some(id as usize)
    }
}

pub struct RenderMap {
    pub image: Option<ColorImage>,
    pub bounds: Rect,
    entries: PagedVec<RenderEntry>,
    platforms: Arc<Vec<Platform>>,
    by_id: PagedVec<u32>,
    spatial: RasterSpatial,
    base: RenderRaster,
}

impl RenderMap {
    /// Re-sample the existing world-space scene at a new physical resolution.
    /// Layout, stable slots and platform geometry are shared; only pixels and
    /// their ownership map are generated in bounded tiles off the UI thread.
    /// The caller must publish both the returned image and map together after
    /// checking that the source frame is still current.
    pub fn resample_tiled(
        &self,
        requested: [usize; 2],
        holes: &[Rect],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        let size = dimensions(requested);
        let len = size[0].checked_mul(size[1])?;
        let mut pixels = vec![BACKGROUND; len];
        let mut owners = vec![NONE; len];
        let index = RasterIndex::new(
            &self.entries,
            &self.platforms,
            self.bounds,
            size,
            token,
            generation,
        )?;
        index.for_each_patch_filtered(
            [0, 0, size[0], size[1]],
            262_144,
            token,
            generation,
            |entry| self.by_id.get(entry.id).is_some_and(|index| *index != NONE),
            |patch| {
                let [w, h] = patch.image.size;
                for y in 0..h {
                    let dst = (patch.origin[1] + y) * size[0] + patch.origin[0];
                    let src = y * w;
                    pixels[dst..dst + w].copy_from_slice(&patch.image.pixels[src..src + w]);
                    owners[dst..dst + w].copy_from_slice(&patch.owners[src..src + w]);
                }
                true
            },
        )?;
        // Holes are in the original world grid; preserve their visible mask in
        // the new ownership grid as well as in the resized texture.
        let sx = size[0] as f32 / self.bounds.w;
        let sy = size[1] as f32 / self.bounds.h;
        for (i, rect) in holes.iter().enumerate() {
            if i % 1024 == 0 && cancelled(token, generation) {
                return None;
            }
            let edge = |value: f32, origin: f32, scale: f32, max: usize| {
                (((value - origin) * scale - 0.5).ceil().max(0.0) as usize).min(max)
            };
            let x0 = edge(rect.x, self.bounds.x, sx, size[0]);
            let x1 = edge(rect.x + rect.w, self.bounds.x, sx, size[0]);
            let y0 = edge(rect.y, self.bounds.y, sy, size[1]);
            let y1 = edge(rect.y + rect.h, self.bounds.y, sy, size[1]);
            if x0 >= x1 || y0 >= y1 {
                continue;
            }
            for y in y0..y1 {
                let row = y * size[0];
                pixels[row + x0..row + x1].fill(BACKGROUND);
                owners[row + x0..row + x1].fill(NONE);
            }
        }
        if cancelled(token, generation) {
            return None;
        }
        // Reuse the exact-grid index just built for resampling. Rebuilding it
        // here would scan the entire scene twice on every DPI change.
        let spatial = RasterSpatial {
            size: index.size,
            domain: index.domain,
            halo: index.halo,
            entry_buckets: index.entry_buckets.clone(),
            platform_buckets: index.platform_buckets.clone(),
        };
        Some(Self {
            image: Some(ColorImage::new(size, pixels)),
            bounds: self.bounds,
            entries: self.entries.clone(),
            platforms: Arc::clone(&self.platforms),
            by_id: self.by_id.clone(),
            spatial,
            base: RenderRaster {
                image: None,
                bounds: self.bounds,
                size,
                owners: Arc::new(owners),
            },
        })
    }

    /// File ids are dataset indices; bounding the lookup table by entry count
    /// prevents arbitrary sparse ids from creating unbounded allocations.
    #[cfg(test)]
    pub fn build(
        entries: Vec<RenderEntry>,
        bounds: Rect,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        Self::build_with_size(
            entries,
            bounds,
            [bounds.w.ceil() as usize, bounds.h.ceil() as usize],
            token,
            generation,
        )
    }

    /// Keep logical geometry and physical pixel dimensions separate for HiDPI
    /// windows. The same raster owns both the displayed pixels and hit map.
    #[cfg(test)]
    pub fn build_with_size(
        entries: Vec<RenderEntry>,
        bounds: Rect,
        size: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        Self::build_with_platforms(entries, Vec::new(), bounds, size, token, generation)
    }

    #[cfg(test)]
    pub fn build_with_platforms(
        entries: Vec<RenderEntry>,
        platforms: Vec<Platform>,
        bounds: Rect,
        size: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        let slot_count = entries.len();
        Self::build_with_slots(
            entries, platforms, bounds, size, slot_count, token, generation,
        )
    }

    /// Build a map with a bounded, caller-owned stable slot space. `slot_count`
    /// comes from the source snapshot or generational arena, not an arbitrary ID.
    /// This preserves sparse IDs after tombstones without indexing a new Vec by
    /// the number of live entries.
    pub fn build_with_slots(
        entries: Vec<RenderEntry>,
        platforms: Vec<Platform>,
        bounds: Rect,
        size: [usize; 2],
        slot_count: usize,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        if cancelled(token, generation)
            || !valid(bounds)
            || slot_count < entries.len()
            || slot_count >= NONE as usize
        {
            return None;
        }
        let mut by_id = vec![NONE; slot_count];
        for (index, entry) in entries.iter().enumerate() {
            if index % 4096 == 0 && cancelled(token, generation) {
                return None;
            }
            if entry.id >= by_id.len() || index >= NONE as usize || by_id[entry.id] != NONE {
                return None;
            }
            by_id[entry.id] = index as u32;
        }
        let mut base = rasterize(&entries, &platforms, bounds, size, token, generation)?;
        let image = base.image.take();
        let spatial = RasterSpatial::build(
            (&entries).into(),
            &platforms,
            bounds,
            size,
            token,
            generation,
        )?;
        Some(Self {
            image,
            bounds,
            entries: PagedVec::from_vec(entries),
            platforms: Arc::new(platforms),
            by_id: PagedVec::from_vec(by_id),
            spatial,
            base,
        })
    }

    /// O(1) snapshot for off-thread updates. Raster pixels already uploaded to
    /// the texture are intentionally not copied. Changes use copy-on-write and
    /// the caller must reject results whose scene generation is no longer live.
    pub fn snapshot_for_update(&self) -> Self {
        Self {
            image: None,
            bounds: self.bounds,
            entries: self.entries.clone(),
            platforms: Arc::clone(&self.platforms),
            by_id: self.by_id.clone(),
            spatial: self.spatial.clone(),
            base: RenderRaster {
                image: None,
                bounds: self.base.bounds,
                size: self.base.size,
                owners: Arc::clone(&self.base.owners),
            },
        }
    }

    /// Live geometry in painting order; tombstones retain invalid rectangles.
    pub fn entries(&self) -> &PagedVec<RenderEntry> {
        &self.entries
    }

    /// Borrow the index paired with this immutable scene and pixel grid.
    /// Any snapshot geometry mutation maintains its own copy-on-write buckets.
    pub fn raster_index(&self) -> RasterIndex<'_> {
        RasterIndex {
            entries: (&self.entries).into(),
            platforms: &self.platforms,
            live: Some(&self.by_id),
            view: self.bounds,
            requested: self.base.size,
            size: self.spatial.size,
            domain: self.spatial.domain,
            halo: self.spatial.halo,
            entry_buckets: self.spatial.entry_buckets.clone(),
            platform_buckets: self.spatial.platform_buckets.clone(),
        }
    }

    /// Platform painting order is shared with the full rasterizer.
    pub fn platforms(&self) -> &[Platform] {
        &self.platforms
    }

    /// Verify in one cancellable pass that no displayed pixel refers to a
    /// tombstoned slot. Run on an immutable snapshot off the UI thread.
    pub fn owners_exclude(
        &self,
        excluded: &std::collections::HashSet<usize>,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<bool> {
        if cancelled(token, generation) {
            return None;
        }
        if excluded.is_empty() {
            return Some(true);
        }
        for chunk in self.base.owners.chunks(4096) {
            if cancelled(token, generation) {
                return None;
            }
            if chunk
                .iter()
                .any(|&owner| owner != NONE && excluded.contains(&(owner as usize)))
            {
                return Some(false);
            }
        }
        (!cancelled(token, generation)).then_some(true)
    }

    /// Validate every displayed owner against an immutable projection without
    /// copying the owner raster. Call off-thread; `None` means the scene was
    /// cancelled and no validation result may be published.
    pub fn owners_satisfy(
        &self,
        mut accepts: impl FnMut(usize) -> bool,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<bool> {
        for chunk in self.base.owners.chunks(4096) {
            if cancelled(token, generation) {
                return None;
            }
            if chunk
                .iter()
                .any(|&owner| owner != NONE && !accepts(owner as usize))
            {
                return (!cancelled(token, generation)).then_some(false);
            }
        }
        (!cancelled(token, generation)).then_some(true)
    }

    /// Actual texture dimensions after the renderer's size cap.
    pub fn physical_size(&self) -> [usize; 2] {
        self.base.size
    }

    pub fn rect(&self, id: usize) -> Option<Rect> {
        let index = *self.by_id.get(id)?;
        (index != NONE).then(|| self.entries[index as usize].rect)
    }

    #[cfg(test)]
    pub fn platform_count(&self) -> usize {
        self.platforms.len()
    }

    #[cfg(test)]
    pub fn color(&self, id: usize) -> Option<Color32> {
        let index = *self.by_id.get(id)?;
        (index != NONE).then(|| self.entries[index as usize].color)
    }

    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        self.base.hit_test(x, y)
    }

    /// Remove just this file's sampled pixels. Geometry and the original GPU
    /// texture stay valid; returned rectangles cover precisely the cleared
    /// owners and can be painted as background over that texture.
    #[cfg(test)]
    pub fn erase(&mut self, id: usize) -> Vec<Rect> {
        let Some(rect) = self.rect(id) else {
            return Vec::new();
        };
        let index = self.by_id[id];
        if index != NONE {
            let old = self.entries[index as usize];
            if entry_has_sample(&old, self.bounds, self.spatial.size) {
                self.spatial
                    .entry_buckets
                    .remove(index, old.rect, self.spatial.domain);
            }
            self.entries[index as usize].rect.w = 0.0;
        }
        self.by_id[id] = NONE;
        let [width, height] = self.base.size;
        let sx = width as f32 / self.bounds.w;
        let sy = height as f32 / self.bounds.h;
        let x0 = (((rect.x - self.bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(width);
        let x1 = ((((rect.x + rect.w) - self.bounds.x) * sx - 0.5)
            .ceil()
            .max(0.0) as usize)
            .min(width);
        let y0 = (((rect.y - self.bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(height);
        let y1 = ((((rect.y + rect.h) - self.bounds.y) * sy - 0.5)
            .ceil()
            .max(0.0) as usize)
            .min(height);
        let mut runs: Vec<[usize; 4]> = Vec::new();
        let mut previous: std::collections::BTreeMap<(usize, usize), usize> =
            std::collections::BTreeMap::new();
        for y in y0..y1 {
            let mut current = std::collections::BTreeMap::new();
            let row = &mut Arc::make_mut(&mut self.base.owners)[y * width..(y + 1) * width];
            let mut x = x0;
            while x < x1 {
                if row[x] != id as u32 {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < x1 && row[x] == id as u32 {
                    row[x] = NONE;
                    x += 1;
                }
                let key = (start, x);
                let index = if let Some(&index) = previous.get(&key) {
                    runs[index][3] += 1;
                    index
                } else {
                    runs.push([start, y, x - start, 1]);
                    runs.len() - 1
                };
                current.insert(key, index);
            }
            previous = current;
        }
        runs.into_iter()
            .map(|[x, y, w, h]| Rect {
                x: self.bounds.x + x as f32 / sx,
                y: self.bounds.y + y as f32 / sy,
                w: w as f32 / sx,
                h: h as f32 / sy,
            })
            .collect()
    }

    /// Hide many IDs in one owner pass. Run on `snapshot_for_update()` off
    /// the UI thread; discard the snapshot when cancelled. Returned rectangles
    /// cover exactly the erased pixels, ready to be published with the map.
    pub fn erase_many(
        &mut self,
        ids: &[usize],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Vec<Rect>> {
        if cancelled(token, generation) {
            return None;
        }
        let mut erased = std::collections::HashSet::with_capacity(ids.len().min(self.by_id.len()));
        for (i, &id) in ids.iter().enumerate() {
            if i % 4096 == 0 && cancelled(token, generation) {
                return None;
            }
            if self.by_id.get(id).is_some_and(|&slot| slot != NONE) {
                erased.insert(id as u32);
            }
        }
        if erased.is_empty() {
            return Some(Vec::new());
        }
        for &id in &erased {
            let index = self.by_id[id as usize];
            if index != NONE {
                let old = self.entries[index as usize];
                if entry_has_sample(&old, self.bounds, self.spatial.size) {
                    self.spatial
                        .entry_buckets
                        .remove(index, old.rect, self.spatial.domain);
                }
                self.entries[index as usize].rect.w = 0.0;
                self.by_id[id as usize] = NONE;
            }
        }
        let [width, height] = self.base.size;
        let owners = Arc::make_mut(&mut self.base.owners);
        let mut runs: Vec<[usize; 4]> = Vec::new();
        let mut previous: std::collections::BTreeMap<(usize, usize), usize> =
            std::collections::BTreeMap::new();
        for y in 0..height {
            if cancelled(token, generation) {
                return None;
            }
            let row = &mut owners[y * width..(y + 1) * width];
            let mut current = std::collections::BTreeMap::new();
            let mut x = 0;
            while x < width {
                if !erased.contains(&row[x]) {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < width && erased.contains(&row[x]) {
                    row[x] = NONE;
                    x += 1;
                }
                let key = (start, x);
                let index = if let Some(&i) = previous.get(&key) {
                    runs[i][3] += 1;
                    i
                } else {
                    runs.push([start, y, x - start, 1]);
                    runs.len() - 1
                };
                current.insert(key, index);
            }
            previous = current;
        }
        let sx = width as f32 / self.bounds.w;
        let sy = height as f32 / self.bounds.h;
        Some(
            runs.into_iter()
                .map(|[x, y, w, h]| Rect {
                    x: self.bounds.x + x as f32 / sx,
                    y: self.bounds.y + y as f32 / sy,
                    w: w as f32 / sx,
                    h: h as f32 / sy,
                })
                .collect(),
        )
    }

    /// Render just the currently visible world rectangle. The overview remains
    /// available while this cancellable detail image is produced. Never create
    /// a bitmap for an entire zoomed canvas.
    #[cfg(test)]
    pub fn rasterize(
        &self,
        view: Rect,
        size: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<RenderRaster> {
        let entries: Vec<_> = self.entries.iter().copied().collect();
        rasterize(&entries, &self.platforms, view, size, token, generation)
    }
}

#[derive(Clone, Copy)]
struct Draw {
    id: u32,
    x0: u16,
    x1: u16,
    y0: u16,
    y1: u16,
    color: Color32,
    shade: f32,
    shade_step: [f32; 2],
}

fn valid(rect: Rect) -> bool {
    rect.x.is_finite()
        && rect.y.is_finite()
        && rect.w.is_finite()
        && rect.h.is_finite()
        && rect.w > 0.0
        && rect.h > 0.0
}

fn cancelled(token: &AtomicU64, generation: u64) -> bool {
    token.load(Ordering::Relaxed) != generation
}

fn dimensions(requested: [usize; 2]) -> [usize; 2] {
    let longest = requested[0].max(requested[1]).max(1);
    let scale = (MAX_SIDE as f64 / longest as f64).min(1.0);
    requested.map(|side| ((side as f64 * scale).round() as usize).clamp(1, MAX_SIDE))
}

/// Actual raster dimensions for a requested physical viewport.
pub fn capped_size(requested: [usize; 2]) -> [usize; 2] {
    dimensions(requested)
}

pub fn logical_pixel_size(bounds: Rect, requested: [usize; 2]) -> f32 {
    let [width, height] = dimensions(requested);
    (bounds.w / width as f32).max(bounds.h / height as f32)
}

/// A rectangular update in the physical pixel grid of an existing map.
/// Colors and owners are computed together and must be published together.
pub struct RasterPatch {
    pub origin: [usize; 2],
    pub image: ColorImage,
    owners: Vec<u32>,
    full_size: [usize; 2],
}

impl RasterPatch {
    /// Allocate one region of the output grid. Callers staging a transaction
    /// are responsible for tracking its coverage before publishing it.
    pub fn blank(origin: [usize; 2], size: [usize; 2], full_size: [usize; 2]) -> Option<Self> {
        let [width, height] = size;
        let area = width.checked_mul(height)?;
        if width == 0
            || height == 0
            || origin[0].checked_add(width)? > full_size[0]
            || origin[1].checked_add(height)? > full_size[1]
        {
            return None;
        }
        Some(Self {
            origin,
            image: ColorImage::new(size, vec![BACKGROUND; area]),
            owners: vec![NONE; area],
            full_size,
        })
    }

    /// Copy the intersecting pixels and owners from another patch on the same
    /// immutable grid. A non-intersection is a successful no-op.
    pub fn blit_from(&mut self, other: &RasterPatch) -> bool {
        if self.full_size != other.full_size {
            return false;
        }
        let [w, h] = self.image.size;
        let [ow, oh] = other.image.size;
        let Some(area) = w.checked_mul(h) else {
            return false;
        };
        let Some(other_area) = ow.checked_mul(oh) else {
            return false;
        };
        if self.owners.len() != area
            || self.image.pixels.len() != area
            || other.owners.len() != other_area
            || other.image.pixels.len() != other_area
        {
            return false;
        }
        let [x, y] = self.origin;
        let [ox, oy] = other.origin;
        let (Some(x_end), Some(y_end), Some(ox_end), Some(oy_end)) = (
            x.checked_add(w),
            y.checked_add(h),
            ox.checked_add(ow),
            oy.checked_add(oh),
        ) else {
            return false;
        };
        if x_end > self.full_size[0]
            || y_end > self.full_size[1]
            || ox_end > self.full_size[0]
            || oy_end > self.full_size[1]
        {
            return false;
        }
        let x0 = x.max(ox);
        let y0 = y.max(oy);
        let x1 = x_end.min(ox_end);
        let y1 = y_end.min(oy_end);
        if x0 >= x1 || y0 >= y1 {
            return true;
        }
        let count = x1 - x0;
        for row in y0..y1 {
            let dst = (row - y) * w + x0 - x;
            let src = (row - oy) * ow + x0 - ox;
            self.image.pixels[dst..dst + count]
                .copy_from_slice(&other.image.pixels[src..src + count]);
            self.owners[dst..dst + count].copy_from_slice(&other.owners[src..src + count]);
        }
        true
    }

    /// Assemble disjoint patches into one exact half-open region. Reject gaps,
    /// overlaps, mixed grids and invalid bounds rather than publishing a partial
    /// visual or hit map.
    pub fn assemble(region: [usize; 4], patches: &[RasterPatch]) -> Option<RasterPatch> {
        let [x0, y0, x1, y1] = region;
        let first = patches.first()?;
        if x0 >= x1 || y0 >= y1 || x1 > first.full_size[0] || y1 > first.full_size[1] {
            return None;
        }
        let width = x1 - x0;
        let height = y1 - y0;
        let area = width.checked_mul(height)?;
        let mut pixels = vec![BACKGROUND; area];
        let mut owners = vec![NONE; area];
        let mut covered = vec![false; area];
        for patch in patches {
            let [pw, ph] = patch.image.size;
            let [px, py] = patch.origin;
            let end_x = px.checked_add(pw)?;
            let end_y = py.checked_add(ph)?;
            if patch.full_size != first.full_size
                || pw == 0
                || ph == 0
                || px < x0
                || py < y0
                || end_x > x1
                || end_y > y1
                || patch.owners.len() != pw.checked_mul(ph)?
                || patch.image.pixels.len() != pw.checked_mul(ph)?
            {
                return None;
            }
            for row in 0..ph {
                let dst = (py - y0 + row) * width + px - x0;
                if covered[dst..dst + pw].iter().any(|value| *value) {
                    return None;
                }
                covered[dst..dst + pw].fill(true);
                pixels[dst..dst + pw]
                    .copy_from_slice(&patch.image.pixels[row * pw..(row + 1) * pw]);
                owners[dst..dst + pw].copy_from_slice(&patch.owners[row * pw..(row + 1) * pw]);
            }
        }
        if covered.iter().any(|value| !value) {
            return None;
        }
        Some(RasterPatch {
            origin: [x0, y0],
            image: ColorImage::new([width, height], pixels),
            owners,
            full_size: first.full_size,
        })
    }

    /// Reproduce an already displayed deletion mask in an old-frame patch.
    /// This must clear ownership as well as color before extracting sprites.
    pub fn mask_background(&mut self, rect: [usize; 4], color: Color32) {
        let [width, height] = self.image.size;
        let [x0, y0, x1, y1] = rect;
        let (x0, x1) = (x0.min(width), x1.min(width));
        let (y0, y1) = (y0.min(height), y1.min(height));
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        for y in y0..y1 {
            self.image.pixels[y * width + x0..y * width + x1].fill(color);
            self.owners[y * width + x0..y * width + x1].fill(NONE);
        }
    }

    /// Make a texture atlas containing only pixels owned by the requested
    /// tiles. This prevents cropped tile quads from copying adjacent files or
    /// the directory platform during a transition.
    pub fn owner_at(&self, x: usize, y: usize) -> Option<usize> {
        let width = self.image.size[0];
        let owner = *self.owners.get(y.checked_mul(width)?.checked_add(x)?)?;
        (owner != NONE).then_some(owner as usize)
    }
}

impl RenderMap {
    /// Validate and apply all patches and geometry as a single map transaction.
    /// The caller must atomically upload the matching texture patches.
    pub fn apply_scene_patches(
        &mut self,
        patches: &[RasterPatch],
        changes: &[(usize, Option<RenderEntry>)],
        platforms: Option<Vec<Platform>>,
    ) -> bool {
        if patches.is_empty() {
            return false;
        }
        // Overlap is ambiguous. A bitmap bounds validation by the output
        // pixel count instead of the square of the number of patches.
        let [full_width, full_height] = self.base.size;
        let Some(area) = full_width.checked_mul(full_height) else {
            return false;
        };
        let mut covered = vec![0u64; area.div_ceil(64)];
        for patch in patches {
            let [width, height] = patch.image.size;
            let [x0, y0] = patch.origin;
            let Some(x1) = x0.checked_add(width) else {
                return false;
            };
            let Some(y1) = y0.checked_add(height) else {
                return false;
            };
            let Some(patch_area) = width.checked_mul(height) else {
                return false;
            };
            if patch.full_size != self.base.size
                || width == 0
                || height == 0
                || x1 > full_width
                || y1 > full_height
                || patch.owners.len() != patch_area
                || patch.image.pixels.len() != patch_area
            {
                return false;
            }
            for y in y0..y1 {
                let mut start = y * full_width + x0;
                let end = y * full_width + x1;
                while start < end {
                    let offset = start % 64;
                    let count = (64 - offset).min(end - start);
                    let mask = if count == 64 {
                        u64::MAX
                    } else {
                        ((1u64 << count) - 1) << offset
                    };
                    let word = &mut covered[start / 64];
                    if *word & mask != 0 {
                        return false;
                    }
                    *word |= mask;
                    start += count;
                }
            }
        }
        let mut next = self.snapshot_for_update();
        if !next.apply_scene_patch(&patches[0], changes, platforms) {
            return false;
        }
        for patch in &patches[1..] {
            if !next.apply_scene_patch(patch, &[], None) {
                return false;
            }
        }
        *self = next;
        true
    }

    /// Commit a patch and its changed geometry as one validated transaction.
    /// IDs outside the changed region must retain their identity and geometry.
    /// The caller must publish the matching texture in the same UI frame.
    pub fn apply_scene_patch(
        &mut self,
        patch: &RasterPatch,
        changes: &[(usize, Option<RenderEntry>)],
        platforms: Option<Vec<Platform>>,
    ) -> bool {
        let [width, height] = patch.image.size;
        let [x, y] = patch.origin;
        if self.base.size != patch.full_size
            || width == 0
            || height == 0
            || x.checked_add(width).is_none_or(|v| v > self.base.size[0])
            || y.checked_add(height).is_none_or(|v| v > self.base.size[1])
            || patch.owners.len() != width * height
        {
            return false;
        }
        let Some(changed) = self.validate_geometry(changes) else {
            return false;
        };
        // An owner in the new patch must resolve to an existing or added entry.
        for &owner in &patch.owners {
            if owner == NONE {
                continue;
            }
            let id = owner as usize;
            let alive = changed
                .get(&id)
                .copied()
                .unwrap_or_else(|| self.by_id.get(id).is_some_and(|index| *index != NONE));
            if !alive {
                return false;
            }
        }
        self.update_geometry(changes, platforms);
        self.apply_patch(patch)
    }

    /// Prepare the target geometry and its COW spatial index without changing
    /// displayed pixels. Call on a background snapshot and publish only with
    /// matching patches and the caller's scene-version check.
    pub fn stage_geometry(
        &self,
        changes: &[(usize, Option<RenderEntry>)],
        platforms: Option<Vec<Platform>>,
    ) -> Option<Self> {
        self.validate_geometry(changes)?;
        let mut staged = self.snapshot_for_update();
        staged.update_geometry(changes, platforms);
        Some(staged)
    }

    /// Like `stage_geometry`, but accepts the size of the caller-owned stable
    /// projection. Deleted projection slots remain tombstones, so the next
    /// visible ID can legitimately be larger than the number of changes. The
    /// slot count must come from the validated projection, not an arbitrary ID.
    pub fn stage_geometry_with_slots(
        &self,
        changes: &[(usize, Option<RenderEntry>)],
        platforms: Option<Vec<Platform>>,
        slot_count: usize,
    ) -> Option<Self> {
        self.validate_geometry_with_slots(changes, slot_count)?;
        let mut staged = self.snapshot_for_update();
        if staged.by_id.len() < slot_count {
            staged.by_id.resize(slot_count, NONE);
        }
        staged.update_geometry(changes, platforms);
        Some(staged)
    }

    fn validate_geometry(
        &self,
        changes: &[(usize, Option<RenderEntry>)],
    ) -> Option<std::collections::HashMap<usize, bool>> {
        self.validate_geometry_with_slots(changes, self.by_id.len().checked_add(changes.len())?)
    }

    fn validate_geometry_with_slots(
        &self,
        changes: &[(usize, Option<RenderEntry>)],
        slot_count: usize,
    ) -> Option<std::collections::HashMap<usize, bool>> {
        if slot_count < self.by_id.len() || slot_count >= NONE as usize {
            return None;
        }
        let mut changed = std::collections::HashMap::with_capacity(changes.len());
        let mut added = 0usize;
        for (id, entry) in changes {
            if *id >= NONE as usize
                || *id >= slot_count
                || changed.insert(*id, entry.is_some()).is_some()
                || entry
                    .as_ref()
                    .is_some_and(|entry| entry.id != *id || !valid(entry.rect))
            {
                return None;
            }
            if entry.is_some() && self.by_id.get(*id).is_none_or(|slot| *slot == NONE) {
                added += 1;
            }
        }
        if self
            .entries
            .len()
            .checked_add(added)
            .is_none_or(|len| len >= NONE as usize)
        {
            return None;
        }
        Some(changed)
    }

    fn update_geometry(
        &mut self,
        changes: &[(usize, Option<RenderEntry>)],
        platforms: Option<Vec<Platform>>,
    ) {
        for (id, entry) in changes {
            if *id >= self.by_id.len() {
                self.by_id.resize(*id + 1, NONE);
            }
            let index = self.by_id[*id];
            if index != NONE {
                let old = self.entries[index as usize];
                if entry_has_sample(&old, self.bounds, self.spatial.size) {
                    self.spatial
                        .entry_buckets
                        .remove(index, old.rect, self.spatial.domain);
                }
            }
            match entry {
                Some(entry) if index == NONE => {
                    let new_index = self.entries.len() as u32;
                    self.by_id[*id] = new_index;
                    self.entries.push(*entry);
                    if entry_has_sample(entry, self.bounds, self.spatial.size) {
                        self.spatial.entry_buckets.insert(
                            new_index,
                            entry.rect,
                            self.spatial.domain,
                        );
                    }
                }
                Some(entry) => {
                    self.entries[index as usize] = *entry;
                    if entry_has_sample(entry, self.bounds, self.spatial.size) {
                        self.spatial
                            .entry_buckets
                            .insert(index, entry.rect, self.spatial.domain);
                    }
                }
                None if index != NONE => {
                    self.by_id[*id] = NONE;
                    self.entries[index as usize].rect.w = 0.0;
                }
                None => {}
            }
        }
        if let Some(platforms) = platforms {
            let mut buckets = RasterBuckets::new();
            for (i, platform) in platforms.iter().enumerate() {
                buckets.insert(i as u32, platform.bounds, self.spatial.domain);
            }
            self.spatial.platform_buckets = buckets;
            self.platforms = Arc::new(platforms);
        }
    }

    /// Update hit-test ownership after the matching texture patch is queued.
    /// The caller must also atomically publish matching geometry and slot IDs.
    pub fn apply_patch(&mut self, patch: &RasterPatch) -> bool {
        let [width, height] = patch.image.size;
        let [x0, y0] = patch.origin;
        if self.base.size != patch.full_size
            || width == 0
            || height == 0
            || x0.checked_add(width).is_none_or(|x| x > self.base.size[0])
            || y0.checked_add(height).is_none_or(|y| y > self.base.size[1])
            || patch.owners.len() != width * height
        {
            return false;
        }
        for y in 0..height {
            let dst = (y0 + y) * self.base.size[0] + x0;
            Arc::make_mut(&mut self.base.owners)[dst..dst + width]
                .copy_from_slice(&patch.owners[y * width..(y + 1) * width]);
        }
        true
    }
}

/// Draw one half-open rectangle in the same pixel grid and drawing order as a
/// full render. Callers supply every intersecting entry and platform in the
/// same order as the full scene, including platforms in the lighting halo.
pub fn raster_patch(
    entries: &[RenderEntry],
    platforms: &[Platform],
    view: Rect,
    requested: [usize; 2],
    region: [usize; 4],
    token: &AtomicU64,
    generation: u64,
) -> Option<RasterPatch> {
    if !valid(view) || cancelled(token, generation) {
        return None;
    }
    let size = dimensions(requested);
    let [x0, y0, x1, y1] = region;
    if x0 >= x1 || y0 >= y1 || x1 > size[0] || y1 > size[1] {
        return None;
    }
    let width = x1 - x0;
    let height = y1 - y0;
    let sx = size[0] as f32 / view.w;
    let sy = size[1] as f32 / view.h;
    let mut pixels = vec![BACKGROUND; width * height];
    let mut owners = vec![NONE; width * height];
    for (i, platform) in platforms.iter().enumerate() {
        if i % 1024 == 0 && cancelled(token, generation) {
            return None;
        }
        let r = platform.bounds;
        let ax = (((r.x - view.x) * sx - 0.5).ceil().max(0.0) as usize)
            .max(x0)
            .min(x1);
        let bx = ((((r.x + r.w) - view.x) * sx - 0.5).ceil().max(0.0) as usize)
            .max(x0)
            .min(x1);
        let ay = (((r.y - view.y) * sy - 0.5).ceil().max(0.0) as usize)
            .max(y0)
            .min(y1);
        let by = ((((r.y + r.h) - view.y) * sy - 0.5).ceil().max(0.0) as usize)
            .max(y0)
            .min(y1);
        for y in ay..by {
            let wy = view.y + (y as f32 + 0.5) / sy;
            for x in ax..bx {
                let wx = view.x + (x as f32 + 0.5) / sx;
                if let Some(color) = platform.sample(wx, wy) {
                    pixels[(y - y0) * width + x - x0] = color;
                }
            }
        }
    }
    for (i, entry) in entries.iter().enumerate() {
        if i % 4096 == 0 && cancelled(token, generation) {
            return None;
        }
        let r = entry.rect;
        if !valid(r) || entry.id >= NONE as usize {
            continue;
        }
        let ax = (((r.x - view.x) * sx - 0.5).ceil().max(0.0) as usize).min(size[0]);
        let bx = ((((r.x + r.w) - view.x) * sx - 0.5).ceil().max(0.0) as usize).min(size[0]);
        let ay = (((r.y - view.y) * sy - 0.5).ceil().max(0.0) as usize).min(size[1]);
        let by = ((((r.y + r.h) - view.y) * sy - 0.5).ceil().max(0.0) as usize).min(size[1]);
        if ax >= bx || ay >= by {
            continue;
        }
        let cx = ax.max(x0);
        let dx = bx.min(x1);
        let cy = ay.max(y0);
        let dy = by.min(y1);
        if cx >= dx || cy >= dy {
            continue;
        }
        let u = (view.x as f64 - r.x as f64 + (ax as f64 + 0.5) / sx as f64) / r.w as f64;
        let shade_step = [
            (-FILE_SHADE_X / (sx as f64 * r.w as f64)) as f32,
            (-FILE_SHADE_Y / (sy as f64 * r.h as f64)) as f32,
        ];
        for y in cy..dy {
            let band_y0 = ay.max((y / BAND_ROWS) * BAND_ROWS);
            let v = (view.y as f64 - r.y as f64 + (band_y0 as f64 + 0.5) / sy as f64) / r.h as f64;
            let base = (FILE_SHADE_LIGHT - FILE_SHADE_X * u - FILE_SHADE_Y * v) as f32;
            let row = base + (y - band_y0) as f32 * shade_step[1];
            for x in cx..dx {
                let n = (y - y0) * width + x - x0;
                pixels[n] = shaded(entry.color, row + (x - ax) as f32 * shade_step[0]);
                owners[n] = entry.id as u32;
            }
        }
    }
    if !platforms.is_empty() {
        let field = HeightField::build_region(platforms, view, size, region, token, generation)?;
        for y in y0..y1 {
            if cancelled(token, generation) {
                return None;
            }
            let wy = view.y + (y as f32 + 0.5) / sy;
            for x in x0..x1 {
                let n = (y - y0) * width + x - x0;
                if pixels[n] != BACKGROUND || owners[n] != NONE {
                    let wx = view.x + (x as f32 + 0.5) / sx;
                    pixels[n] = illuminate(pixels[n], &field, wx, wy);
                }
            }
        }
    }
    if cancelled(token, generation) {
        return None;
    }
    Some(RasterPatch {
        origin: [x0, y0],
        image: ColorImage::new([width, height], pixels),
        owners,
        full_size: size,
    })
}

// A small, fixed grid keeps index overhead bounded independently of the
// physical output size. Items spanning many cells are retained once in the
// broad list instead of being copied into every bucket.
const RASTER_GRID: usize = 16;
const RASTER_BUCKET_LIMIT: usize = 8;

#[derive(Clone)]
struct RasterBuckets {
    cells: Vec<Arc<Vec<u32>>>,
    broad: Arc<Vec<u32>>,
}

impl RasterBuckets {
    fn new() -> Self {
        Self {
            cells: (0..RASTER_GRID * RASTER_GRID)
                .map(|_| Arc::new(Vec::new()))
                .collect(),
            broad: Arc::new(Vec::new()),
        }
    }

    fn insert(&mut self, index: u32, rect: Rect, domain: Rect) {
        let Some([x0, y0, x1, y1]) = raster_cells(rect, domain) else {
            return;
        };
        if (x1 - x0) * (y1 - y0) > RASTER_BUCKET_LIMIT {
            insert_sorted(Arc::make_mut(&mut self.broad), index);
            return;
        }
        for y in y0..y1 {
            for x in x0..x1 {
                insert_sorted(Arc::make_mut(&mut self.cells[y * RASTER_GRID + x]), index);
            }
        }
    }

    fn remove(&mut self, index: u32, rect: Rect, domain: Rect) {
        let Some([x0, y0, x1, y1]) = raster_cells(rect, domain) else {
            return;
        };
        if (x1 - x0) * (y1 - y0) > RASTER_BUCKET_LIMIT {
            remove_sorted(Arc::make_mut(&mut self.broad), index);
        } else {
            for y in y0..y1 {
                for x in x0..x1 {
                    remove_sorted(Arc::make_mut(&mut self.cells[y * RASTER_GRID + x]), index);
                }
            }
        }
    }

    fn query(&self, rect: Rect, domain: Rect) -> Vec<u32> {
        let Some([x0, y0, x1, y1]) = raster_cells(rect, domain) else {
            return Vec::new();
        };
        let mut found = self.broad.as_ref().clone();
        for y in y0..y1 {
            for x in x0..x1 {
                found.extend_from_slice(&self.cells[y * RASTER_GRID + x]);
            }
        }
        // The full raster draws in source order. Sorting also removes entries
        // duplicated in adjacent grid cells, preserving ownership and lighting.
        found.sort_unstable();
        found.dedup();
        found
    }
}

fn insert_sorted(items: &mut Vec<u32>, value: u32) {
    match items.binary_search(&value) {
        Ok(_) => {}
        Err(i) => items.insert(i, value),
    }
}
fn remove_sorted(items: &mut Vec<u32>, value: u32) {
    if let Ok(i) = items.binary_search(&value) {
        items.remove(i);
    }
}

fn raster_cells(rect: Rect, domain: Rect) -> Option<[usize; 4]> {
    if !valid(rect) || !valid(domain) {
        return None;
    }
    let x0 = rect.x.max(domain.x);
    let y0 = rect.y.max(domain.y);
    let x1 = (rect.x + rect.w).min(domain.x + domain.w);
    let y1 = (rect.y + rect.h).min(domain.y + domain.h);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let cell = |value: f32, start: f32, extent: f32| {
        (((value - start) / extent * RASTER_GRID as f32).floor() as usize).min(RASTER_GRID - 1)
    };
    Some([
        cell(x0, domain.x, domain.w),
        cell(y0, domain.y, domain.h),
        cell(x1, domain.x, domain.w) + 1,
        cell(y1, domain.y, domain.h) + 1,
    ])
}

// Entries without a sampled pixel cannot affect either the displayed texture
// or the owner map. Match `raster_patch` rounding exactly so an invisible
// subpixel entry does not consume a bucket in very large scenes.
fn entry_has_sample(entry: &RenderEntry, view: Rect, size: [usize; 2]) -> bool {
    let r = entry.rect;
    if !valid(r) || entry.id >= NONE as usize {
        return false;
    }
    let sx = size[0] as f32 / view.w;
    let sy = size[1] as f32 / view.h;
    let ax = (((r.x - view.x) * sx - 0.5).ceil().max(0.0) as usize).min(size[0]);
    let bx = ((((r.x + r.w) - view.x) * sx - 0.5).ceil().max(0.0) as usize).min(size[0]);
    let ay = (((r.y - view.y) * sy - 0.5).ceil().max(0.0) as usize).min(size[1]);
    let by = ((((r.y + r.h) - view.y) * sy - 0.5).ceil().max(0.0) as usize).min(size[1]);
    ax < bx && ay < by
}

/// Both initial contiguous geometry and persistent paged snapshots can feed
/// the spatial index without flattening the latter.
pub enum EntryView<'a> {
    Slice(&'a [RenderEntry]),
    Paged(&'a PagedVec<RenderEntry>),
}
impl<'a> From<&'a [RenderEntry]> for EntryView<'a> {
    fn from(value: &'a [RenderEntry]) -> Self {
        Self::Slice(value)
    }
}
impl<'a> From<&'a Vec<RenderEntry>> for EntryView<'a> {
    fn from(value: &'a Vec<RenderEntry>) -> Self {
        Self::Slice(value)
    }
}
impl<'a> From<&'a PagedVec<RenderEntry>> for EntryView<'a> {
    fn from(value: &'a PagedVec<RenderEntry>) -> Self {
        Self::Paged(value)
    }
}
impl EntryView<'_> {
    fn borrowed(&self) -> EntryView<'_> {
        match self {
            Self::Slice(items) => EntryView::Slice(items),
            Self::Paged(items) => EntryView::Paged(items),
        }
    }
    fn len(&self) -> usize {
        match self {
            Self::Slice(items) => items.len(),
            Self::Paged(items) => items.len(),
        }
    }
    fn iter(&self) -> Box<dyn Iterator<Item = &RenderEntry> + '_> {
        match self {
            Self::Slice(items) => Box::new(items.iter()),
            Self::Paged(items) => Box::new(items.iter()),
        }
    }
    fn get(&self, index: usize) -> &RenderEntry {
        match self {
            Self::Slice(items) => &items[index],
            Self::Paged(items) => &items[index],
        }
    }
}

/// Index data is bound to one scene geometry and output grid. Buckets are
/// copy-on-write, so publishing a new frame never mutates an older frame.
#[derive(Clone)]
struct RasterSpatial {
    size: [usize; 2],
    domain: Rect,
    halo: f32,
    entry_buckets: RasterBuckets,
    platform_buckets: RasterBuckets,
}
impl RasterSpatial {
    fn build(
        entries: EntryView<'_>,
        platforms: &[Platform],
        view: Rect,
        requested: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        if !valid(view)
            || entries.len() >= NONE as usize
            || platforms.len() >= NONE as usize
            || cancelled(token, generation)
        {
            return None;
        }
        let size = dimensions(requested);
        let step = (view.w / size[0] as f32)
            .max(view.h / size[1] as f32)
            .max(0.25);
        let halo = LIGHT_REACH + 3.0 * step;
        let domain = Rect {
            x: view.x - halo,
            y: view.y - halo,
            w: view.w + 2.0 * halo,
            h: view.h + 2.0 * halo,
        };
        if !valid(domain) {
            return None;
        }
        let mut entry_buckets = RasterBuckets::new();
        for (i, entry) in entries.iter().enumerate() {
            if i % 4096 == 0 && cancelled(token, generation) {
                return None;
            }
            if entry_has_sample(entry, view, size) {
                entry_buckets.insert(i as u32, entry.rect, domain);
            }
        }
        let mut platform_buckets = RasterBuckets::new();
        for (i, platform) in platforms.iter().enumerate() {
            if i % 1024 == 0 && cancelled(token, generation) {
                return None;
            }
            platform_buckets.insert(i as u32, platform.bounds, domain);
        }
        (!cancelled(token, generation)).then_some(Self {
            size,
            domain,
            halo,
            entry_buckets,
            platform_buckets,
        })
    }
}

/// A borrowed view of spatial buckets belonging to the same immutable frame.
pub struct RasterIndex<'a> {
    entries: EntryView<'a>,
    platforms: &'a [Platform],
    live: Option<&'a PagedVec<u32>>,
    view: Rect,
    requested: [usize; 2],
    size: [usize; 2],
    domain: Rect,
    halo: f32,
    entry_buckets: RasterBuckets,
    platform_buckets: RasterBuckets,
}
impl<'a> RasterIndex<'a> {
    pub fn new(
        entries: impl Into<EntryView<'a>>,
        platforms: &'a [Platform],
        view: Rect,
        requested: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        let entries = entries.into();
        let spatial = RasterSpatial::build(
            entries.borrowed(),
            platforms,
            view,
            requested,
            token,
            generation,
        )?;
        Some(Self {
            entries,
            platforms,
            live: None,
            view,
            requested,
            size: spatial.size,
            domain: spatial.domain,
            halo: spatial.halo,
            entry_buckets: spatial.entry_buckets,
            platform_buckets: spatial.platform_buckets,
        })
    }

    /// Generate disjoint half-open patches. `max_pixels` bounds each patch,
    /// not the memory retained by the callback. The callback must stage all
    /// patches until it can publish matching texture and owner data together.
    /// Cancellation or a false callback stops generation and returns `None`;
    /// callers must discard any patches staged for that incomplete transaction.
    pub fn for_each_patch(
        &self,
        region: [usize; 4],
        max_pixels: usize,
        token: &AtomicU64,
        generation: u64,
        stage: impl FnMut(RasterPatch) -> bool,
    ) -> Option<()> {
        self.for_each_patch_filtered(region, max_pixels, token, generation, |_| true, stage)
    }

    /// As `for_each_patch`, retaining only selected entries in each patch.
    /// The index still belongs to the immutable original scene; the predicate
    /// changes drawing, never the index's scene/version validity.
    pub fn for_each_patch_filtered(
        &self,
        region: [usize; 4],
        max_pixels: usize,
        token: &AtomicU64,
        generation: u64,
        keep: impl Fn(&RenderEntry) -> bool,
        mut stage: impl FnMut(RasterPatch) -> bool,
    ) -> Option<()> {
        let [x0, y0, x1, y1] = region;
        if x0 >= x1 || y0 >= y1 || x1 > self.size[0] || y1 > self.size[1] || max_pixels == 0 {
            return None;
        }
        // Square-ish tiles keep the lighting halo reasonable even for very
        // wide regions and retain a strict per-patch pixel bound.
        let tile_width = (max_pixels as f64).sqrt().floor() as usize;
        let tile_width = tile_width.max(1).min(x1 - x0);
        let tile_height = (max_pixels / tile_width).max(1);
        let mut y = y0;
        while y < y1 {
            let end_y = y.saturating_add(tile_height).min(y1);
            let mut x = x0;
            while x < x1 {
                if cancelled(token, generation) {
                    return None;
                }
                let end_x = x.saturating_add(tile_width).min(x1);
                let world = Rect {
                    x: self.view.x + self.view.w * x as f32 / self.size[0] as f32,
                    y: self.view.y + self.view.h * y as f32 / self.size[1] as f32,
                    w: self.view.w * (end_x - x) as f32 / self.size[0] as f32,
                    h: self.view.h * (end_y - y) as f32 / self.size[1] as f32,
                };
                let light_region = Rect {
                    x: world.x - self.halo,
                    y: world.y - self.halo,
                    w: world.w + 2.0 * self.halo,
                    h: world.h + 2.0 * self.halo,
                };
                let entries: Vec<_> = self
                    .entry_buckets
                    .query(world, self.domain)
                    .into_iter()
                    .map(|i| *self.entries.get(i as usize))
                    .filter(|entry| {
                        self.live.is_none_or(|live| {
                            live.get(entry.id).is_some_and(|&index| index != NONE)
                        }) && keep(entry)
                    })
                    .collect();
                let platforms: Vec<_> = self
                    .platform_buckets
                    .query(light_region, self.domain)
                    .into_iter()
                    .map(|i| self.platforms[i as usize])
                    .collect();
                let patch = raster_patch(
                    &entries,
                    &platforms,
                    self.view,
                    self.requested,
                    [x, y, end_x, end_y],
                    token,
                    generation,
                )?;
                if !stage(patch) || cancelled(token, generation) {
                    return None;
                }
                x = end_x;
            }
            y = end_y;
        }
        Some(())
    }
}

fn rasterize(
    entries: &[RenderEntry],
    platforms: &[Platform],
    view: Rect,
    requested: [usize; 2],
    token: &AtomicU64,
    generation: u64,
) -> Option<RenderRaster> {
    if !valid(view) || cancelled(token, generation) {
        return None;
    }
    let size = dimensions(requested);
    let [width, height] = size;
    let sx = width as f32 / view.w;
    let sy = height as f32 / view.h;
    let mut bands: Vec<Vec<Draw>> = (0..height.div_ceil(BAND_ROWS))
        .map(|_| Vec::new())
        .collect();
    for (index, entry) in entries.iter().enumerate() {
        if index % 4096 == 0 && cancelled(token, generation) {
            return None;
        }
        let r = entry.rect;
        if !valid(r)
            || r.x >= view.x + view.w
            || r.y >= view.y + view.h
            || r.x + r.w <= view.x
            || r.y + r.h <= view.y
        {
            continue;
        }
        // Sample pixel centres: small files never become artificial minimum-size
        // blocks that overwrite their neighbours. Every drawn pixel has exactly
        // one original file id; further zoom renders finer original rectangles.
        let x0 = (((r.x - view.x) * sx - 0.5).ceil().max(0.0) as usize).min(width);
        let x1 = ((((r.x + r.w) - view.x) * sx - 0.5).ceil().max(0.0) as usize).min(width);
        let y0 = (((r.y - view.y) * sy - 0.5).ceil().max(0.0) as usize).min(height);
        let y1 = ((((r.y + r.h) - view.y) * sy - 0.5).ceil().max(0.0) as usize).min(height);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        for band in y0 / BAND_ROWS..=(y1 - 1) / BAND_ROWS {
            let band_y0 = y0.max(band * BAND_ROWS);
            let u = (view.x as f64 - r.x as f64 + (x0 as f64 + 0.5) / sx as f64) / r.w as f64;
            let v = (view.y as f64 - r.y as f64 + (band_y0 as f64 + 0.5) / sy as f64) / r.h as f64;
            bands[band].push(Draw {
                id: entry.id as u32,
                x0: x0 as u16,
                x1: x1 as u16,
                y0: band_y0 as u16,
                y1: y1.min((band + 1) * BAND_ROWS) as u16,
                color: entry.color,
                shade: (FILE_SHADE_LIGHT - FILE_SHADE_X * u - FILE_SHADE_Y * v) as f32,
                shade_step: [
                    (-FILE_SHADE_X / (sx as f64 * r.w as f64)) as f32,
                    (-FILE_SHADE_Y / (sy as f64 * r.h as f64)) as f32,
                ],
            });
        }
    }
    if cancelled(token, generation) {
        return None;
    }
    let mut platform_bands: Vec<Vec<(Platform, [usize; 4])>> = (0..height.div_ceil(BAND_ROWS))
        .map(|_| Vec::new())
        .collect();
    for (index, &platform) in platforms.iter().enumerate() {
        if index % 1024 == 0 && cancelled(token, generation) {
            return None;
        }
        let r = platform.bounds;
        let x0 = (((r.x - view.x) * sx - 0.5).ceil().max(0.0) as usize).min(width);
        let x1 = ((((r.x + r.w) - view.x) * sx - 0.5).ceil().max(0.0) as usize).min(width);
        let y0 = (((r.y - view.y) * sy - 0.5).ceil().max(0.0) as usize).min(height);
        let y1 = ((((r.y + r.h) - view.y) * sy - 0.5).ceil().max(0.0) as usize).min(height);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        for band in y0 / BAND_ROWS..=(y1 - 1) / BAND_ROWS {
            platform_bands[band].push((
                platform,
                [
                    x0,
                    x1,
                    y0.max(band * BAND_ROWS),
                    y1.min((band + 1) * BAND_ROWS),
                ],
            ));
        }
    }
    let mut pixels = vec![BACKGROUND; width * height];
    let mut owners = vec![NONE; width * height];
    pixels
        .par_chunks_mut(width * BAND_ROWS)
        .zip(owners.par_chunks_mut(width * BAND_ROWS))
        .zip(bands.into_par_iter())
        .zip(platform_bands.into_par_iter())
        .enumerate()
        .for_each(|(band, (((pixels, owners), draws), platforms))| {
            for &(platform, [x0, x1, y0, y1]) in &platforms {
                if cancelled(token, generation) {
                    return;
                }
                for y in y0..y1 {
                    let world_y = view.y + (y as f32 + 0.5) / sy;
                    for x in x0..x1 {
                        let world_x = view.x + (x as f32 + 0.5) / sx;
                        if let Some(color) = platform.sample(world_x, world_y) {
                            let pixel = &mut pixels[(y - band * BAND_ROWS) * width + x];
                            *pixel = color;
                        }
                    }
                }
            }
            for (index, draw) in draws.into_iter().enumerate() {
                if index % 4096 == 0 && cancelled(token, generation) {
                    return;
                }
                for y in draw.y0 as usize..draw.y1 as usize {
                    let start = (y - band * BAND_ROWS) * width + draw.x0 as usize;
                    let end = start + (draw.x1 - draw.x0) as usize;
                    let row_shade = draw.shade + (y - draw.y0 as usize) as f32 * draw.shade_step[1];
                    for (offset, pixel) in pixels[start..end].iter_mut().enumerate() {
                        *pixel = shaded(draw.color, row_shade + offset as f32 * draw.shade_step[0]);
                    }
                    owners[start..end].fill(draw.id);
                }
            }
        });
    if cancelled(token, generation) {
        return None;
    }
    if !platforms.is_empty() {
        let field = HeightField::build(platforms, view, size, token, generation)?;
        pixels
            .par_chunks_mut(width)
            .enumerate()
            .for_each(|(y, row)| {
                if cancelled(token, generation) {
                    return;
                }
                let world_y = view.y + (y as f32 + 0.5) / sy;
                for (x, pixel) in row.iter_mut().enumerate() {
                    if *pixel == BACKGROUND && owners[y * width + x] == NONE {
                        continue;
                    }
                    let world_x = view.x + (x as f32 + 0.5) / sx;
                    *pixel = illuminate(*pixel, &field, world_x, world_y);
                }
            });
    }
    if cancelled(token, generation) {
        return None;
    }
    Some(RenderRaster {
        image: Some(ColorImage::new(size, pixels)),
        bounds: view,
        size,
        owners: Arc::new(owners),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paged_snapshots_copy_only_changed_pages_and_keep_slot_order() {
        let original = PagedVec::from_vec((0..(PAGE_LEN * 4 + 3) as u32).collect());
        let mut next = original.clone();
        next[PAGE_LEN + 5] = 91;
        next.push(123);
        assert_eq!(original[PAGE_LEN + 5], (PAGE_LEN + 5) as u32);
        assert_eq!(next[PAGE_LEN + 5], 91);
        assert_eq!(next.len(), original.len() + 1);
        assert_eq!(next.iter().last(), Some(&123));
        assert!(Arc::ptr_eq(&original.pages[0], &next.pages[0]));
        assert!(!Arc::ptr_eq(&original.pages[1], &next.pages[1]));
        assert!(Arc::ptr_eq(&original.pages[2], &next.pages[2]));
        assert!(!Arc::ptr_eq(&original.pages[4], &next.pages[4]));
    }

    #[test]
    fn paged_scene_patch_keeps_unchanged_geometry_pages_shared() {
        let token = AtomicU64::new(1);
        let bounds = rect(0.0, 0.0, 100.0, 100.0);
        let entries: Vec<_> = (0..PAGE_LEN * 3)
            .map(|id| RenderEntry {
                id,
                rect: rect(1.0, 1.0, 2.0, 2.0),
                color: Color32::WHITE,
            })
            .collect();
        let map = RenderMap::build_with_size(entries, bounds, [8, 8], &token, 1).unwrap();
        let mut next = map.snapshot_for_update();
        let patch = raster_patch(&[], &[], bounds, [8, 8], [0, 0, 1, 1], &token, 1).unwrap();
        let changed = RenderEntry {
            id: PAGE_LEN + 5,
            rect: rect(3.0, 3.0, 2.0, 2.0),
            color: Color32::WHITE,
        };
        assert!(next.apply_scene_patch(&patch, &[(changed.id, Some(changed))], None));
        assert_eq!(map.rect(changed.id), Some(rect(1.0, 1.0, 2.0, 2.0)));
        assert_eq!(next.rect(changed.id), Some(changed.rect));
        assert!(Arc::ptr_eq(&map.entries.pages[0], &next.entries.pages[0]));
        assert!(!Arc::ptr_eq(&map.entries.pages[1], &next.entries.pages[1]));
        assert!(Arc::ptr_eq(&map.entries.pages[2], &next.entries.pages[2]));
        assert!(Arc::ptr_eq(&map.by_id.pages[0], &next.by_id.pages[0]));
    }
    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    fn entries() -> Vec<RenderEntry> {
        vec![
            RenderEntry {
                id: 0,
                rect: rect(0.0, 0.0, 50.0, 100.0),
                color: Color32::RED,
            },
            RenderEntry {
                id: 1,
                rect: rect(50.0, 0.0, 49.999, 100.0),
                color: Color32::BLUE,
            },
            RenderEntry {
                id: 2,
                rect: rect(99.999, 0.0, 0.001, 100.0),
                color: Color32::GREEN,
            },
        ]
    }
    #[test]
    fn persistent_spatial_index_tracks_edits_without_mutating_old_frame() {
        let token = AtomicU64::new(8);
        let bounds = rect(-20.0, 10.0, 120.0, 100.0);
        let size = [240, 200];
        let files = vec![
            RenderEntry {
                id: 0,
                rect: rect(-20.0, 10.0, 30.0, 100.0),
                color: Color32::RED,
            },
            RenderEntry {
                id: 1,
                rect: rect(10.0, 10.0, 30.0, 100.0),
                color: Color32::GREEN,
            },
            RenderEntry {
                id: 2,
                rect: rect(40.0, 10.0, 60.0, 100.0),
                color: Color32::BLUE,
            },
        ];
        let old = RenderMap::build_with_size(files, bounds, size, &token, 8).unwrap();
        let new_platform = Platform::new(bounds, Color32::GRAY, 0, 0.5).unwrap();
        let moved = RenderEntry {
            id: 1,
            rect: rect(20.0, 20.0, 40.0, 80.0),
            color: Color32::GREEN,
        };
        let added = RenderEntry {
            id: 3,
            rect: rect(-20.0, 10.0, 20.0, 100.0),
            color: Color32::YELLOW,
        };
        let next = old
            .stage_geometry(
                &[(0, None), (1, Some(moved)), (3, Some(added))],
                Some(vec![new_platform]),
            )
            .unwrap();
        assert_eq!(old.rect(0), Some(rect(-20.0, 10.0, 30.0, 100.0)));
        assert_eq!(old.rect(1), Some(rect(10.0, 10.0, 30.0, 100.0)));
        assert_eq!(next.rect(0), None);
        assert_eq!(next.rect(1), Some(moved.rect));
        assert!(old.stage_geometry(&[(0, None), (0, None)], None).is_none());
        for scene in [&old, &next] {
            let mut patches = Vec::new();
            scene
                .raster_index()
                .for_each_patch([0, 0, size[0], size[1]], 1500, &token, 8, |patch| {
                    patches.push(patch);
                    true
                })
                .unwrap();
            let combined = RasterPatch::assemble([0, 0, size[0], size[1]], &patches).unwrap();
            let live: Vec<_> = scene
                .entries()
                .iter()
                .copied()
                .filter(|e| scene.rect(e.id).is_some() && valid(e.rect))
                .collect();
            let full = rasterize(&live, scene.platforms(), bounds, size, &token, 8).unwrap();
            assert_eq!(combined.image.pixels, full.image.unwrap().pixels);
            assert_eq!(combined.owners, *full.owners);
        }
        let resized = next.resample_tiled([480, 400], &[], &token, 8).unwrap();
        assert_eq!(resized.raster_index().size, [480, 400]);
        assert_eq!(next.raster_index().size, size);
    }

    #[test]
    fn staged_sparse_projection_slot_keeps_tombstones_and_owner_consistent() {
        let token = AtomicU64::new(9);
        let bounds = rect(0.0, 0.0, 100.0, 100.0);
        let size = [100, 100];
        let old = RenderMap::build_with_slots(Vec::new(), Vec::new(), bounds, size, 0, &token, 9)
            .unwrap();
        let entry = RenderEntry {
            id: 1,
            rect: rect(10.0, 10.0, 80.0, 80.0),
            color: Color32::GREEN,
        };
        // Slot zero is a tombstone in the authoritative projection.
        assert!(old.stage_geometry(&[(1, Some(entry))], None).is_none());
        assert!(
            old.stage_geometry_with_slots(&[(1, Some(entry))], None, 1)
                .is_none()
        );
        let mut next = old
            .stage_geometry_with_slots(&[(1, Some(entry))], None, 2)
            .unwrap();
        let mut patches = Vec::new();
        next.raster_index()
            .for_each_patch([0, 0, 100, 100], 2048, &token, 9, |patch| {
                patches.push(patch);
                true
            })
            .unwrap();
        assert!(next.apply_scene_patches(&patches, &[], None));
        assert_eq!(old.hit_test(50.0, 50.0), None);
        assert_eq!(next.rect(0), None);
        assert_eq!(next.hit_test(50.0, 50.0), Some(1));
        let full = rasterize(&[entry], &[], bounds, size, &token, 9).unwrap();
        assert_eq!(*next.base.owners, *full.owners);
    }
    #[test]
    fn tiled_resize_preserves_tombstones_geometry_and_pixel_ownership() {
        let token = AtomicU64::new(3);
        let bounds = rect(-20.0, 15.0, 100.0, 80.0);
        let files = vec![
            RenderEntry {
                id: 0,
                rect: rect(-20.0, 15.0, 50.0, 80.0),
                color: Color32::RED,
            },
            RenderEntry {
                id: 1,
                rect: rect(30.0, 15.0, 50.0, 80.0),
                color: Color32::BLUE,
            },
        ];
        let platforms = vec![Platform::new(bounds, Color32::GRAY, 0, 0.5).unwrap()];
        let mut map = RenderMap::build_with_platforms(
            files.clone(),
            platforms.clone(),
            bounds,
            [100, 80],
            &token,
            3,
        )
        .unwrap();
        let holes = map.erase_many(&[0], &token, 3).unwrap();
        let resized = map.resample_tiled([1000, 800], &[], &token, 3).unwrap();
        let full = rasterize(&files[1..], &platforms, bounds, [1000, 800], &token, 3).unwrap();
        assert_eq!(
            resized.image.as_ref().unwrap().pixels,
            full.image.unwrap().pixels
        );
        assert_eq!(*resized.base.owners, *full.owners);
        assert_eq!(resized.rect(0), None);
        assert_eq!(resized.rect(1), map.rect(1));
        assert_eq!(resized.hit_test(50.0, 40.0), Some(1));
        let masked = map.resample_tiled([1000, 800], &holes, &token, 3).unwrap();
        assert_eq!(masked.hit_test(0.0, 40.0), None);
        assert_eq!(masked.hit_test(50.0, 40.0), Some(1));
        token.store(4, Ordering::Relaxed);
        assert!(map.resample_tiled([1000, 800], &holes, &token, 3).is_none());
    }
    #[test]
    fn indexed_tiles_match_full_scene_with_lighting_and_nonzero_origin() {
        let token = AtomicU64::new(7);
        let bounds = rect(-37.0, 14.0, 213.0, 177.0);
        let platforms = vec![
            Platform::new(bounds, Color32::GRAY, 0, 0.5).unwrap(),
            Platform::new(rect(-18.0, 42.0, 92.0, 91.0), Color32::GRAY, 1, 0.5).unwrap(),
            Platform::new(rect(100.0, 90.0, 70.0, 76.0), Color32::DARK_GRAY, 2, 0.5).unwrap(),
        ];
        let entries = vec![
            RenderEntry {
                id: 1,
                rect: rect(-33.0, 19.0, 100.0, 84.0),
                color: Color32::RED,
            },
            RenderEntry {
                id: 2,
                rect: rect(40.0, 85.0, 126.0, 90.0),
                color: Color32::BLUE,
            },
            RenderEntry {
                id: 3,
                rect: rect(125.0, 31.0, 0.01, 150.0),
                color: Color32::GREEN,
            },
        ];
        for size in [[213, 177], [426, 354]] {
            let full = rasterize(&entries, &platforms, bounds, size, &token, 7).unwrap();
            let index = RasterIndex::new(&entries, &platforms, bounds, size, &token, 7).unwrap();
            let mut patches = Vec::new();
            index
                .for_each_patch([0, 0, size[0], size[1]], 1200, &token, 7, |patch| {
                    assert!(patch.image.size[0] * patch.image.size[1] <= 1200);
                    patches.push(patch);
                    true
                })
                .unwrap();
            let combined = RasterPatch::assemble([0, 0, size[0], size[1]], &patches).unwrap();
            assert_eq!(combined.image.pixels, full.image.unwrap().pixels);
            assert_eq!(combined.owners, *full.owners);
            assert!(RasterPatch::assemble([0, 0, size[0], size[1]], &patches[1..]).is_none());
        }
    }

    #[test]
    fn indexed_tiles_cancel_and_scene_patch_batch_is_atomic() {
        let token = AtomicU64::new(3);
        let bounds = rect(0.0, 0.0, 100.0, 100.0);
        let entries = entries();
        let index = RasterIndex::new(&entries, &[], bounds, [100, 100], &token, 3).unwrap();
        let mut patches = Vec::new();
        assert!(
            index
                .for_each_patch([0, 0, 100, 100], 256, &token, 3, |patch| {
                    patches.push(patch);
                    token.store(4, Ordering::Relaxed);
                    true
                })
                .is_none()
        );
        assert_eq!(patches.len(), 1);
        token.store(3, Ordering::Relaxed);
        let mut map =
            RenderMap::build_with_size(entries.clone(), bounds, [100, 100], &token, 3).unwrap();
        let old = map.hit_test(25.0, 25.0);
        let mut wrong = patches.remove(0);
        wrong.owners[0] = 999_999;
        assert!(!map.apply_scene_patches(&[wrong], &[(0, None)], None));
        assert_eq!(map.hit_test(25.0, 25.0), old);

        let first =
            raster_patch(&entries, &[], bounds, [100, 100], [0, 0, 20, 20], &token, 3).unwrap();
        let mut last = raster_patch(
            &entries,
            &[],
            bounds,
            [100, 100],
            [80, 80, 100, 100],
            &token,
            3,
        )
        .unwrap();
        last.owners[0] = 999_999;
        assert!(!map.apply_scene_patches(&[first, last], &[(0, None)], None));
        assert_eq!(map.hit_test(25.0, 25.0), old);
        assert!(map.rect(0).is_some());

        let first =
            raster_patch(&entries, &[], bounds, [100, 100], [0, 0, 20, 20], &token, 3).unwrap();
        let overlap = raster_patch(
            &entries,
            &[],
            bounds,
            [100, 100],
            [19, 19, 40, 40],
            &token,
            3,
        )
        .unwrap();
        assert!(!map.apply_scene_patches(&[first, overlap], &[], None));
        assert_eq!(map.hit_test(25.0, 25.0), old);
    }

    #[test]
    fn index_omits_subpixel_entries_with_no_sample_but_keeps_sampled_slivers() {
        let token = AtomicU64::new(2);
        let view = rect(0.0, 0.0, 100.0, 100.0);
        let entries = vec![
            RenderEntry {
                id: 0,
                rect: rect(0.0, 0.0, 0.1, 100.0),
                color: Color32::RED,
            },
            RenderEntry {
                id: 1,
                rect: rect(0.499, 0.0, 0.002, 100.0),
                color: Color32::GREEN,
            },
        ];
        let index = RasterIndex::new(&entries, &[], view, [100, 100], &token, 2).unwrap();
        assert!(!entry_has_sample(&entries[0], view, [100, 100]));
        assert!(entry_has_sample(&entries[1], view, [100, 100]));
        let indexed: Vec<_> = index.entry_buckets.query(view, index.domain);
        assert!(!indexed.contains(&0));
        assert!(indexed.contains(&1));
        let mut patches = Vec::new();
        index
            .for_each_patch([0, 0, 100, 100], 2048, &token, 2, |patch| {
                patches.push(patch);
                true
            })
            .unwrap();
        let actual = RasterPatch::assemble([0, 0, 100, 100], &patches).unwrap();
        let full = rasterize(&entries, &[], view, [100, 100], &token, 2).unwrap();
        assert_eq!(actual.image.pixels, full.image.unwrap().pixels);
        assert_eq!(actual.owners, *full.owners);
    }

    #[test]
    fn patch_blit_copies_only_intersection_and_rejects_other_grid() {
        let mut dst = RasterPatch::blank([2, 2], [4, 4], [10, 10]).unwrap();
        let mut src = RasterPatch::blank([0, 0], [4, 4], [10, 10]).unwrap();
        src.image.pixels.fill(Color32::RED);
        src.owners.fill(7);
        assert!(dst.blit_from(&src));
        for y in 0..4 {
            for x in 0..4 {
                let expected = x < 2 && y < 2;
                assert_eq!(dst.owner_at(x, y), expected.then_some(7));
                assert_eq!(
                    dst.image.pixels[y * 4 + x],
                    if expected { Color32::RED } else { BACKGROUND }
                );
            }
        }
        let other = RasterPatch::blank([0, 0], [1, 1], [9, 9]).unwrap();
        assert!(!dst.blit_from(&other));
        assert_eq!(dst.owner_at(0, 0), Some(7));
        assert!(RasterPatch::blank([9, 9], [2, 1], [10, 10]).is_none());
    }

    #[test]
    fn indexed_sparse_candidates_keep_platform_halo_and_paint_order() {
        let token = AtomicU64::new(11);
        let view = rect(-320.0, -270.0, 800.0, 600.0);
        let platforms: Vec<_> = (0..80)
            .map(|i| {
                let x = -340.0 + ((i * 67) % 820) as f32;
                let y = -290.0 + ((i * 43) % 630) as f32;
                Platform::new(rect(x, y, 75.0, 54.0), Color32::GRAY, (i % 8) as u8, 1.0).unwrap()
            })
            .collect();
        let entries: Vec<_> = (0..300)
            .map(|i| RenderEntry {
                id: i,
                rect: rect(
                    -330.0 + ((i * 53) % 810) as f32,
                    -280.0 + ((i * 71) % 610) as f32,
                    (6 + i % 17) as f32,
                    (5 + i % 13) as f32,
                ),
                color: Color32::from_rgb((i * 3) as u8, (i * 5) as u8, (i * 7) as u8),
            })
            .collect();
        let size = [401, 301];
        let full = rasterize(&entries, &platforms, view, size, &token, 11).unwrap();
        let index = RasterIndex::new(&entries, &platforms, view, size, &token, 11).unwrap();
        let mut patches = Vec::new();
        index
            .for_each_patch([0, 0, size[0], size[1]], 4096, &token, 11, |patch| {
                patches.push(patch);
                true
            })
            .unwrap();
        let actual = RasterPatch::assemble([0, 0, size[0], size[1]], &patches).unwrap();
        assert_eq!(actual.image.pixels, full.image.unwrap().pixels);
        assert_eq!(actual.owners, *full.owners);

        // The same immutable index can raster a filtered animation layer
        // without rebuilding the spatial buckets or changing painter order.
        let filtered: Vec<_> = entries
            .iter()
            .copied()
            .filter(|entry| entry.id % 3 != 0)
            .collect();
        let expected = rasterize(&filtered, &platforms, view, size, &token, 11).unwrap();
        let mut filtered_patches = Vec::new();
        index
            .for_each_patch_filtered(
                [0, 0, size[0], size[1]],
                4096,
                &token,
                11,
                |entry| entry.id % 3 != 0,
                |patch| {
                    filtered_patches.push(patch);
                    true
                },
            )
            .unwrap();
        let actual = RasterPatch::assemble([0, 0, size[0], size[1]], &filtered_patches).unwrap();
        assert_eq!(actual.image.pixels, expected.image.unwrap().pixels);
        assert_eq!(actual.owners, *expected.owners);
    }
    #[test]
    fn patch_matches_full_raster_across_bands_lighting_and_hidpi() {
        let token = AtomicU64::new(42);
        let bounds = rect(0.0, 0.0, 180.0, 300.0);
        let platforms = vec![
            Platform::new(bounds, Color32::GRAY, 0, 0.5).unwrap(),
            Platform::new(rect(18.0, 120.0, 95.0, 90.0), Color32::GRAY, 1, 0.5).unwrap(),
        ];
        let entries = vec![
            RenderEntry {
                id: 0,
                rect: rect(0.0, 0.0, 88.5, 300.0),
                color: Color32::RED,
            },
            RenderEntry {
                id: 1,
                rect: rect(88.5, 10.3, 50.0, 230.0),
                color: Color32::BLUE,
            },
            RenderEntry {
                id: 2,
                rect: rect(138.5, 0.0, 0.001, 300.0),
                color: Color32::GREEN,
            },
        ];
        for requested in [[180, 300], [271, 451], [4096, 3000]] {
            let full = rasterize(&entries, &platforms, bounds, requested, &token, 42).unwrap();
            let [w, h] = full.size;
            for region in [
                [0, 0, w.min(17), h.min(19)],
                [w / 4, h / 4, (w / 4 + 53).min(w), (h / 4 + 79).min(h)],
                [w - 12, h - 14, w, h],
            ] {
                let patch =
                    raster_patch(&entries, &platforms, bounds, requested, region, &token, 42)
                        .unwrap();
                let pw = region[2] - region[0];
                for y in region[1]..region[3] {
                    for x in region[0]..region[2] {
                        let a = (y - region[1]) * pw + x - region[0];
                        let b = y * w + x;
                        assert_eq!(
                            patch.image.pixels[a],
                            full.image.as_ref().unwrap().pixels[b],
                            "pixel {x},{y} size {requested:?}"
                        );
                        assert_eq!(patch.owners[a], full.owners[b]);
                    }
                }
            }
        }
        assert!(
            raster_patch(
                &entries,
                &platforms,
                bounds,
                [180, 300],
                [0, 0, 1, 1],
                &token,
                41
            )
            .is_none()
        );
    }

    #[test]
    fn scene_patch_publishes_geometry_and_owners_or_rejects_without_mutation() {
        let token = AtomicU64::new(1);
        let bounds = rect(0.0, 0.0, 100.0, 100.0);
        let mut map = RenderMap::build(entries(), bounds, &token, 1).unwrap();
        let replacement = RenderEntry {
            id: 1,
            rect: rect(50.0, 0.0, 49.999, 100.0),
            color: Color32::GREEN,
        };
        let patch = raster_patch(
            &[replacement],
            &[],
            bounds,
            [100, 100],
            [50, 0, 100, 100],
            &token,
            1,
        )
        .unwrap();
        assert!(!map.apply_scene_patch(&patch, &[(1, None)], None));
        assert_eq!(map.hit_test(70.0, 50.0), Some(1));
        assert_eq!(map.color(1), Some(Color32::BLUE));
        assert!(map.apply_scene_patch(&patch, &[(1, Some(replacement))], None));
        assert_eq!(map.hit_test(70.0, 50.0), Some(1));
        assert_eq!(map.color(1), Some(Color32::GREEN));
        assert_eq!(map.hit_test(20.0, 50.0), Some(0));
    }

    #[test]
    fn background_snapshot_batch_erase_matches_individual_erases_without_touching_live_map() {
        let token = AtomicU64::new(1);
        let bounds = rect(0.0, 0.0, 100.0, 100.0);
        let live = RenderMap::build_with_size(entries(), bounds, [231, 177], &token, 1).unwrap();
        let mut single = live.snapshot_for_update();
        let mut batch = live.snapshot_for_update();
        single.erase(0);
        single.erase(1);
        let holes = batch.erase_many(&[0, 1, 1, 999], &token, 1).unwrap();
        assert_eq!(single.base.owners.as_slice(), batch.base.owners.as_slice());
        assert!(batch.rect(0).is_none() && batch.rect(1).is_none());
        assert_eq!(live.hit_test(20.0, 50.0), Some(0));
        assert_eq!(live.hit_test(70.0, 50.0), Some(1));
        assert!(!holes.is_empty());
        for y in 0..177 {
            for x in 0..231 {
                let wx = (x as f32 + 0.5) * 100.0 / 231.0;
                let wy = (y as f32 + 0.5) * 100.0 / 177.0;
                let covered = holes
                    .iter()
                    .any(|r| wx >= r.x && wx < r.x + r.w && wy >= r.y && wy < r.y + r.h);
                assert_eq!(
                    covered,
                    live.base.owners[y * 231 + x] != NONE && batch.base.owners[y * 231 + x] == NONE
                );
            }
        }
        assert!(batch.erase_many(&[0], &token, 0).is_none());
    }

    #[test]
    fn owners_satisfy_checks_all_visible_owners_and_cancellation() {
        let token = AtomicU64::new(1);
        let mut map = RenderMap::build_with_size(
            entries(),
            rect(0.0, 0.0, 100.0, 100.0),
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        assert_eq!(map.owners_satisfy(|id| id < 2, &token, 1), Some(true));
        assert_eq!(map.owners_satisfy(|id| id == 0, &token, 1), Some(false));
        map.erase_many(&[1], &token, 1).unwrap();
        assert_eq!(map.owners_satisfy(|id| id == 0, &token, 1), Some(true));
        assert_eq!(map.owners_satisfy(|_| true, &token, 0), None);
        assert_eq!(
            map.owners_satisfy(
                |_| {
                    token.store(2, Ordering::Relaxed);
                    false
                },
                &token,
                1
            ),
            None
        );
    }

    #[test]
    fn sparse_stable_slots_accept_tombstones_and_reject_duplicate_ids() {
        let token = AtomicU64::new(1);
        let mut e = entries();
        e[1].id = 5;
        e.truncate(2);
        let map = RenderMap::build_with_slots(
            e.clone(),
            Vec::new(),
            rect(0.0, 0.0, 100.0, 100.0),
            [100, 100],
            6,
            &token,
            1,
        )
        .unwrap();
        assert_eq!(map.hit_test(70.0, 50.0), Some(5));
        let missing = std::collections::HashSet::from([2usize, 3, 4]);
        assert_eq!(map.owners_exclude(&missing, &token, 1), Some(true));
        assert_eq!(
            map.owners_exclude(&std::collections::HashSet::from([5]), &token, 1),
            Some(false)
        );
        assert_eq!(map.owners_exclude(&missing, &token, 0), None);
        assert!(
            RenderMap::build_with_slots(
                e.clone(),
                Vec::new(),
                rect(0.0, 0.0, 100.0, 100.0),
                [100, 100],
                5,
                &token,
                1
            )
            .is_none()
        );
        e[0].id = 5;
        assert!(
            RenderMap::build_with_slots(
                e,
                Vec::new(),
                rect(0.0, 0.0, 100.0, 100.0),
                [100, 100],
                6,
                &token,
                1
            )
            .is_none()
        );
    }

    #[test]
    fn shared_height_preserves_parent_edges_and_file_ownership() {
        let token = AtomicU64::new(1);
        let parent = Platform::new(rect(0.0, 0.0, 100.0, 100.0), Color32::GRAY, 0, 0.5).unwrap();
        let child = Platform::new(parent.top, Color32::GRAY, 1, 0.5).unwrap();
        let entry = RenderEntry {
            id: 0,
            rect: child.top,
            color: Color32::from_rgb(70, 145, 150),
        };
        let map = RenderMap::build_with_platforms(
            vec![entry],
            vec![parent, child],
            parent.bounds,
            [200, 200],
            &token,
            1,
        )
        .unwrap();
        let plain =
            RenderMap::build_with_size(vec![entry], parent.bounds, [200, 200], &token, 1).unwrap();
        let field =
            HeightField::build(&[parent, child], parent.bounds, [200, 200], &token, 1).unwrap();
        let only_child =
            HeightField::build(&[child], parent.bounds, [200, 200], &token, 1).unwrap();
        assert!(field.sample(1.0, 50.0) > only_child.sample(1.0, 50.0));
        for y in 0..200 {
            for x in 0..200 {
                assert_eq!(map.base.owners[y * 200 + x], plain.base.owners[y * 200 + x]);
            }
        }
        let pixels = &map.image.as_ref().unwrap().pixels;
        let base = &plain.image.as_ref().unwrap().pixels;
        assert!(pixels[100 * 200 + 3].g() > base[100 * 200 + 3].g());
        assert!(pixels[100 * 200 + 195].g() < base[100 * 200 + 195].g());
        assert!(pixels[194 * 200 + 100].g() < base[194 * 200 + 100].g());
        assert_eq!(pixels[100 * 200 + 100], base[100 * 200 + 100]);
        assert!(field.values.len() <= (MAX_SIDE + 52) * (MAX_SIDE + 52));
    }

    #[test]
    fn contact_shadow_falls_on_lower_material_and_never_on_the_higher_plane() {
        let token = AtomicU64::new(1);
        let child = Platform::new(rect(20.0, 20.0, 50.0, 50.0), Color32::GRAY, 0, 0.5).unwrap();
        let field = HeightField::build(
            &[child],
            rect(0.0, 0.0, 100.0, 100.0),
            [200, 200],
            &token,
            1,
        )
        .unwrap();
        let color = Color32::from_rgb(100, 140, 150);
        assert!(illuminate(color, &field, 70.25, 55.0).g() < color.g());
        assert!(illuminate(color, &field, 55.0, 70.25).g() < color.g());
        // Test occlusion independently from the bevel's dark-facing normal.
        assert!(contact_occlusion(&field, 70.25, 55.0) > 0.0);
        assert!(contact_occlusion(&field, 55.0, 70.25) > 0.0);
        assert_eq!(contact_occlusion(&field, 20.25, 55.0), 0.0);
        assert_eq!(contact_occlusion(&field, 55.0, 20.25), 0.0);
        assert_eq!(illuminate(color, &field, 55.0, 55.0), color);
        assert_eq!(illuminate(color, &field, 80.0, 80.0), color);
    }

    #[test]
    fn elevation_halo_keeps_crop_band_and_hidpi_world_lighting_consistent() {
        let token = AtomicU64::new(1);
        let platform =
            Platform::new(rect(20.0, 120.0, 100.0, 100.0), Color32::GRAY, 0, 0.5).unwrap();
        let bounds = rect(0.0, 0.0, 180.0, 300.0);
        let full = HeightField::build(&[platform], bounds, [180, 300], &token, 1).unwrap();
        let crop = HeightField::build(
            &[platform],
            rect(19.0, 122.0, 20.0, 30.0),
            [20, 30],
            &token,
            1,
        )
        .unwrap();
        let hidpi = HeightField::build(&[platform], bounds, [360, 600], &token, 1).unwrap();
        let color = Color32::from_rgb(70, 145, 150);
        for y in 0..30 {
            for x in 0..20 {
                let wx = 19.5 + x as f32;
                let wy = 122.5 + y as f32;
                let a = illuminate(color, &full, wx, wy);
                assert_eq!(a, illuminate(color, &crop, wx, wy));
                let b = illuminate(color, &hidpi, wx, wy);
                for (a, b) in a.to_array().into_iter().zip(b.to_array()) {
                    assert!(
                        (a as i16 - b as i16).abs() <= 16,
                        "HiDPI light mismatch at {wx},{wy}: {a} {b}"
                    );
                }
            }
        }
        for (view, size) in [
            (rect(0.0, 0.0, 2048.0, 2048.0), [2048, 2048]),
            (rect(0.0, 0.0, 512.0, 512.0), [2048, 2048]),
            (rect(20.0, 120.0, 0.01, 0.01), [2048, 2048]),
        ] {
            let field = HeightField::build(&[platform], view, size, &token, 1).unwrap();
            assert!(field.values.len() <= (MAX_SIDE + 52) * (MAX_SIDE + 52));
            eprintln!(
                "height buffer: world={view:?} pixels={size:?} height={:?} bytes={}",
                field.size,
                field.values.len() * std::mem::size_of::<f32>()
            );
        }
    }

    #[test]
    fn erase_masks_only_owned_pixels_at_hidpi_and_subpixel_boundaries() {
        let token = AtomicU64::new(1);
        for size in [[100, 100], [231, 177], [4096, 3000]] {
            let mut map = RenderMap::build_with_size(
                entries(),
                rect(0.0, 0.0, 100.0, 100.0),
                size,
                &token,
                1,
            )
            .unwrap();
            let before = map.base.owners.clone();
            let pixels = map.image.as_ref().unwrap().pixels.clone();
            let [w, h] = map.base.size;
            let holes = map.erase(1);
            assert_eq!(holes.len(), 1);
            assert_eq!(map.rect(1), None);
            assert!(map.erase(1).is_empty());
            assert!(map.erase(2).is_empty()); // No overview pixel belongs to this subpixel file.
            assert_eq!(map.image.as_ref().unwrap().pixels, pixels); // Original texture is reusable.
            for y in 0..h {
                for x in 0..w {
                    let p = (
                        100.0 * (x as f32 + 0.5) / w as f32,
                        100.0 * (y as f32 + 0.5) / h as f32,
                    );
                    let covered = holes
                        .iter()
                        .any(|r| p.0 >= r.x && p.0 < r.x + r.w && p.1 >= r.y && p.1 < r.y + r.h);
                    assert_eq!(covered, before[y * w + x] == 1);
                    let expected = (before[y * w + x] == 0).then_some(0);
                    assert_eq!(map.hit_test(p.0, p.1), expected);
                }
            }
        }
    }
    #[test]
    fn erase_large_file_coalesces_all_rows_into_one_hole() {
        let token = AtomicU64::new(1);
        let mut map = RenderMap::build_with_size(
            vec![RenderEntry {
                id: 0,
                rect: rect(0.0, 0.0, 2048.0, 2048.0),
                color: Color32::RED,
            }],
            rect(0.0, 0.0, 2048.0, 2048.0),
            [2048, 2048],
            &token,
            1,
        )
        .unwrap();
        let start = std::time::Instant::now();
        let holes = map.erase(0);
        eprintln!(
            "erase 2048x2048 owners: {:?}, {} cached mask",
            start.elapsed(),
            holes.len()
        );
        assert_eq!(holes, vec![rect(0.0, 0.0, 2048.0, 2048.0)]);
        assert!(map.base.owners.iter().all(|&owner| owner == NONE));
    }
    #[test]
    fn raster_pixel_and_hit_have_same_original_file_owner() {
        let token = AtomicU64::new(1);
        let map = RenderMap::build(entries(), rect(0.0, 0.0, 100.0, 100.0), &token, 1).unwrap();
        assert_eq!(map.hit_test(20.0, 50.0), Some(0));
        assert_eq!(map.hit_test(70.0, 50.0), Some(1));
        assert_eq!(map.hit_test(100.0, 50.0), None);
        assert_eq!(map.hit_test(f32::NAN, 50.0), None);
        assert_eq!(map.rect(2), Some(rect(99.999, 0.0, 0.001, 100.0)));
        let image = map.image.as_ref().unwrap();
        let red = image.pixels[50 * 100 + 20];
        let blue = image.pixels[50 * 100 + 70];
        assert!(red.r() > 200 && red.g() == 0 && red.b() == 0);
        assert!(blue.b() > 200 && blue.r() == 0 && blue.g() == 0);
    }
    #[test]
    fn same_colour_files_restart_gradient_without_gaps() {
        let token = AtomicU64::new(1);
        let base = Color32::from_rgb(65, 143, 151);
        let entries = vec![
            RenderEntry {
                id: 0,
                rect: rect(0.0, 0.0, 50.0, 100.0),
                color: base,
            },
            RenderEntry {
                id: 1,
                rect: rect(50.0, 0.0, 50.0, 100.0),
                color: base,
            },
        ];
        let map = RenderMap::build(entries, rect(0.0, 0.0, 100.0, 100.0), &token, 1).unwrap();
        let pixels = &map.image.as_ref().unwrap().pixels;
        for y in 0..100 {
            for x in 0..100 {
                assert_eq!(map.hit_test(x as f32 + 0.5, y as f32 + 0.5), Some(x / 50));
                assert_ne!(pixels[y * 100 + x], BACKGROUND);
            }
        }
        assert_eq!(pixels[20 * 100 + 10], pixels[20 * 100 + 60]);
        assert!(pixels[50 * 100 + 50].g() >= pixels[50 * 100 + 49].g() + 20);
        assert!(pixels[10 * 100 + 10].g() > pixels[90 * 100 + 40].g());
    }
    #[test]
    fn crop_and_hidpi_samples_keep_original_file_shading_and_ownership() {
        let token = AtomicU64::new(1);
        let map = RenderMap::build(entries(), rect(0.0, 0.0, 100.0, 100.0), &token, 1).unwrap();
        let detail = map
            .rasterize(rect(10.0, 20.0, 60.0, 40.0), [60, 40], &token, 1)
            .unwrap();
        for y in 0..40 {
            for x in 0..60 {
                assert_eq!(
                    detail.image.as_ref().unwrap().pixels[y * 60 + x],
                    map.image.as_ref().unwrap().pixels[(y + 20) * 100 + x + 10]
                );
                assert_eq!(
                    detail.hit_test(x as f32 + 10.5, y as f32 + 20.5),
                    map.hit_test(x as f32 + 10.5, y as f32 + 20.5)
                );
            }
        }
        let hi = RenderMap::build_with_size(entries(), map.bounds, [200, 200], &token, 1).unwrap();
        assert_eq!(hi.image.as_ref().unwrap().size, [200, 200]);
        for &(x, y, id) in &[
            (0.25, 0.25, 0),
            (49.75, 99.75, 0),
            (50.25, 0.25, 1),
            (99.75, 99.75, 1),
        ] {
            assert_eq!(hi.hit_test(x, y), Some(id));
        }
    }
    #[test]
    fn resized_treemaps_cover_every_pixel_without_border_holes() {
        let token = AtomicU64::new(1);
        let items: Vec<_> = (0..5000)
            .map(|id| (id, ((id % 31 + 1) as u64).pow(3)))
            .collect();
        for size in [[95, 75], [211, 93], [91, 203]] {
            let bounds = rect(0.0, 0.0, size[0] as f32, size[1] as f32);
            let geometry = crate::treemap::layout_with_cancel(&items, bounds, &token, 1).unwrap();
            let entries = geometry
                .into_iter()
                .map(|(id, rect)| RenderEntry {
                    id,
                    rect,
                    color: Color32::from_rgb(65, 143, 151),
                })
                .collect();
            let map = RenderMap::build_with_size(entries, bounds, size, &token, 1).unwrap();
            assert!(
                map.base.owners.iter().all(|&id| id != NONE),
                "unowned pixel after resize to {size:?}"
            );
        }
    }
    #[test]
    fn gradient_is_continuous_across_parallel_band_boundaries() {
        let token = AtomicU64::new(1);
        let map = RenderMap::build(
            vec![RenderEntry {
                id: 0,
                rect: rect(0.0, 0.0, 64.0, 300.0),
                color: Color32::from_rgb(65, 143, 151),
            }],
            rect(0.0, 0.0, 64.0, 300.0),
            &token,
            1,
        )
        .unwrap();
        let pixels = &map.image.as_ref().unwrap().pixels;
        for row in [127, 128, 129, 255, 256, 257] {
            let a = pixels[(row - 1) * 64 + 32];
            let b = pixels[row * 64 + 32];
            assert!(
                (a.g() as i16 - b.g() as i16).abs() <= 1,
                "band boundary at row {row}"
            );
        }
        let cropped = map
            .rasterize(rect(10.0, 120.0, 30.0, 40.0), [30, 40], &token, 1)
            .unwrap();
        for y in 0..40 {
            for x in 0..30 {
                let a = pixels[(y + 120) * 64 + x + 10];
                let b = cropped.image.as_ref().unwrap().pixels[y * 30 + x];
                assert!((a.r() as i16 - b.r() as i16).abs() <= 1);
                assert!((a.g() as i16 - b.g() as i16).abs() <= 1);
                assert!((a.b() as i16 - b.b() as i16).abs() <= 1);
            }
        }
    }
    #[test]
    fn viewport_detail_reveals_original_subpixel_file_and_translates_origin() {
        let token = AtomicU64::new(1);
        let map = RenderMap::build(entries(), rect(0.0, 0.0, 100.0, 100.0), &token, 1).unwrap();
        let detail = map
            .rasterize(rect(99.99, 10.0, 0.01, 20.0), [100, 200], &token, 1)
            .unwrap();
        assert_eq!(detail.hit_test(99.9995, 15.0), Some(2));
        assert_eq!(detail.hit_test(99.995, 15.0), Some(1));
        assert_eq!(detail.hit_test(99.98, 15.0), None);
    }
    #[test]
    fn cancellation_and_bitmap_size_are_bounded_even_at_extreme_zoom() {
        let token = AtomicU64::new(1);
        assert!(RenderMap::build(entries(), rect(0.0, 0.0, 100.0, 100.0), &token, 0).is_none());
        assert_eq!(dimensions([40000, 30000]), [2048, 1536]);
        assert_eq!(dimensions([0, 0]), [1, 1]);
        let map = RenderMap::build(entries(), rect(0.0, 0.0, 100.0, 100.0), &token, 1).unwrap();
        let detail = map
            .rasterize(map.bounds, [40000, 30000], &token, 1)
            .unwrap();
        assert_eq!(detail.image.unwrap().pixels.len(), 2048 * 1536);
        token.store(2, Ordering::Relaxed);
        assert!(map.rasterize(map.bounds, [10, 10], &token, 1).is_none());
    }
    #[test]
    #[ignore = "release-only million-file performance measurement"]
    fn benchmark_three_and_half_million_files() {
        use std::time::Instant;
        let side = 1871usize;
        let n = side * side;
        let entries = (0..n)
            .map(|id| RenderEntry {
                id,
                rect: rect((id % side) as f32, (id / side) as f32, 1.0, 1.0),
                color: if id % 2 == 0 {
                    Color32::BLUE
                } else {
                    Color32::GREEN
                },
            })
            .collect();
        let token = AtomicU64::new(1);
        let started = Instant::now();
        let map =
            RenderMap::build(entries, rect(0.0, 0.0, side as f32, side as f32), &token, 1).unwrap();
        let build = started.elapsed();
        let started = Instant::now();
        let mut checksum = 0;
        for id in 0..100_000 {
            checksum += map
                .hit_test((id % side) as f32 + 0.5, (id / side) as f32 + 0.5)
                .unwrap();
        }
        assert!(checksum > 0);
        let hit = started.elapsed();
        let started = Instant::now();
        let detail = map
            .rasterize(rect(900.0, 500.0, 100.0, 80.0), [1200, 960], &token, 1)
            .unwrap();
        let detail_time = started.elapsed();
        assert_eq!(detail.hit_test(950.5, 540.5), Some(540 * side + 950));
        let ctx = eframe::egui::Context::default();
        let started = Instant::now();
        let mut frame_times = Vec::new();
        for _ in 0..300 {
            let frame_started = Instant::now();
            let output = ctx.run(Default::default(), |ctx| {
                eframe::egui::CentralPanel::default().show(ctx, |ui| {
                    ui.painter().image(
                        eframe::egui::TextureId::User(1),
                        ui.max_rect(),
                        eframe::egui::Rect::from_min_max(
                            eframe::egui::Pos2::ZERO,
                            eframe::egui::pos2(1.0, 1.0),
                        ),
                        Color32::WHITE,
                    );
                    let _ = map.hit_test(10.5, 20.5);
                });
            });
            let primitives = ctx.tessellate(output.shapes, output.pixels_per_point);
            assert!(primitives.len() < 16);
            frame_times.push(frame_started.elapsed());
        }
        frame_times.sort_unstable();
        eprintln!(
            "files={n} build={build:?} detail={detail_time:?} hit100k={hit:?} cached_image_frame_avg={:?} cached_frame_p95={:?} bitmap_pixels={} geometry_bytes={}",
            started.elapsed() / 300,
            frame_times[285],
            map.base.owners.len(),
            map.entries.len() * std::mem::size_of::<RenderEntry>() + map.by_id.len() * 4
        );
    }
}
