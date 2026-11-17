//! Cached file raster and pixel ownership. Work in this module runs off the UI
//! thread; a frame draws one texture and queries one ownership pixel.
use crate::treemap::Rect;
use eframe::egui::{Color32, ColorImage};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_SIDE: usize = 2048;
const BAND_ROWS: usize = 128;
const NONE: u32 = u32::MAX;
const BACKGROUND: Color32 = Color32::from_rgb(19, 23, 29);
// Logical-point design tokens for the shared platform material.
const MIN_PLATFORM_SIDE: f32 = 28.0;
const MIN_PLATFORM_AREA: f32 = 1800.0;
const MAX_SHOULDER: f32 = 18.0;
const MAX_RISE: f32 = 8.0;
const FILE_SHADE_LIGHT: f64 = 1.12;
const FILE_SHADE_X: f64 = 0.065;
const FILE_SHADE_Y: f64 = 0.045;

#[derive(Clone, Copy, Debug)]
pub struct Platform {
    pub bounds: Rect,
    pub top: Rect,
    pub content: Rect,
    pub color: Color32,
    rise: f32,
    bevel: f32,
}

impl Platform {
    pub fn new(bounds: Rect, color: Color32) -> Option<Self> {
        let side = bounds.w.min(bounds.h);
        if !valid(bounds) || side < MIN_PLATFORM_SIDE || bounds.w * bounds.h < MIN_PLATFORM_AREA {
            return None;
        }
        let gap = (side * 0.012).clamp(0.8, 2.0);
        let rise = (side * 0.045).clamp(2.5, MAX_RISE);
        let top = Rect {
            x: bounds.x + gap,
            y: bounds.y + gap,
            w: bounds.w - gap * 2.0 - rise * 0.25,
            h: bounds.h - gap * 2.0 - rise,
        };
        let shoulder = (side * 0.065).clamp(3.0, MAX_SHOULDER);
        let content = Rect {
            x: top.x + shoulder,
            y: top.y + shoulder,
            w: top.w - shoulder * 2.0,
            h: top.h - shoulder * 2.0,
        };
        Some(Self {
            bounds,
            top,
            content,
            color,
            rise,
            bevel: (rise * 0.42).max(1.0),
        })
    }

    // Returns material only inside this allocation. Shadows never obscure a
    // neighbouring file; every exposed shoulder/side keeps a NONE owner.
    fn sample(self, x: f32, y: f32) -> Option<Color32> {
        let r = self.top;
        if x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h {
            let left = x - r.x;
            let upper = y - r.y;
            let right = r.x + r.w - x;
            let bottom = r.y + r.h - y;
            let material = 1.055 - 0.07 * upper / r.h - 0.025 * left / r.w;
            // Continuous chamfer lighting reads as a physical surface rather
            // than a one-pixel frame around a flat rectangle.
            let smooth = |distance: f32| {
                let t = (1.0 - distance / self.bevel).clamp(0.0, 1.0);
                t * t * (3.0 - 2.0 * t)
            };
            let shade = material + 0.18 * smooth(upper) + 0.11 * smooth(left)
                - 0.20 * smooth(bottom)
                - 0.13 * smooth(right);
            return Some(shaded(self.color, shade));
        }
        let dx = self.rise * 0.25;
        if y >= r.y + r.h && y < r.y + r.h + self.rise {
            let f = (y - r.y - r.h) / self.rise;
            if x >= r.x + dx * f && x < r.x + r.w + dx * f {
                return Some(shaded(self.color, 0.75 - 0.12 * f));
            }
        }
        if x >= r.x + r.w && x < r.x + r.w + dx {
            let f = (x - r.x - r.w) / dx;
            if y >= r.y + self.rise * f && y < r.y + r.h + self.rise * f {
                return Some(shaded(self.color, 0.66 - 0.07 * f));
            }
        }
        // Premultiplied contact shadow blends with the already-painted parent
        // material instead of cutting a black outline into its shoulder.
        let distance = (x - r.x - r.w - dx)
            .max(0.0)
            .max((y - r.y - r.h - self.rise).max(0.0));
        if x >= r.x && y >= r.y && distance < 2.0 {
            return Some(Color32::from_black_alpha(
                (70.0 * (1.0 - distance / 2.0)) as u8,
            ));
        }
        None
    }
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
    owners: Vec<u32>,
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
    entries: Vec<RenderEntry>,
    #[cfg(test)]
    platforms: Vec<Platform>,
    by_id: Vec<u32>,
    base: RenderRaster,
}

impl RenderMap {
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

    pub fn build_with_platforms(
        entries: Vec<RenderEntry>,
        platforms: Vec<Platform>,
        bounds: Rect,
        size: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        if cancelled(token, generation) || !valid(bounds) {
            return None;
        }
        let mut by_id = vec![NONE; entries.len()];
        for (index, entry) in entries.iter().enumerate() {
            if index % 4096 == 0 && cancelled(token, generation) {
                return None;
            }
            if entry.id >= by_id.len() || index >= NONE as usize {
                return None;
            }
            by_id[entry.id] = index as u32;
        }
        let mut base = rasterize(&entries, &platforms, bounds, size, token, generation)?;
        let image = base.image.take();
        Some(Self {
            image,
            bounds,
            entries,
            #[cfg(test)]
            platforms,
            by_id,
            base,
        })
    }

    pub fn rect(&self, id: usize) -> Option<Rect> {
        let index = *self.by_id.get(id)?;
        (index != NONE).then(|| self.entries[index as usize].rect)
    }

    #[cfg(test)]
    pub fn platform_count(&self) -> usize {
        self.platforms.len()
    }

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
    pub fn erase(&mut self, id: usize) -> Vec<Rect> {
        let Some(rect) = self.rect(id) else {
            return Vec::new();
        };
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
            let row = &mut self.base.owners[y * width..(y + 1) * width];
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
        rasterize(
            &self.entries,
            &self.platforms,
            view,
            size,
            token,
            generation,
        )
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
            for (platform, [x0, x1, y0, y1]) in platforms {
                if cancelled(token, generation) {
                    return;
                }
                for y in y0..y1 {
                    let world_y = view.y + (y as f32 + 0.5) / sy;
                    for x in x0..x1 {
                        let world_x = view.x + (x as f32 + 0.5) / sx;
                        if let Some(color) = platform.sample(world_x, world_y) {
                            let pixel = &mut pixels[(y - band * BAND_ROWS) * width + x];
                            let inverse = 255 - color.a() as u16;
                            let blend = |front: u8, back: u8| {
                                (front as u16 + (back as u16 * inverse + 127) / 255).min(255) as u8
                            };
                            *pixel = Color32::from_rgba_premultiplied(
                                blend(color.r(), pixel.r()),
                                blend(color.g(), pixel.g()),
                                blend(color.b(), pixel.b()),
                                255,
                            );
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
    Some(RenderRaster {
        image: Some(ColorImage::new(size, pixels)),
        bounds: view,
        size,
        owners,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn platform_shoulders_and_sides_are_material_without_file_owners() {
        let token = AtomicU64::new(1);
        let platform = Platform::new(
            rect(0.0, 0.0, 100.0, 100.0),
            Color32::from_rgb(70, 145, 150),
        )
        .unwrap();
        let entry = RenderEntry {
            id: 0,
            rect: platform.content,
            color: platform.color,
        };
        let mut map = RenderMap::build_with_platforms(
            vec![entry],
            vec![platform],
            platform.bounds,
            [200, 200],
            &token,
            1,
        )
        .unwrap();
        assert_eq!(
            map.hit_test(platform.top.x + 0.5, platform.top.y + 0.5),
            None
        );
        assert_eq!(
            map.hit_test(50.0, platform.top.y + platform.top.h + 1.0),
            None
        );
        assert_eq!(map.hit_test(50.0, 50.0), Some(0));
        let highlight = platform
            .sample(platform.top.x + 5.0, platform.top.y + 0.1)
            .unwrap();
        let side = platform
            .sample(50.0, platform.top.y + platform.top.h + 1.0)
            .unwrap();
        assert!(highlight.g() > side.g() + 60);
        let before = map.image.as_ref().unwrap().pixels.clone();
        let holes = map.erase(0);
        assert_eq!(map.hit_test(50.0, 50.0), None);
        assert!(!holes.is_empty());
        assert!(
            holes
                .iter()
                .all(|hole| hole.x >= entry.rect.x - 0.5 && hole.y >= entry.rect.y - 0.5)
        );
        assert_eq!(map.image.as_ref().unwrap().pixels, before);
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
        assert!(pixels[50 * 100 + 50].g() > pixels[50 * 100 + 49].g() + 5);
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
