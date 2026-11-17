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
    by_id: Vec<u32>,
    base: RenderRaster,
}

impl RenderMap {
    /// File ids are dataset indices; bounding the lookup table by entry count
    /// prevents arbitrary sparse ids from creating unbounded allocations.
    pub fn build(
        entries: Vec<RenderEntry>,
        bounds: Rect,
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
        let mut base = rasterize(
            &entries,
            bounds,
            [bounds.w.ceil() as usize, bounds.h.ceil() as usize],
            token,
            generation,
        )?;
        let image = base.image.take();
        Some(Self {
            image,
            bounds,
            entries,
            by_id,
            base,
        })
    }

    pub fn rect(&self, id: usize) -> Option<Rect> {
        let index = *self.by_id.get(id)?;
        (index != NONE).then(|| self.entries[index as usize].rect)
    }

    pub fn hit_test(&self, x: f32, y: f32) -> Option<usize> {
        self.base.hit_test(x, y)
    }

    /// Render just the currently visible world rectangle. The overview remains
    /// available while this cancellable detail image is produced. Never create
    /// a bitmap for an entire zoomed canvas.
    pub fn rasterize(
        &self,
        view: Rect,
        size: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<RenderRaster> {
        rasterize(&self.entries, view, size, token, generation)
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
        // Separate individually resolvable files even when their directory
        // colour is identical. Inset before viewport clipping, so panning does
        // not create spurious borders at the visible area's edges.
        let inset_x = if r.w * sx > 5.0 && r.h * sy > 5.0 {
            0.7 / sx
        } else {
            0.0
        };
        let inset_y = if inset_x > 0.0 { 0.7 / sy } else { 0.0 };
        let x0 = ((((r.x + inset_x) - view.x) * sx - 0.5).ceil().max(0.0) as usize).min(width);
        let x1 = ((((r.x + r.w - inset_x) - view.x) * sx - 0.5)
            .ceil()
            .max(0.0) as usize)
            .min(width);
        let y0 = ((((r.y + inset_y) - view.y) * sy - 0.5).ceil().max(0.0) as usize).min(height);
        let y1 = ((((r.y + r.h - inset_y) - view.y) * sy - 0.5)
            .ceil()
            .max(0.0) as usize)
            .min(height);
        if x1 <= x0 || y1 <= y0 {
            continue;
        }
        for band in y0 / BAND_ROWS..=(y1 - 1) / BAND_ROWS {
            bands[band].push(Draw {
                id: entry.id as u32,
                x0: x0 as u16,
                x1: x1 as u16,
                y0: y0.max(band * BAND_ROWS) as u16,
                y1: y1.min((band + 1) * BAND_ROWS) as u16,
                color: entry.color,
            });
        }
    }
    if cancelled(token, generation) {
        return None;
    }
    let mut pixels = vec![BACKGROUND; width * height];
    let mut owners = vec![NONE; width * height];
    pixels
        .par_chunks_mut(width * BAND_ROWS)
        .zip(owners.par_chunks_mut(width * BAND_ROWS))
        .zip(bands.into_par_iter())
        .enumerate()
        .for_each(|(band, ((pixels, owners), draws))| {
            for (index, draw) in draws.into_iter().enumerate() {
                if index % 4096 == 0 && cancelled(token, generation) {
                    return;
                }
                for y in draw.y0 as usize..draw.y1 as usize {
                    let start = (y - band * BAND_ROWS) * width + draw.x0 as usize;
                    let end = start + (draw.x1 - draw.x0) as usize;
                    pixels[start..end].fill(draw.color);
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
    fn raster_pixel_and_hit_have_same_original_file_owner() {
        let token = AtomicU64::new(1);
        let map = RenderMap::build(entries(), rect(0.0, 0.0, 100.0, 100.0), &token, 1).unwrap();
        assert_eq!(map.hit_test(20.0, 50.0), Some(0));
        assert_eq!(map.hit_test(70.0, 50.0), Some(1));
        assert_eq!(map.hit_test(100.0, 50.0), None);
        assert_eq!(map.hit_test(f32::NAN, 50.0), None);
        assert_eq!(map.rect(2), Some(rect(99.999, 0.0, 0.001, 100.0)));
        let image = map.image.as_ref().unwrap();
        assert_eq!(image.pixels[50 * 100 + 20], Color32::RED);
        assert_eq!(image.pixels[50 * 100 + 70], Color32::BLUE);
    }
    #[test]
    fn same_colour_neighbours_have_non_file_gutters() {
        let token = AtomicU64::new(1);
        let entries = vec![
            RenderEntry {
                id: 0,
                rect: rect(0.0, 0.0, 50.0, 100.0),
                color: Color32::BLUE,
            },
            RenderEntry {
                id: 1,
                rect: rect(50.0, 0.0, 50.0, 100.0),
                color: Color32::BLUE,
            },
        ];
        let map = RenderMap::build(entries, rect(0.0, 0.0, 100.0, 100.0), &token, 1).unwrap();
        assert_eq!(map.hit_test(48.5, 50.5), Some(0));
        assert_eq!(map.hit_test(49.5, 50.5), None);
        assert_eq!(map.hit_test(50.5, 50.5), None);
        assert_eq!(map.hit_test(51.5, 50.5), Some(1));
        assert_eq!(
            map.image.as_ref().unwrap().pixels[50 * 100 + 50],
            BACKGROUND
        );
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
