//! Nested directory platforms for the cached, top-down terrace map.
use crate::{
    render::{Platform, RenderEntry},
    treemap::{self, Rect},
};
use eframe::egui::Color32;
use rayon::prelude::*;
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
struct Directory<'a> {
    path: Option<&'a Path>,
    parent: usize,
    children: Vec<usize>,
    files: Vec<(usize, u64)>,
    weight: u64,
}

pub struct DirectoryLayout {
    pub entries: Vec<RenderEntry>,
    pub platforms: Vec<Platform>,
}

pub fn layout(
    groups: BTreeMap<&Path, Vec<(usize, u64)>>,
    root: &Path,
    bounds: Rect,
    pixel_size: f32,
    token: &AtomicU64,
    generation: u64,
) -> Option<DirectoryLayout> {
    let cancelled = || token.load(Ordering::Relaxed) != generation;
    let mut directories = vec![Directory::default()];
    let mut indices: HashMap<&Path, usize> = HashMap::new();
    let mut file_count = 0;
    for (path, files) in groups {
        if cancelled() {
            return None;
        }
        file_count += files.len();
        let mut missing = Vec::new();
        let mut ancestor = Some(path);
        let mut parent = 0;
        while let Some(path) = ancestor {
            if cancelled() {
                return None;
            }
            if let Some(&index) = indices.get(path) {
                parent = index;
                break;
            }
            missing.push(path);
            ancestor = path.parent();
        }
        for path in missing.into_iter().rev() {
            let id = directories.len();
            directories.push(Directory {
                path: Some(path),
                parent,
                ..Default::default()
            });
            directories[parent].children.push(id);
            indices.insert(path, id);
            parent = id;
        }
        let mut weight = 0u64;
        for chunk in files.chunks(4096) {
            if cancelled() {
                return None;
            }
            weight = chunk
                .iter()
                .fold(weight, |sum, (_, size)| sum.saturating_add(*size));
        }
        directories[parent].weight = weight;
        directories[parent].files = files;
    }
    for id in (1..directories.len()).rev() {
        if id % 1024 == 0 && cancelled() {
            return None;
        }
        let parent = directories[id].parent;
        directories[parent].weight = directories[parent]
            .weight
            .saturating_add(directories[id].weight);
    }
    let mut entries = Vec::with_capacity(file_count);
    let mut platforms = Vec::new();
    // Iterative breadth-first publication paints a whole parent before its
    // children. Unary paths do not consume shoulder space or extra elevation.
    let mut pending = vec![(0, bounds, None::<u8>)];
    while !pending.is_empty() {
        if cancelled() {
            return None;
        }
        let pieces: Option<Vec<_>> = pending
            .into_par_iter()
            .map(|(id, bounds, inherited)| {
                if cancelled() {
                    return None;
                }
                let directory = &directories[id];
                let below_root = directory
                    .path
                    .is_some_and(|path| path != root && path.starts_with(root));
                let depth = inherited.unwrap_or(0);
                let file_color = crate::color(directory.path.unwrap_or(root));
                let factor = 1.0 + (depth as f32 * 0.035).min(0.14);
                let channel = |v: u8| (v as f32 * factor).min(255.0) as u8;
                let platform_color = Color32::from_rgb(
                    channel(crate::PANEL.r()),
                    channel(crate::PANEL.g()),
                    channel(crate::PANEL.b()),
                );

                let unary = directory.children.len() == 1 && directory.files.is_empty();
                let platform = (below_root && !unary)
                    .then(|| Platform::new(bounds, platform_color, depth, pixel_size))
                    .flatten();
                // Files and child allocations share this same top surface.
                // The only exposed shoulder is below an inset child platform.
                let content = platform.map_or(bounds, |platform| platform.top);
                let family = if below_root {
                    Some(depth.saturating_add(u8::from(platform.is_some())))
                } else {
                    None
                };
                let child_count = directory.children.len();
                let mut items = Vec::with_capacity(child_count + directory.files.len());
                for (slot, &child) in directory.children.iter().enumerate() {
                    if slot % 4096 == 0 && cancelled() {
                        return None;
                    }
                    items.push((slot, directories[child].weight));
                }
                for (slot, &(_, weight)) in directory.files.iter().enumerate() {
                    if slot % 4096 == 0 && cancelled() {
                        return None;
                    }
                    items.push((child_count + slot, weight));
                }
                let geometry = treemap::layout_with_cancel(&items, content, token, generation)?;
                let mut files = Vec::with_capacity(directory.files.len());
                let mut children = Vec::with_capacity(child_count);
                for (index, (slot, rect)) in geometry.into_iter().enumerate() {
                    if index % 4096 == 0 && cancelled() {
                        return None;
                    }
                    if slot < child_count {
                        children.push((directory.children[slot], rect, family));
                    } else {
                        files.push(RenderEntry {
                            id: directory.files[slot - child_count].0,
                            rect,
                            color: file_color,
                        });
                    }
                }
                Some((files, children, platform))
            })
            .collect();
        pending = Vec::new();
        for (mut files, children, platform) in pieces? {
            if cancelled() {
                return None;
            }
            entries.append(&mut files);
            pending.extend(children);
            if let Some(platform) = platform {
                platforms.push(platform);
            }
        }
    }
    (!cancelled()).then_some(DirectoryLayout { entries, platforms })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn bounds() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: 1200.0,
            h: 760.0,
        }
    }
    fn contains(a: Rect, b: Rect) -> bool {
        b.x >= a.x && b.y >= a.y && b.x + b.w <= a.x + a.w + 0.01 && b.y + b.h <= a.y + a.h + 0.01
    }
    fn area(r: Rect) -> f32 {
        r.w * r.h
    }

    #[test]
    fn files_use_their_direct_parent_palette_and_fit_inside_weighted_platforms() {
        let token = AtomicU64::new(1);
        let scene = layout(
            BTreeMap::from([
                (Path::new("/a"), vec![(0, 10)]),
                (Path::new("/a/nested"), vec![(1, 10), (2, 20)]),
                (Path::new("/b"), vec![(3, 40)]),
            ]),
            Path::new("/"),
            bounds(),
            1.0,
            &token,
            1,
        )
        .unwrap();
        assert_eq!(scene.entries.len(), 4);
        assert_eq!(scene.platforms.len(), 3);
        let file = |id| scene.entries.iter().find(|entry| entry.id == id).unwrap();
        assert_eq!(file(0).color, crate::color(Path::new("/a")));
        assert_eq!(file(1).color, crate::color(Path::new("/a/nested")));
        assert_eq!(file(1).color, file(2).color);
        assert_eq!(file(3).color, crate::color(Path::new("/b")));
        let focused = layout(
            BTreeMap::from([
                (Path::new("/a"), vec![(0, 10)]),
                (Path::new("/a/nested"), vec![(1, 10), (2, 20)]),
            ]),
            Path::new("/a"),
            bounds(),
            1.0,
            &token,
            1,
        )
        .unwrap();
        for entry in &focused.entries {
            assert_eq!(entry.color, file(entry.id).color);
        }
        let parent = scene
            .platforms
            .iter()
            .find(|p| (area(p.bounds) - area(bounds()) * 0.5).abs() < 0.1)
            .unwrap();
        let nested = scene
            .platforms
            .iter()
            .find(|p| area(p.bounds) < area(parent.bounds) && contains(parent.top, p.bounds))
            .unwrap();
        assert!(contains(parent.top, nested.bounds));
        assert!(contains(parent.top, file(0).rect));
        assert!(contains(nested.top, file(1).rect) && contains(nested.top, file(2).rect));
        assert!((area(file(1).rect) + area(file(2).rect) - area(nested.top)).abs() < 0.1);
        let terminal_top = nested.top;
        let terminal_bounds = nested.bounds;

        assert!((area(nested.bounds) / area(file(0).rect) - 3.0).abs() < 0.001);
        assert!((area(file(2).rect) / area(file(1).rect) - 2.0).abs() < 0.001);
        let map = crate::render::RenderMap::build_with_platforms(
            scene.entries,
            scene.platforms,
            bounds(),
            [1200, 760],
            &token,
            1,
        )
        .unwrap();

        for y in 0..760 {
            for x in 0..1200 {
                let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
                if px >= terminal_top.x
                    && px < terminal_top.x + terminal_top.w
                    && py >= terminal_top.y
                    && py < terminal_top.y + terminal_top.h
                {
                    assert!(matches!(map.hit_test(px, py), Some(1 | 2)));
                }
            }
        }
        // Query ownership at a sampled pixel centre, not a subpixel point
        // whose displayed pixel may still belong to the adjoining file.
        let side_y = (terminal_top.y + terminal_top.h - 0.5).ceil() + 0.5;
        if side_y < terminal_bounds.y + terminal_bounds.h {
            assert_eq!(
                map.hit_test(terminal_top.x + terminal_top.w * 0.5, side_y),
                None
            );
        }
        for id in 0..4 {
            let parent = ["/a", "/a/nested", "/a/nested", "/b"][id];
            assert_eq!(map.color(id), Some(crate::color(Path::new(parent))));
            let r = map.rect(id).unwrap();
            assert_eq!(map.hit_test(r.x + r.w * 0.5, r.y + r.h * 0.5), Some(id));
        }
        token.store(2, Ordering::Relaxed);
        assert!(layout(BTreeMap::new(), Path::new("/"), bounds(), 1.0, &token, 1).is_none());
    }

    #[test]
    fn unary_ancestors_and_dense_lod_do_not_consume_file_space() {
        let token = AtomicU64::new(1);
        let build = |root: &Path, leaf: &Path| {
            let scene = layout(
                BTreeMap::from([(leaf, vec![(0, 1), (1, 2)])]),
                root,
                bounds(),
                1.0,
                &token,
                1,
            )
            .unwrap();
            assert_eq!(scene.platforms.len(), 1);
            let rects: Vec<_> = scene.entries.iter().map(|e| e.rect).collect();
            rects
        };
        assert_eq!(
            build(Path::new("/"), Path::new("/a")),
            build(Path::new("/root"), Path::new("/root/one/two/a"))
        );
        let paths: Vec<_> = (0..2000)
            .map(|id| std::path::PathBuf::from(format!("/root/group{id}")))
            .collect();
        let groups = paths
            .iter()
            .enumerate()
            .map(|(id, path)| (path.as_path(), vec![(id, 1)]))
            .collect();
        let scene = layout(groups, Path::new("/root"), bounds(), 1.0, &token, 1).unwrap();
        assert_eq!(scene.entries.len(), 2000);
        assert!(scene.platforms.is_empty());
        assert!(
            scene
                .entries
                .iter()
                .all(|entry| entry.rect.w > 0.0 && entry.rect.h > 0.0)
        );
        let total: f32 = scene.entries.iter().map(|entry| area(entry.rect)).sum();
        assert!((total - area(bounds())).abs() < 4.0);
    }

    #[test]
    fn terrace_preview_and_cropped_raster_keep_the_same_world_material() {
        let token = AtomicU64::new(1);
        let paths = [
            "/Users/Pictures/Family",
            "/Users/Pictures/Trips",
            "/Users/Documents",
            "/Users/Library/Caches",
            "/Applications/Design",
            "/Applications/Development",
            "/Applications/Utilities",
            "/System/Library",
        ];
        let mut groups = BTreeMap::new();
        let mut id = 0;
        let dense = std::env::var_os("TERRACE_DENSE").is_some();
        for (group, path) in paths.iter().enumerate() {
            let files = (0..if dense { 1500 } else { 16 })
                .map(|file| {
                    let entry = (id, ((file * 17 + group * 3) % 31 + 2) as u64);
                    id += 1;
                    entry
                })
                .collect();
            groups.insert(Path::new(path), files);
        }
        let scene = layout(groups, Path::new("/"), bounds(), 1.0, &token, 1).unwrap();
        assert!(scene.platforms.len() >= 9);
        let map = crate::render::RenderMap::build_with_platforms(
            scene.entries,
            scene.platforms,
            bounds(),
            [1200, 760],
            &token,
            1,
        )
        .unwrap();
        let crop = Rect {
            x: 90.0,
            y: 100.0,
            w: 1000.0,
            h: 500.0,
        };
        let detail = map.rasterize(crop, [1000, 500], &token, 1).unwrap();
        let image = map.image.as_ref().unwrap();
        for y in 0..500 {
            for x in 0..1000 {
                let a = image.pixels[(y + 100) * 1200 + x + 90];
                let b = detail.image.as_ref().unwrap().pixels[y * 1000 + x];
                for (a, b) in a.to_array().into_iter().zip(b.to_array()) {
                    assert!((a as i16 - b as i16).abs() <= 1);
                }
                assert_eq!(
                    map.hit_test(x as f32 + 90.5, y as f32 + 100.5),
                    detail.hit_test(x as f32 + 90.5, y as f32 + 100.5)
                );
            }
        }
        if let Ok(path) = std::env::var("TERRACE_PREVIEW_PATH") {
            use std::io::Write;
            let mut file = std::fs::File::create(path).unwrap();
            write!(file, "P6\n1200 760\n255\n").unwrap();
            for pixel in &image.pixels {
                file.write_all(&pixel.to_array()[..3]).unwrap();
            }
        }
    }
}

#[cfg(test)]
mod compact_depth_tests {
    use super::*;
    use std::{io::Write, path::PathBuf};

    fn scene(mode: &str) -> DirectoryLayout {
        let mut owned = BTreeMap::<PathBuf, Vec<(usize, u64)>>::new();
        let mut next_id = 0;
        let mut files = |count: usize, weight: u64| -> Vec<_> {
            (0..count)
                .map(|i| {
                    let id = next_id;
                    next_id += 1;
                    (id, weight + (i % 17) as u64)
                })
                .collect()
        };
        if mode == "deep" {
            let mut path = PathBuf::from("/root/ridge");
            for level in 0..20 {
                owned.insert(path.clone(), files(2, 5));
                path.push(format!("level{level:02}"));
            }
            owned.insert(path, files(700, 1000));
            owned.insert(PathBuf::from("/root/other"), files(160, 1000));
        } else if mode == "hierarchy" {
            // assembly and its intermediate branch have no direct files;
            // mixed has direct files adjacent to a raised child surface.
            owned.insert(
                PathBuf::from("/root/assembly/branch/alpha"),
                files(48, 1000),
            );
            owned.insert(PathBuf::from("/root/assembly/branch/beta"), files(30, 900));
            owned.insert(PathBuf::from("/root/assembly/side"), files(40, 950));
            owned.insert(PathBuf::from("/root/mixed"), files(50, 1000));
            owned.insert(PathBuf::from("/root/mixed/child"), files(60, 1000));
            owned.insert(PathBuf::from("/root/mixed/child/nested"), files(24, 1000));
        } else {
            owned.insert(PathBuf::from("/root/mixed"), files(320, 1000));
            owned.insert(PathBuf::from("/root/mixed/tiny"), files(3, 1));
            owned.insert(PathBuf::from("/root/other"), files(100, 1000));
        }
        let groups = owned
            .iter()
            .map(|(path, files)| (path.as_path(), files.clone()))
            .collect();
        let token = AtomicU64::new(1);
        layout(
            groups,
            Path::new("/root"),
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1200.0,
                h: 760.0,
            },
            1.0,
            &token,
            1,
        )
        .unwrap()
    }

    #[test]
    fn near_unary_geometry_budget_is_bounded_and_mixed_files_have_no_inner_rim() {
        for mode in ["deep", "mixed", "hierarchy"] {
            let layout = scene(mode);
            assert!(
                layout
                    .entries
                    .iter()
                    .all(|entry| entry.rect.w > 0.0 && entry.rect.h > 0.0)
            );
            let mut ids: Vec<_> = layout.entries.iter().map(|entry| entry.id).collect();
            ids.sort_unstable();
            assert!(ids.iter().enumerate().all(|(id, &actual)| id == actual));
            let main = layout
                .platforms
                .iter()
                .max_by(|a, b| (a.bounds.w * a.bounds.h).total_cmp(&(b.bounds.w * b.bounds.h)))
                .unwrap();
            if mode == "deep" {
                // Sum only the geometric margins of the main ancestor chain.
                // This excludes legitimate area reduction from file weights.
                let mut losses = [0.0f32; 4];
                for p in layout.platforms.iter().filter(|p| {
                    p.bounds.x >= main.bounds.x
                        && p.bounds.y >= main.bounds.y
                        && p.bounds.x + p.bounds.w <= main.bounds.x + main.bounds.w + 0.01
                        && p.bounds.y + p.bounds.h <= main.bounds.y + main.bounds.h + 0.01
                }) {
                    losses[0] += p.top.x - p.bounds.x;
                    losses[1] += p.top.y - p.bounds.y;
                    losses[2] += p.bounds.x + p.bounds.w - p.top.x - p.top.w;
                    losses[3] += p.bounds.y + p.bounds.h - p.top.y - p.top.h;
                }
                assert!(
                    losses.iter().all(|&loss| loss <= 11.51),
                    "per-edge losses={losses:?}"
                );
                assert!(
                    layout.platforms.len() < 20,
                    "subpixel layers must merge visually"
                );
                eprintln!(
                    "near-unary per-edge decoration={losses:?}, visible_platforms={}",
                    layout.platforms.len()
                );
            } else if mode == "mixed" {
                assert!(
                    layout
                        .entries
                        .iter()
                        .filter(|entry| entry.id < 320)
                        .any(|entry| (entry.rect.y - main.top.y).abs() < 0.001)
                );
                assert!(
                    layout
                        .entries
                        .iter()
                        .filter(|entry| entry.id < 320)
                        .any(|entry| (entry.rect.x - main.top.x).abs() < 0.001)
                );
            }
            let token = AtomicU64::new(1);
            let bounds = Rect {
                x: 0.0,
                y: 0.0,
                w: 1200.0,
                h: 760.0,
            };
            let map = crate::render::RenderMap::build_with_platforms(
                layout.entries,
                layout.platforms,
                bounds,
                [1200, 760],
                &token,
                1,
            )
            .unwrap();
            if let Ok(prefix) = std::env::var("COMPACT_PREVIEW_PREFIX") {
                let image = map.image.as_ref().unwrap();
                let mut file = std::fs::File::create(format!("{prefix}-{mode}.ppm")).unwrap();
                write!(file, "P6\n1200 760\n255\n").unwrap();
                for pixel in &image.pixels {
                    file.write_all(&pixel.to_array()[..3]).unwrap();
                }
            }
        }
    }
}
