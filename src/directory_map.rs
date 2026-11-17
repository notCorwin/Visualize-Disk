//! Directory nesting and shared cushion surfaces for the cached file map.
use crate::{
    render::{RenderEntry, Surface},
    treemap::{self, Rect},
};
use rayon::prelude::*;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Default)]
struct Directory {
    parent: usize,
    children: Vec<usize>,
    files: Vec<(usize, u64)>,
    weight: u64,
    color: eframe::egui::Color32,
}

pub fn layout(
    groups: BTreeMap<&Path, Vec<(usize, u64)>>,
    bounds: Rect,
    token: &AtomicU64,
    generation: u64,
) -> Option<(Vec<RenderEntry>, Vec<Surface>)> {
    let cancelled = || token.load(Ordering::Relaxed) != generation;
    // A synthetic root also handles snapshots containing unrelated roots.
    // Resolve ancestors only once per unique directory, never once per file.
    let mut directories = vec![Directory::default()];
    let mut indices: HashMap<PathBuf, usize> = HashMap::new();
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
                parent,
                color: crate::color(path),
                ..Default::default()
            });
            directories[parent].children.push(id);
            indices.insert(path.to_path_buf(), id);
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
    let mut surfaces = vec![Surface::default(); directories.len()];
    // Iteration tolerates deeply nested paths. Unary chains add no new visual
    // subdivision, so they do not accumulate identical cushions or consume depth.
    let mut pending = vec![(0, bounds, 0i32, Surface::default())];
    while !pending.is_empty() {
        if cancelled() {
            return None;
        }
        // Sibling directories share the global Rayon pool. Each task produces
        // only its own geometry; publishing a generation stays all-or-nothing.
        let pieces: Option<Vec<_>> = pending
            .into_par_iter()
            .map(|(id, bounds, depth, surface)| {
                if cancelled() {
                    return None;
                }
                let directory = &directories[id];
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
                let divided = items.len() > 1;
                let geometry = treemap::layout_with_cancel(&items, bounds, token, generation)?;
                let mut files = Vec::with_capacity(directory.files.len());
                let mut children = Vec::with_capacity(child_count);
                for (index, (slot, rect)) in geometry.into_iter().enumerate() {
                    if index % 4096 == 0 && cancelled() {
                        return None;
                    }
                    if slot < child_count {
                        let child = directory.children[slot];
                        let inherited = if divided {
                            surface.cushion(rect, 1.05 * 0.72f64.powi(depth))
                        } else {
                            surface
                        };
                        children.push((
                            child,
                            rect,
                            depth.saturating_add(i32::from(divided)),
                            inherited,
                        ));
                    } else {
                        files.push(RenderEntry {
                            id: directory.files[slot - child_count].0,
                            rect,
                            color: directory.color,
                            surface: id as u32,
                        });
                    }
                }
                Some((id, surface, files, children))
            })
            .collect();
        pending = Vec::new();
        for (id, surface, mut files, children) in pieces? {
            if cancelled() {
                return None;
            }
            surfaces[id] = surface;
            entries.append(&mut files);
            pending.extend(children);
        }
    }
    (!cancelled()).then_some((entries, surfaces))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Color32;

    fn bounds() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: 1200.0,
            h: 760.0,
        }
    }

    #[test]
    fn descendants_form_contiguous_weighted_regions_without_losing_files() {
        let token = AtomicU64::new(1);
        let groups = BTreeMap::from([
            (Path::new("/a"), vec![(0, 10)]),
            (Path::new("/a/nested"), vec![(1, 10), (2, 20)]),
            (Path::new("/b"), vec![(3, 40)]),
        ]);
        let (entries, surfaces) = layout(groups, bounds(), &token, 1).unwrap();
        assert_eq!(entries.len(), 4);
        let mut union = Rect {
            x: f32::MAX,
            y: f32::MAX,
            w: 0.0,
            h: 0.0,
        };
        let (mut right, mut bottom) = (0.0f32, 0.0f32);
        for entry in entries.iter().filter(|entry| entry.id < 3) {
            union.x = union.x.min(entry.rect.x);
            union.y = union.y.min(entry.rect.y);
            right = right.max(entry.rect.x + entry.rect.w);
            bottom = bottom.max(entry.rect.y + entry.rect.h);
        }
        union.w = right - union.x;
        union.h = bottom - union.y;
        let area = bounds().w * bounds().h;
        assert!((union.w * union.h - area * 0.5).abs() < 0.1);
        for entry in &entries {
            let weight = [10.0, 10.0, 20.0, 40.0][entry.id];
            assert!((entry.rect.w * entry.rect.h - area * weight / 80.0).abs() < 0.1);
        }
        let map = crate::render::RenderMap::build_with_surfaces(
            entries,
            surfaces,
            bounds(),
            [1200, 760],
            &token,
            1,
        )
        .unwrap();
        for y in 0..760 {
            for x in 0..1200 {
                assert!(map.hit_test(x as f32 + 0.5, y as f32 + 0.5).is_some());
            }
        }
        token.store(2, Ordering::Relaxed);
        assert!(layout(BTreeMap::new(), bounds(), &token, 1).is_none());
    }

    #[test]
    fn unary_ancestors_do_not_change_shading_or_geometry() {
        let token = AtomicU64::new(1);
        let build = |a, b| {
            let groups = BTreeMap::from([(a, vec![(0, 10), (1, 20)]), (b, vec![(2, 30)])]);
            let (mut entries, surfaces) = layout(groups, bounds(), &token, 1).unwrap();
            entries
                .iter_mut()
                .for_each(|entry| entry.color = Color32::from_rgb(65, 143, 151));
            crate::render::RenderMap::build_with_surfaces(
                entries,
                surfaces,
                bounds(),
                [1200, 760],
                &token,
                1,
            )
            .unwrap()
        };
        let shallow = build(Path::new("/a"), Path::new("/b"));
        let deep = build(Path::new("/root/one/two/a"), Path::new("/root/one/two/b"));
        assert_eq!(shallow.image.unwrap().pixels, deep.image.unwrap().pixels);
    }

    #[test]
    fn nested_cushion_preview_and_crop_share_world_lighting() {
        let token = AtomicU64::new(1);
        let paths = [
            "/Applications/Design",
            "/Applications/Development",
            "/Applications/Utilities",
            "/Users/Pictures/Trips",
            "/Users/Pictures/Family",
            "/Users/Documents",
            "/Users/Library/Caches",
            "/System",
            "/System/Library",
        ];
        let mut groups = BTreeMap::new();
        let mut id = 0;
        for (group, path) in paths.iter().enumerate() {
            let files = (0..(12 + group * 5))
                .map(|file| {
                    let item = (
                        id,
                        ((file * 7 + group * 13) % 23 + 2) as u64 * (10 - group) as u64,
                    );
                    id += 1;
                    item
                })
                .collect();
            groups.insert(Path::new(path), files);
        }
        let (entries, surfaces) = layout(groups, bounds(), &token, 1).unwrap();
        let map = crate::render::RenderMap::build_with_surfaces(
            entries,
            surfaces,
            bounds(),
            [1200, 760],
            &token,
            1,
        )
        .unwrap();
        let detail = map
            .rasterize(
                Rect {
                    x: 140.0,
                    y: 120.0,
                    w: 900.0,
                    h: 520.0,
                },
                [900, 520],
                &token,
                1,
            )
            .unwrap();
        let image = map.image.as_ref().unwrap();
        for y in 0..520 {
            for x in 0..900 {
                let a = image.pixels[(y + 120) * 1200 + x + 140];
                let b = detail.image.as_ref().unwrap().pixels[y * 900 + x];
                for (a, b) in a.to_array().into_iter().zip(b.to_array()) {
                    assert!((a as i16 - b as i16).abs() <= 1);
                }
            }
        }
        if let Ok(path) = std::env::var("CUSHION_PREVIEW_PATH") {
            use std::io::Write;
            let mut file = std::fs::File::create(path).unwrap();
            write!(file, "P6\n1200 760\n255\n").unwrap();
            for pixel in &image.pixels {
                file.write_all(&pixel.to_array()[..3]).unwrap();
            }
        }
    }
}
