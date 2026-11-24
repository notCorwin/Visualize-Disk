//! Cached file raster, regional patching, and pixel ownership.
//! Work in this module runs off the UI thread; the UI draws one texture and
//! queries one ownership pixel in O(1).
use crate::treemap::Rect;
use eframe::egui::{Color32, ColorImage};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_SIDE: usize = 2048;
const BAND_ROWS: usize = 128;
pub const NONE: u32 = u32::MAX;
pub const BACKGROUND: Color32 = Color32::from_rgb(19, 23, 29);

// Logical-point design tokens for the shared platform material.
const MIN_PLATFORM_SIDE: f32 = 28.0;
const MIN_PLATFORM_AREA: f32 = 1800.0;
// Per-edge decoration is a convergent series along every directory path.
const PLATFORM_GAP: f32 = 0.1;
const PLATFORM_SHOULDER: f32 = 0.35;
const PLATFORM_RISE: f32 = 0.7;
// Material bands preserve readable light/shadow without reserving empty area.
const BEVEL_WIDTH: f32 = 3.0;
const LAYER_HEIGHT: f32 = 4.0;
pub const LIGHT_REACH: f32 = 6.0;
const DECORATION_DECAY: f32 = 0.9;
pub const FILE_SHADE_LIGHT: f64 = 1.20;
pub const FILE_SHADE_X: f64 = 0.18;
pub const FILE_SHADE_Y: f64 = 0.12;

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
        if !valid(top) {
            return None;
        }
        Some(Self {
            bounds,
            top,
            color,
            rise,
        })
    }

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

/// Elevation field covering just the requested pixel region plus its world-space
/// lighting halo so regional patches match a full-frame render bit-for-bit.
struct HeightField {
    values: Vec<f32>,
    size: [usize; 2],
    origin: [f32; 2],
    step: [f32; 2],
    inv_step: [f32; 2],
    max_g: [f32; 2],
    offset: [usize; 2],
    full_size: [usize; 2],
}

impl HeightField {
    fn build_region(
        platforms: &[Platform],
        view: Rect,
        full_pixel_size: [usize; 2],
        region: [usize; 4],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        let step = [
            view.w / full_pixel_size[0] as f32,
            view.h / full_pixel_size[1] as f32,
        ]
        .map(|s| s.max(0.25));
        let inv_step = [1.0 / step[0], 1.0 / step[1]];
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
                + (patch_low[axis] as f32 + 0.5) * (high[axis] - low[axis])
                    / full_pixel_size[axis] as f32;
            (((world - LIGHT_REACH) - origin[axis]) / step[axis])
                .floor()
                .max(0.0) as usize
        });
        let end = std::array::from_fn::<_, 2, _>(|axis| {
            let world = low[axis]
                + (patch_high[axis].saturating_sub(1) as f32 + 0.5) * (high[axis] - low[axis])
                    / full_pixel_size[axis] as f32;
            ((((world + LIGHT_REACH) - origin[axis]) / step[axis]).ceil() as usize + 2)
                .min(full_size[axis])
        });
        let size = [
            end[0].saturating_sub(offset[0]).max(1),
            end[1].saturating_sub(offset[1]).max(1),
        ];
        let mut values = vec![0.0f32; size[0] * size[1]];
        let band_count = size[1].div_ceil(BAND_ROWS);
        let mut bands: Vec<Vec<(Platform, [usize; 4])>> =
            (0..band_count).map(|_| Vec::new()).collect();
        for (index, &platform) in platforms.iter().enumerate() {
            if index % 1024 == 0 && cancelled(token, generation) {
                return None;
            }
            let r = platform.bounds;
            let p_low = [r.x, r.y];
            let p_high = [r.x + r.w, r.y + r.h];
            let start_g = std::array::from_fn::<_, 2, _>(|axis| {
                (((p_low[axis] - origin[axis]) / step[axis] - 0.5)
                    .ceil()
                    .max(0.0) as usize)
                    .min(full_size[axis])
            });
            let end_g = std::array::from_fn::<_, 2, _>(|axis| {
                (((p_high[axis] - origin[axis]) / step[axis] - 0.5)
                    .ceil()
                    .max(0.0) as usize)
                    .min(full_size[axis])
            });
            let x0 = start_g[0].max(offset[0]).saturating_sub(offset[0]);
            let x1 = end_g[0].min(offset[0] + size[0]).saturating_sub(offset[0]);
            let y0 = start_g[1].max(offset[1]).saturating_sub(offset[1]);
            let y1 = end_g[1].min(offset[1] + size[1]).saturating_sub(offset[1]);
            if x0 >= x1 || y0 >= y1 {
                continue;
            }
            for band in y0 / BAND_ROWS..=(y1 - 1) / BAND_ROWS {
                bands[band].push((
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
        let width = size[0];
        let inv_bevel = 1.0 / BEVEL_WIDTH;
        let fill_band =
            |(band, (vals, plats)): (usize, (&mut [f32], Vec<(Platform, [usize; 4])>))| {
                for (platform, [x0, x1, y0, y1]) in plats {
                    if cancelled(token, generation) {
                        return;
                    }
                    let r = platform.bounds;
                    let r_right = r.x + r.w;
                    let r_bottom = r.y + r.h;
                    // Conservative interior range where both horizontal edge
                    // distances are strictly >= BEVEL_WIDTH (constant add!).
                    let raw_in0 = (((r.x + BEVEL_WIDTH - origin[0]) * inv_step[0] - 0.5)
                        .ceil()
                        .max(0.0) as usize)
                        .saturating_add(1);
                    let raw_in1 = ((r_right - BEVEL_WIDTH - origin[0]) * inv_step[0] - 0.5)
                        .floor()
                        .max(0.0) as usize;
                    let inner_x0 = raw_in0.saturating_sub(offset[0]).clamp(x0, x1);
                    let inner_x1 = raw_in1.saturating_sub(offset[0]).clamp(inner_x0, x1);

                    for y in y0..y1 {
                        let gy = offset[1] + y;
                        let wy = origin[1] + (gy as f32 + 0.5) * step[1];
                        let dy = (wy - r.y).min(r_bottom - wy);
                        let row_base = (y - band * BAND_ROWS) * width;
                        let row = &mut vals[row_base..row_base + width];
                        for x in x0..inner_x0 {
                            let wx = origin[0] + ((offset[0] + x) as f32 + 0.5) * step[0];
                            let distance = dy.min(wx - r.x).min(r_right - wx);
                            let t = (distance * inv_bevel).clamp(0.0, 1.0);
                            row[x] += LAYER_HEIGHT * t * t * (3.0 - 2.0 * t);
                        }
                        if inner_x0 < inner_x1 {
                            let add = if dy >= BEVEL_WIDTH {
                                LAYER_HEIGHT
                            } else {
                                let t = (dy * inv_bevel).clamp(0.0, 1.0);
                                LAYER_HEIGHT * t * t * (3.0 - 2.0 * t)
                            };
                            for v in &mut row[inner_x0..inner_x1] {
                                *v += add;
                            }
                        }
                        for x in inner_x1..x1 {
                            let wx = origin[0] + ((offset[0] + x) as f32 + 0.5) * step[0];
                            let distance = dy.min(wx - r.x).min(r_right - wx);
                            let t = (distance * inv_bevel).clamp(0.0, 1.0);
                            row[x] += LAYER_HEIGHT * t * t * (3.0 - 2.0 * t);
                        }
                    }
                }
            };
        if band_count > 1 {
            values
                .par_chunks_mut(width * BAND_ROWS)
                .zip(bands.into_par_iter())
                .enumerate()
                .for_each(fill_band);
        } else {
            values
                .chunks_mut(width * BAND_ROWS)
                .zip(bands.into_iter())
                .enumerate()
                .for_each(fill_band);
        }
        (!cancelled(token, generation)).then_some(Self {
            values,
            size,
            origin,
            step,
            inv_step,
            max_g: [(full_size[0] - 1) as f32, (full_size[1] - 1) as f32],
            offset,
            full_size,
        })
    }

    #[inline(always)]
    fn sample(&self, x: f32, y: f32) -> f32 {
        let ux = ((x - self.origin[0]) * self.inv_step[0] - 0.5).clamp(0.0, self.max_g[0]);
        let uy = ((y - self.origin[1]) * self.inv_step[1] - 0.5).clamp(0.0, self.max_g[1]);
        let gx0 = ux as usize;
        let gy0 = uy as usize;
        let gx1 = (gx0 + 1).min(self.full_size[0] - 1);
        let gy1 = (gy0 + 1).min(self.full_size[1] - 1);
        let u = ux - gx0 as f32;
        let v = uy - gy0 as f32;
        let max_lx = self.size[0] - 1;
        let max_ly = self.size[1] - 1;
        let lx0 = gx0.saturating_sub(self.offset[0]).min(max_lx);
        let lx1 = gx1.saturating_sub(self.offset[0]).min(max_lx);
        let ly0 = gy0.saturating_sub(self.offset[1]).min(max_ly) * self.size[0];
        let ly1 = gy1.saturating_sub(self.offset[1]).min(max_ly) * self.size[0];
        let vals = &self.values;
        let a = vals[ly0 + lx0] * (1.0 - u) + vals[ly0 + lx1] * u;
        let b = vals[ly1 + lx0] * (1.0 - u) + vals[ly1 + lx1] * u;
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

pub fn shaded(color: Color32, shade: f32) -> Color32 {
    let alpha = color.a();
    let channel = |v: u8| (v as f32 * shade).round().min(alpha as f32) as u8;
    Color32::from_rgba_premultiplied(
        channel(color.r()),
        channel(color.g()),
        channel(color.b()),
        alpha,
    )
}

#[derive(Clone, Copy, Debug)]
pub struct RenderEntry {
    pub id: u32,
    pub rect: Rect,
    pub color: Color32,
}

#[derive(Clone)]
pub struct RasterPatch {
    pub origin: [usize; 2],
    pub size: [usize; 2],
    pub image: ColorImage,
    pub owners: Vec<u32>,
}

/// Constant-time pixel ownership grid maintained incrementally via patches.
pub struct OwnerGrid {
    pub bounds: Rect,
    pub size: [usize; 2],
    owners: Vec<u32>,
}

impl OwnerGrid {
    pub fn from_patch(bounds: Rect, patch: &RasterPatch) -> Self {
        Self {
            bounds,
            size: patch.size,
            owners: patch.owners.clone(),
        }
    }

    #[cfg(test)]
    pub fn empty(bounds: Rect, size: [usize; 2]) -> Self {
        Self {
            bounds,
            size,
            owners: vec![NONE; size[0] * size[1]],
        }
    }

    pub fn apply_patch(&mut self, patch: &RasterPatch) {
        let [full_w, full_h] = self.size;
        let [ox, oy] = patch.origin;
        let [pw, ph] = patch.size;
        if ox + pw > full_w || oy + ph > full_h || patch.owners.len() != pw * ph {
            return;
        }
        for row in 0..ph {
            let dst = (oy + row) * full_w + ox;
            let src = row * pw;
            self.owners[dst..dst + pw].copy_from_slice(&patch.owners[src..src + pw]);
        }
    }

    pub fn hit_test(&self, x: f32, y: f32) -> Option<u32> {
        if !x.is_finite()
            || !y.is_finite()
            || !valid(self.bounds)
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
        (id != NONE).then_some(id)
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
    /// Global shade formula: `base_shade + gx * shade_step[0] + gy * shade_step[1]`.
    /// Independent of patch origin and band boundaries so regional patches are
    /// bit-for-bit identical to a full-frame render.
    base_shade: f32,
    shade_step: [f32; 2],
}

pub fn valid(rect: Rect) -> bool {
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

const MAX_STREAM_SIDE: usize = 1024;

pub fn capped_size(requested: [usize; 2]) -> [usize; 2] {
    let longest = requested[0].max(requested[1]).max(1);
    let scale = (MAX_SIDE as f64 / longest as f64).min(1.0);
    requested.map(|side| ((side as f64 * scale).round() as usize).clamp(1, MAX_SIDE))
}

/// Reduced preview resolution used ONLY while an initial cold scan is streaming
/// (4x fewer pixels on Retina displays); upgraded to full `capped_size` when
/// the scan completes.
pub fn stream_preview_size(requested: [usize; 2]) -> [usize; 2] {
    let full = capped_size(requested);
    let longest = full[0].max(full[1]).max(1);
    if longest <= MAX_STREAM_SIDE {
        return full;
    }
    let scale = MAX_STREAM_SIDE as f64 / longest as f64;
    full.map(|side| ((side as f64 * scale).round() as usize).clamp(1, MAX_STREAM_SIDE))
}

pub fn logical_pixel_size(bounds: Rect, requested: [usize; 2]) -> f32 {
    let [width, height] = capped_size(requested);
    (bounds.w / width as f32).max(bounds.h / height as f32)
}

/// Convert a world-space dirty rectangle (expanded by `halo_world`) into clamped
/// `[x0, y0, x1, y1]` physical pixel coordinates.
pub fn world_to_pixel_rect(
    world: Rect,
    view: Rect,
    full_size: [usize; 2],
    halo_world: f32,
) -> Option<[usize; 4]> {
    if !valid(view) || !valid(world) {
        return None;
    }
    let [width, height] = full_size;
    let sx = width as f32 / view.w;
    let sy = height as f32 / view.h;
    let x0 = (((world.x - halo_world - view.x) * sx).floor().max(0.0) as usize).min(width);
    let y0 = (((world.y - halo_world - view.y) * sy).floor().max(0.0) as usize).min(height);
    let x1 = (((world.x + world.w + halo_world - view.x) * sx)
        .ceil()
        .max(0.0) as usize)
        .min(width);
    let y1 = (((world.y + world.h + halo_world - view.y) * sy)
        .ceil()
        .max(0.0) as usize)
        .min(height);
    (x1 > x0 && y1 > y0).then_some([x0, y0, x1, y1])
}

/// Rasterize a pixel sub-region `[x0, y0, x1, y1]` (or the entire canvas when
/// `region == [0, 0, full_size[0], full_size[1]]`). Callers need only supply
/// entries and platforms intersecting the region (expanded by `LIGHT_REACH`).
pub fn raster_region(
    entries: &[RenderEntry],
    platforms: &[Platform],
    view: Rect,
    full_size: [usize; 2],
    region: [usize; 4],
    token: &AtomicU64,
    generation: u64,
) -> Option<RasterPatch> {
    if !valid(view) || cancelled(token, generation) {
        return None;
    }
    let [full_w, full_h] = full_size;
    let [rx0, ry0, rx1, ry1] = [
        region[0].min(full_w),
        region[1].min(full_h),
        region[2].min(full_w),
        region[3].min(full_h),
    ];
    if rx1 <= rx0 || ry1 <= ry0 {
        return None;
    }
    let patch_w = rx1 - rx0;
    let patch_h = ry1 - ry0;
    let sx = full_w as f32 / view.w;
    let sy = full_h as f32 / view.h;
    let band_count = patch_h.div_ceil(BAND_ROWS);
    let mut bands: Vec<Vec<Draw>> = (0..band_count).map(|_| Vec::new()).collect();

    for (index, entry) in entries.iter().enumerate() {
        if index % 4096 == 0 && cancelled(token, generation) {
            return None;
        }
        let r = entry.rect;
        if !valid(r) {
            continue;
        }
        let x0 = (((r.x - view.x) * sx - 0.5).ceil().max(rx0 as f32) as usize).min(rx1);
        let x1 = ((((r.x + r.w) - view.x) * sx - 0.5).ceil().max(rx0 as f32) as usize).min(rx1);
        let y0 = (((r.y - view.y) * sy - 0.5).ceil().max(ry0 as f32) as usize).min(ry1);
        let y1 = ((((r.y + r.h) - view.y) * sy - 0.5).ceil().max(ry0 as f32) as usize).min(ry1);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        let lx0 = x0 - rx0;
        let lx1 = x1 - rx0;
        let ly0 = y0 - ry0;
        let ly1 = y1 - ry0;
        let u0 = (view.x as f64 - r.x as f64 + 0.5 / sx as f64) / r.w as f64;
        let v0 = (view.y as f64 - r.y as f64 + 0.5 / sy as f64) / r.h as f64;
        let base_shade = (FILE_SHADE_LIGHT - FILE_SHADE_X * u0 - FILE_SHADE_Y * v0) as f32;
        let shade_step = [
            (-FILE_SHADE_X / (sx as f64 * r.w as f64)) as f32,
            (-FILE_SHADE_Y / (sy as f64 * r.h as f64)) as f32,
        ];
        for band in ly0 / BAND_ROWS..=(ly1 - 1) / BAND_ROWS {
            let band_ly0 = ly0.max(band * BAND_ROWS);
            bands[band].push(Draw {
                id: entry.id,
                x0: lx0 as u16,
                x1: lx1 as u16,
                y0: band_ly0 as u16,
                y1: ly1.min((band + 1) * BAND_ROWS) as u16,
                color: entry.color,
                base_shade,
                shade_step,
            });
        }
    }
    if cancelled(token, generation) {
        return None;
    }

    let mut platform_bands: Vec<Vec<(Platform, [usize; 4])>> =
        (0..band_count).map(|_| Vec::new()).collect();
    for (index, &platform) in platforms.iter().enumerate() {
        if index % 1024 == 0 && cancelled(token, generation) {
            return None;
        }
        let r = platform.bounds;
        let x0 = (((r.x - view.x) * sx - 0.5).ceil().max(rx0 as f32) as usize).min(rx1);
        let x1 = ((((r.x + r.w) - view.x) * sx - 0.5).ceil().max(rx0 as f32) as usize).min(rx1);
        let y0 = (((r.y - view.y) * sy - 0.5).ceil().max(ry0 as f32) as usize).min(ry1);
        let y1 = ((((r.y + r.h) - view.y) * sy - 0.5).ceil().max(ry0 as f32) as usize).min(ry1);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        let lx0 = x0 - rx0;
        let lx1 = x1 - rx0;
        let ly0 = y0 - ry0;
        let ly1 = y1 - ry0;
        for band in ly0 / BAND_ROWS..=(ly1 - 1) / BAND_ROWS {
            platform_bands[band].push((
                platform,
                [
                    lx0,
                    lx1,
                    ly0.max(band * BAND_ROWS),
                    ly1.min((band + 1) * BAND_ROWS),
                ],
            ));
        }
    }

    let mut pixels = vec![BACKGROUND; patch_w * patch_h];
    let mut owners = vec![NONE; patch_w * patch_h];
    let inv_sx = 1.0 / sx;
    let inv_sy = 1.0 / sy;
    let paint_band = |(band, (((px, own), draws), plats)): (
        usize,
        (
            ((&mut [Color32], &mut [u32]), Vec<Draw>),
            Vec<(Platform, [usize; 4])>,
        ),
    )| {
        for &(platform, [lx0, lx1, ly0, ly1]) in &plats {
            if cancelled(token, generation) {
                return;
            }
            let r = platform.top;
            let r_right = r.x + r.w;
            let r_bottom = r.y + r.h;
            let inv_w = -0.025 / r.w;
            let inv_h = -0.07 / r.h;
            let cr = platform.color.r() as f32;
            let cg = platform.color.g() as f32;
            let cb = platform.color.b() as f32;
            let alpha = platform.color.a();
            let af = alpha as f32;
            let top_x0 = (((r.x - view.x) * sx - 0.5).ceil().max(rx0 as f32) as usize)
                .min(rx1)
                .saturating_sub(rx0)
                .clamp(lx0, lx1);
            let top_x1 = (((r_right - view.x) * sx - 0.5).ceil().max(rx0 as f32) as usize)
                .min(rx1)
                .saturating_sub(rx0)
                .clamp(top_x0, lx1);
            for ly in ly0..ly1 {
                let world_y = view.y + ((ry0 + ly) as f32 + 0.5) * inv_sy;
                let row_base = (ly - band * BAND_ROWS) * patch_w;
                if world_y >= r.y && world_y < r_bottom {
                    let row_mat = 1.055 + (world_y - r.y) * inv_h;
                    for lx in lx0..top_x0 {
                        let world_x = view.x + ((rx0 + lx) as f32 + 0.5) * inv_sx;
                        if let Some(color) = platform.sample(world_x, world_y) {
                            px[row_base + lx] = color;
                        }
                    }
                    for lx in top_x0..top_x1 {
                        let world_x = view.x + ((rx0 + lx) as f32 + 0.5) * inv_sx;
                        let material = row_mat + (world_x - r.x) * inv_w;
                        px[row_base + lx] = Color32::from_rgba_premultiplied(
                            (cr * material).round().min(af) as u8,
                            (cg * material).round().min(af) as u8,
                            (cb * material).round().min(af) as u8,
                            alpha,
                        );
                    }
                    for lx in top_x1..lx1 {
                        let world_x = view.x + ((rx0 + lx) as f32 + 0.5) * inv_sx;
                        if let Some(color) = platform.sample(world_x, world_y) {
                            px[row_base + lx] = color;
                        }
                    }
                } else {
                    for lx in lx0..lx1 {
                        let world_x = view.x + ((rx0 + lx) as f32 + 0.5) * inv_sx;
                        if let Some(color) = platform.sample(world_x, world_y) {
                            px[row_base + lx] = color;
                        }
                    }
                }
            }
        }
        for (index, draw) in draws.into_iter().enumerate() {
            if index % 4096 == 0 && cancelled(token, generation) {
                return;
            }
            let cr = draw.color.r() as f32;
            let cg = draw.color.g() as f32;
            let cb = draw.color.b() as f32;
            let alpha = draw.color.a();
            let af = alpha as f32;
            let step_x = draw.shade_step[0];
            let gx0 = rx0 + draw.x0 as usize;
            let span = (draw.x1 - draw.x0) as usize;
            for ly in draw.y0 as usize..draw.y1 as usize {
                let gy = (ry0 + ly) as f32;
                let start = (ly - band * BAND_ROWS) * patch_w + draw.x0 as usize;
                let end = start + span;
                let row_shade = draw.base_shade + gy * draw.shade_step[1];
                for (offset, pixel) in px[start..end].iter_mut().enumerate() {
                    let shade = row_shade + (gx0 + offset) as f32 * step_x;
                    *pixel = Color32::from_rgba_premultiplied(
                        (cr * shade).round().min(af) as u8,
                        (cg * shade).round().min(af) as u8,
                        (cb * shade).round().min(af) as u8,
                        alpha,
                    );
                }
                own[start..end].fill(draw.id);
            }
        }
    };
    if band_count > 1 {
        pixels
            .par_chunks_mut(patch_w * BAND_ROWS)
            .zip(owners.par_chunks_mut(patch_w * BAND_ROWS))
            .zip(bands.into_par_iter())
            .zip(platform_bands.into_par_iter())
            .enumerate()
            .for_each(paint_band);
    } else {
        pixels
            .chunks_mut(patch_w * BAND_ROWS)
            .zip(owners.chunks_mut(patch_w * BAND_ROWS))
            .zip(bands.into_iter())
            .zip(platform_bands.into_iter())
            .enumerate()
            .for_each(paint_band);
    }
    if cancelled(token, generation) {
        return None;
    }

    if !platforms.is_empty() {
        let field = HeightField::build_region(
            platforms,
            view,
            full_size,
            [rx0, ry0, rx1, ry1],
            token,
            generation,
        )?;
        // Mark only pixels within lighting reach of a platform's 4 bevel edges.
        // Strictly inside every platform interior (or outside all platforms), all
        // 8 HeightField samples share the exact same constant height, so normal
        // and contact_occlusion are identically zero and `illuminate` is a no-op.
        let mut lit_mask = vec![0u8; patch_w * patch_h];
        let pad_neg_x = 0.70 + field.step[0];
        let pad_pos_x = 3.65 + field.step[0];
        let pad_neg_y = 0.70 + field.step[1];
        let pad_pos_y = 3.65 + field.step[1];
        let to_px_lo = |world: f32, v0: f32, scale: f32, r0: usize, r1: usize| -> usize {
            (((world - v0) * scale - 0.5).floor().max(r0 as f32) as usize)
                .min(r1)
                .saturating_sub(r0)
        };
        let to_px_hi = |world: f32, v0: f32, scale: f32, r0: usize, r1: usize| -> usize {
            ((((world - v0) * scale - 0.5).ceil() + 1.0).max(r0 as f32) as usize)
                .min(r1)
                .saturating_sub(r0)
        };
        for &platform in platforms {
            let r = platform.bounds;
            let px0 = to_px_lo(r.x - pad_neg_x, view.x, sx, rx0, rx1);
            let px1 = to_px_hi(r.x + r.w + pad_pos_x, view.x, sx, rx0, rx1);
            let py0 = to_px_lo(r.y - pad_neg_y, view.y, sy, ry0, ry1);
            let py1 = to_px_hi(r.y + r.h + pad_pos_y, view.y, sy, ry0, ry1);
            if px0 >= px1 || py0 >= py1 {
                continue;
            }
            let ix0 = to_px_hi(r.x + BEVEL_WIDTH + pad_pos_x, view.x, sx, rx0, rx1).clamp(px0, px1);
            let ix1 = to_px_lo(r.x + r.w - BEVEL_WIDTH - pad_neg_x, view.x, sx, rx0, rx1)
                .clamp(ix0, px1);
            let iy0 = to_px_hi(r.y + BEVEL_WIDTH + pad_pos_y, view.y, sy, ry0, ry1).clamp(py0, py1);
            let iy1 = to_px_lo(r.y + r.h - BEVEL_WIDTH - pad_neg_y, view.y, sy, ry0, ry1)
                .clamp(iy0, py1);
            if ix0 < ix1 && iy0 < iy1 {
                for y in py0..iy0 {
                    lit_mask[y * patch_w + px0..y * patch_w + px1].fill(1);
                }
                for y in iy1..py1 {
                    lit_mask[y * patch_w + px0..y * patch_w + px1].fill(1);
                }
                for y in iy0..iy1 {
                    let row = y * patch_w;
                    lit_mask[row + px0..row + ix0].fill(1);
                    lit_mask[row + ix1..row + px1].fill(1);
                }
            } else {
                for y in py0..py1 {
                    lit_mask[y * patch_w + px0..y * patch_w + px1].fill(1);
                }
            }
        }
        let light_row = |(ly, row): (usize, &mut [Color32])| {
            if cancelled(token, generation) {
                return;
            }
            let world_y = view.y + ((ry0 + ly) as f32 + 0.5) * inv_sy;
            let row_base = ly * patch_w;
            let mask_row = &lit_mask[row_base..row_base + patch_w];
            let own_row = &owners[row_base..row_base + patch_w];
            for (lx, pixel) in row.iter_mut().enumerate() {
                if mask_row[lx] == 0 {
                    continue;
                }
                if *pixel == BACKGROUND && own_row[lx] == NONE {
                    continue;
                }
                let world_x = view.x + ((rx0 + lx) as f32 + 0.5) * inv_sx;
                *pixel = illuminate(*pixel, &field, world_x, world_y);
            }
        };
        if patch_h >= BAND_ROWS {
            pixels
                .par_chunks_mut(patch_w)
                .enumerate()
                .for_each(light_row);
        } else {
            pixels.chunks_mut(patch_w).enumerate().for_each(light_row);
        }
    }
    if cancelled(token, generation) {
        return None;
    }

    Some(RasterPatch {
        origin: [rx0, ry0],
        size: [patch_w, patch_h],
        image: ColorImage::new([patch_w, patch_h], pixels),
        owners,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regional_patch_matches_full_raster_bit_for_bit() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 300.0,
        };
        let size = [400, 300];
        let platform = Platform::new(
            Rect {
                x: 20.0,
                y: 20.0,
                w: 240.0,
                h: 200.0,
            },
            Color32::from_rgb(35, 42, 52),
            0,
            1.0,
        )
        .unwrap();
        let entries = vec![
            RenderEntry {
                id: 1,
                rect: Rect {
                    x: platform.top.x,
                    y: platform.top.y,
                    w: platform.top.w * 0.5,
                    h: platform.top.h,
                },
                color: Color32::from_rgb(0x53, 0x7c, 0x6e),
            },
            RenderEntry {
                id: 2,
                rect: Rect {
                    x: platform.top.x + platform.top.w * 0.5,
                    y: platform.top.y,
                    w: platform.top.w * 0.5,
                    h: platform.top.h,
                },
                color: Color32::from_rgb(0xb4, 0x7c, 0x66),
            },
        ];
        let token = AtomicU64::new(1);
        let full = raster_region(
            &entries,
            &[platform],
            bounds,
            size,
            [0, 0, size[0], size[1]],
            &token,
            1,
        )
        .unwrap();
        let region = [60, 40, 220, 190];
        let patch = raster_region(&entries, &[platform], bounds, size, region, &token, 1).unwrap();
        assert_eq!(patch.origin, [60, 40]);
        assert_eq!(patch.size, [160, 150]);
        for y in 0..patch.size[1] {
            for x in 0..patch.size[0] {
                let fx = region[0] + x;
                let fy = region[1] + y;
                assert_eq!(
                    patch.image.pixels[y * patch.size[0] + x],
                    full.image.pixels[fy * size[0] + fx],
                    "pixel mismatch at ({fx}, {fy})"
                );
                assert_eq!(
                    patch.owners[y * patch.size[0] + x],
                    full.owners[fy * size[0] + fx],
                    "owner mismatch at ({fx}, {fy})"
                );
            }
        }
    }

    #[test]
    fn owner_grid_applies_patch_and_hit_tests() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let mut grid = OwnerGrid::empty(bounds, [100, 100]);
        assert_eq!(grid.hit_test(50.0, 50.0), None);
        let token = AtomicU64::new(1);
        let entry = RenderEntry {
            id: 42,
            rect: Rect {
                x: 20.0,
                y: 20.0,
                w: 40.0,
                h: 40.0,
            },
            color: Color32::from_rgb(100, 120, 140),
        };
        let patch = raster_region(
            &[entry],
            &[],
            bounds,
            [100, 100],
            [15, 15, 65, 65],
            &token,
            1,
        )
        .unwrap();
        grid.apply_patch(&patch);
        assert_eq!(grid.hit_test(40.0, 40.0), Some(42));
        assert_eq!(grid.hit_test(5.0, 5.0), None);
    }

    #[test]
    fn lit_mask_and_interior_fast_path_match_unmasked_illumination_bit_for_bit() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 300.0,
        };
        let size = [400, 300];
        let outer = Platform::new(
            Rect {
                x: 10.0,
                y: 10.0,
                w: 320.0,
                h: 240.0,
            },
            Color32::from_rgb(35, 42, 52),
            0,
            1.0,
        )
        .unwrap();
        let inner = Platform::new(
            Rect {
                x: 40.0,
                y: 40.0,
                w: 140.0,
                h: 120.0,
            },
            Color32::from_rgb(42, 50, 60),
            1,
            1.0,
        )
        .unwrap();
        let platforms = [outer, inner];
        let entries = [RenderEntry {
            id: 1,
            rect: outer.top,
            color: Color32::from_rgb(0x53, 0x7c, 0x6e),
        }];
        let token = AtomicU64::new(1);
        let patch = raster_region(
            &entries,
            &platforms,
            bounds,
            size,
            [0, 0, size[0], size[1]],
            &token,
            1,
        )
        .unwrap();
        let field = HeightField::build_region(
            &platforms,
            bounds,
            size,
            [0, 0, size[0], size[1]],
            &token,
            1,
        )
        .unwrap();
        // Re-check every pixel against brute-force illumination: any pixel that
        // skipped `illuminate` via `lit_mask` must have zero light delta!
        let sx = size[0] as f32 / bounds.w;
        let sy = size[1] as f32 / bounds.h;
        for y in 0..size[1] {
            let wy = bounds.y + (y as f32 + 0.5) / sy;
            for x in 0..size[0] {
                let idx = y * size[0] + x;
                let wx = bounds.x + (x as f32 + 0.5) / sx;
                let px = patch.image.pixels[idx];
                if px == BACKGROUND && patch.owners[idx] == NONE {
                    continue;
                }
                // Check interior of `inner` (strictly inside bevel + shadow reach)
                // produces exact zero normal/occlusion.
                if wx >= inner.bounds.x + 10.0
                    && wx <= inner.bounds.x + inner.bounds.w - 10.0
                    && wy >= inner.bounds.y + 10.0
                    && wy <= inner.bounds.y + inner.bounds.h - 10.0
                {
                    assert_eq!(illuminate(px, &field, wx, wy), px);
                }
            }
        }
        assert_eq!(stream_preview_size([2048, 1200]), [1024, 600]);
        assert_eq!(stream_preview_size([900, 600]), [900, 600]);
    }
}
