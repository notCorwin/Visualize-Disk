use super::*;
use crate::{directory_map, scene_model::TileId};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

fn id(n: u64) -> scan::FileIdentity {
    scan::FileIdentity {
        device: n,
        ..Default::default()
    }
}
fn fact(path: &str, n: u64, children: bool) -> scan::DirectoryFact {
    scan::DirectoryFact {
        path: path.into(),
        identity: id(n),
        enumeration_complete: true,
        excluded: false,
        has_subdirectories: children,
    }
}
fn file(path: &str, parent: &str, n: u64, parent_id: u64, size: u64) -> scan::FileEntry {
    scan::FileEntry {
        path: path.into(),
        parent: Arc::from(Path::new(parent)),
        size,
        identity: id(n),
        parent_identity: id(parent_id),
        parent_is_leaf: true,
        can_trash: true,
    }
}
fn receive(coordinator: &Coordinator) -> Event {
    coordinator
        .rx
        .recv_timeout(Duration::from_secs(10))
        .expect("coordinator response")
}

fn check_cross_root_move(extra_visible_change: bool) {
    let source = Arc::new(scan::ScanResult {
        root: "/disk".into(),
        files: vec![
            file("/disk/a/f", "/disk/a", 11, 2, 40),
            file("/disk/b/g", "/disk/b", 12, 3, 40),
            file("/disk/c/h", "/disk/c", 13, 4, 40),
            file("/disk/d/i", "/disk/d", 14, 5, 100),
        ],
        directories: vec![
            fact("/disk", 1, true),
            fact("/disk/a", 2, false),
            fact("/disk/b", 3, false),
            fact("/disk/c", 4, false),
            fact("/disk/d", 5, false),
        ]
        .into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 600.,
        h: 400.,
    };
    let token = AtomicU64::new(1);
    let layout = directory_map::layout_with_partitions(
        [
            (Path::new("/disk/a"), vec![(0, 40)]),
            (Path::new("/disk/b"), vec![(1, 40)]),
            (Path::new("/disk/c"), vec![(2, 40)]),
            (Path::new("/disk/d"), vec![(3, 100)]),
        ],
        Path::new("/disk"),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [600, 400],
        &token,
        1,
    )
    .unwrap();
    let old_image = map.image.as_ref().expect("initial image").clone();
    let old = map.snapshot_for_update();
    let stable = old.rect(3).unwrap();
    let stable_c = old.rect(2).unwrap();
    let old_move = old.rect(0).unwrap();
    let stable_centers = [
        (stable.x + stable.w / 2., stable.y + stable.h / 2.),
        (stable_c.x + stable_c.w / 2., stable_c.y + stable_c.h / 2.),
    ];
    let coordinator = Coordinator::start(source.clone(), 91, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(91);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Files,
            data: source.clone(),
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::new(BTreeSet::new()),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    let Event::ModeReady(initial) = receive(&coordinator) else {
        panic!("mode ready")
    };
    let moved_id = initial.projection.id(0).unwrap();
    assert!(matches!(moved_id, TileId::File(_)));
    let moved = file("/disk/b/moved", "/disk/b", 11, 3, 40);
    let mut changes = vec![
        Change::MoveFile {
            from: "/disk/a/f".into(),
            identity: id(11),
            to: moved,
        },
        Change::UpsertDirectory(fact("/disk/c", 4, true)),
        Change::UpsertDirectory(fact("/disk/c/empty", 6, false)),
    ];
    if extra_visible_change {
        changes.push(Change::UpsertFile(file(
            "/disk/c/new",
            "/disk/c",
            15,
            4,
            25,
        )));
    }
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes: vec![ModeBase {
                mode: View::Files,
                layout_generation: 1,
                texture,
                map,
                holes: Arc::new(vec![]),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 4096,
            }],
        }))
        .ok()
        .unwrap();
    let Event::Prepared(result) = receive(&coordinator) else {
        panic!("prepared")
    };
    assert!(result.rebuild.is_empty());
    let view = &result.views[0];
    assert_eq!(view.texture, texture);
    assert_eq!(view.map.rect(3), Some(stable), "unrelated root child moved");
    if !extra_visible_change {
        assert_eq!(
            view.map.rect(2),
            Some(stable_c),
            "unprojected directory fact moved its existing file"
        );
    } else {
        assert!(
            (0..view.projection.len())
                .filter_map(|slot| view.projection.get(slot))
                .any(|entry| entry.path.as_path() == Path::new("/disk/c/new"))
        );
    }
    assert_eq!(
        old.hit_test(old_move.x + old_move.w / 2., old_move.y + old_move.h / 2.),
        Some(0)
    );
    let moved_slot = (0..view.projection.len())
        .find(|&slot| view.projection.id(slot) == Some(moved_id))
        .expect("same moved identity");
    assert_eq!(
        view.projection.get(moved_slot).unwrap().path.as_path(),
        Path::new("/disk/b/moved")
    );
    let new_rect = view.map.rect(moved_slot).expect("new geometry");
    assert_eq!(
        view.map
            .hit_test(new_rect.x + new_rect.w / 2., new_rect.y + new_rect.h / 2.),
        Some(moved_slot)
    );
    let dirty_pixels: usize = view
        .patches
        .iter()
        .map(|p| p.image.size[0] * p.image.size[1])
        .sum();
    assert!(
        dirty_pixels < 600 * 400,
        "all pixels unnecessarily dirtied: {dirty_pixels}"
    );
    for patch in &view.patches {
        for (i, (x, y)) in stable_centers.into_iter().enumerate() {
            if extra_visible_change && i == 1 {
                continue;
            }
            let in_patch = x >= patch.origin[0] as f32
                && x < (patch.origin[0] + patch.image.size[0]) as f32
                && y >= patch.origin[1] as f32
                && y < (patch.origin[1] + patch.image.size[1]) as f32;
            if in_patch {
                let x = x as usize;
                let y = y as usize;
                let patch_offset =
                    (y - patch.origin[1]) * patch.image.size[0] + x - patch.origin[0];
                assert_eq!(
                    patch.image.pixels[patch_offset],
                    old_image.pixels[y * 600 + x]
                );
            }
            assert_eq!(view.map.hit_test(x, y), old.hit_test(x, y));
        }
        for y in 0..patch.image.size[1] {
            for x in 0..patch.image.size[0] {
                let gx = patch.origin[0] + x;
                let gy = patch.origin[1] + y;
                let pixel_x = gx as f32 + 0.5;
                let pixel_y = gy as f32 + 0.5;
                assert_eq!(patch.owner_at(x, y), view.map.hit_test(pixel_x, pixel_y));
                if pixel_x > stable.x + 10.
                    && pixel_x < stable.x + stable.w - 10.
                    && pixel_y > stable.y + 10.
                    && pixel_y < stable.y + stable.h - 10.
                    && old.hit_test(pixel_x, pixel_y) == Some(3)
                {
                    assert_eq!(patch.owner_at(x, y), Some(3));
                    assert_eq!(
                        patch.image.pixels[y * patch.image.size[0] + x],
                        old_image.pixels[gy * 600 + gx]
                    );
                }
            }
        }
    }
}

fn check_frame_against_reference(
    view: &ModeUpdate,
    pixels: &mut Vec<eframe::egui::Color32>,
    size: [usize; 2],
) {
    for patch in &view.patches {
        for y in 0..patch.image.size[1] {
            for x in 0..patch.image.size[0] {
                let gx = patch.origin[0] + x;
                let gy = patch.origin[1] + y;
                pixels[gy * size[0] + gx] = patch.image.pixels[y * patch.image.size[0] + x];
            }
        }
    }
    let bounds = view.map.bounds;
    let token = AtomicU64::new(1);
    let entries: Vec<_> = view
        .map
        .entries()
        .iter()
        .copied()
        .filter(|entry| view.map.rect(entry.id).is_some())
        .collect();
    let reference = crate::render::raster_patch(
        &entries,
        view.map.platforms(),
        bounds,
        size,
        [0, 0, size[0], size[1]],
        &token,
        1,
    )
    .unwrap();
    assert!(
        view.holes.is_empty(),
        "this scenario should publish all deletion masks into patches"
    );
    for y in 0..size[1] {
        for x in 0..size[0] {
            let px = bounds.x + (x as f32 + 0.5) * bounds.w / size[0] as f32;
            let py = bounds.y + (y as f32 + 0.5) * bounds.h / size[1] as f32;
            assert_eq!(view.map.hit_test(px, py), reference.owner_at(x, y));
        }
    }
    assert_eq!(*pixels, reference.image.pixels);
}

#[test]
fn cross_root_move_with_unprojected_empty_directory_is_atomic_and_keeps_unrelated_geometry() {
    check_cross_root_move(false);
}

#[test]
fn cross_root_move_with_another_subtree_change_is_atomic_and_keeps_unrelated_geometry() {
    check_cross_root_move(true);
}

fn check_hybrid_root_leaf_expands_and_collapses(root: &str) {
    let root_path = Path::new(root);
    let file_path = root_path.join("f");
    let empty_path = root_path.join("empty");
    let file_path = file_path.to_str().unwrap();
    let empty_path = empty_path.to_str().unwrap();
    let before_folds = crate::scan_root_fold::PREPARED_ROOT_FOLDS.load(Ordering::Relaxed);
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![file(file_path, root, 2, 1, 100)],
        directories: vec![fact(root, 1, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let projected = Arc::new(scan::ScanResult {
        root: source.root.clone(),
        files: vec![scan::FileEntry {
            can_trash: false,
            parent_is_leaf: false,
            ..file(root, "/", 1, 0, 100)
        }],
        directories: source.directories.clone(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 100.,
        h: 100.,
    };
    let token = AtomicU64::new(1);
    let layout = directory_map::layout_with_partitions(
        [(Path::new("/"), vec![(0, 100)])],
        root_path,
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [100, 100],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let coordinator = Coordinator::start(source, 92, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(92);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Hybrid,
            data: projected,
            source_ids: Some(Arc::from([None])),
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::new(BTreeSet::new()),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
    let update = |base, changes, map, holes| {
        Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes: vec![ModeBase {
                mode: View::Hybrid,
                layout_generation: 1,
                texture,
                map,
                holes,
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 256,
            }],
        })
    };
    coordinator
        .try_send(update(
            base,
            vec![
                Change::UpsertDirectory(fact(root, 1, true)),
                Change::UpsertDirectory(fact(empty_path, 3, false)),
            ],
            map,
            Arc::new(vec![]),
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut expanded) = receive(&coordinator) else {
        panic!("expand")
    };
    assert!(expanded.rebuild.is_empty());
    let view = expanded.views.pop().expect("expanded view");
    check_frame_against_reference(&view, &mut pixels, [100, 100]);
    let paths: Vec<_> = (0..view.projection.len())
        .filter_map(|i| view.projection.get(i))
        .map(|entry| entry.path.to_path_buf())
        .collect();
    assert_eq!(paths, vec![PathBuf::from(file_path)]);
    assert!(
        crate::scan_root_fold::PREPARED_ROOT_FOLDS.load(Ordering::Relaxed) > before_folds,
        "expansion must use scan-root local fold"
    );
    let expanded_slot = (0..view.projection.len())
        .find(|&i| view.projection.get(i).is_some())
        .unwrap();
    assert_eq!(view.map.hit_test(50., 50.), Some(expanded_slot));
    let after_expansion = crate::scan_root_fold::PREPARED_ROOT_FOLDS.load(Ordering::Relaxed);
    coordinator
        .try_send(Command::Commit(expanded.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(update(
            expanded.version,
            vec![
                Change::RemoveDirectory {
                    path: empty_path.into(),
                    identity: id(3),
                },
                Change::UpsertDirectory(fact(root, 1, false)),
            ],
            view.map,
            view.holes,
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut collapsed) = receive(&coordinator) else {
        panic!("collapse")
    };
    assert!(collapsed.rebuild.is_empty());
    let view = collapsed.views.pop().expect("collapsed view");
    check_frame_against_reference(&view, &mut pixels, [100, 100]);
    let paths: Vec<_> = (0..view.projection.len())
        .filter_map(|i| view.projection.get(i))
        .map(|entry| entry.path.to_path_buf())
        .collect();
    assert_eq!(paths, vec![PathBuf::from(root)]);
    assert_eq!(view.texture, texture);
    assert!(
        crate::scan_root_fold::PREPARED_ROOT_FOLDS.load(Ordering::Relaxed) > after_expansion,
        "collapse must use scan-root local fold"
    );
    let collapsed_slot = (0..view.projection.len())
        .find(|&i| view.projection.get(i).is_some())
        .unwrap();
    assert_eq!(view.map.hit_test(50., 50.), Some(collapsed_slot));
}

#[test]
fn hybrid_root_leaf_expands_and_collapses_for_empty_child() {
    check_hybrid_root_leaf_expands_and_collapses("/disk");
    check_hybrid_root_leaf_expands_and_collapses("/");
}

#[test]
fn distant_file_updates_preserve_full_frame_pixels_without_full_screen_dirt() {
    let files: Vec<_> = ['a', 'b', 'c', 'd']
        .iter()
        .enumerate()
        .flat_map(|(i, name)| {
            let parent = format!("/disk/{name}");
            let parent_id = 2 + i as u64;
            [0, 1].into_iter().map(move |j| {
                file(
                    &format!("{parent}/file{j}"),
                    &parent,
                    10 + 2 * i as u64 + j,
                    parent_id,
                    100,
                )
            })
        })
        .collect();
    let source = Arc::new(scan::ScanResult {
        root: "/disk".into(),
        files,
        directories: vec![
            fact("/disk", 1, true),
            fact("/disk/a", 2, false),
            fact("/disk/b", 3, false),
            fact("/disk/c", 4, false),
            fact("/disk/d", 5, false),
        ]
        .into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 640.,
        h: 400.,
    };
    let token = AtomicU64::new(1);
    let layout = directory_map::layout_with_partitions(
        [
            (Path::new("/disk/a"), vec![(0, 100), (1, 100)]),
            (Path::new("/disk/b"), vec![(2, 100), (3, 100)]),
            (Path::new("/disk/c"), vec![(4, 100), (5, 100)]),
            (Path::new("/disk/d"), vec![(6, 100), (7, 100)]),
        ],
        Path::new("/disk"),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [640, 400],
        &token,
        1,
    )
    .unwrap();
    let mut actual_pixels = map.image.as_ref().unwrap().pixels.clone();
    let mut choices = None;
    for a in [0, 2, 4, 6] {
        for b in [0, 2, 4, 6] {
            let (ra, rb) = (map.rect(a).unwrap(), map.rect(b).unwrap());
            let distance = (ra.x + ra.w / 2. - rb.x - rb.w / 2.).powi(2)
                + (ra.y + ra.h / 2. - rb.y - rb.h / 2.).powi(2);
            if choices.is_none_or(|(_, _, best)| distance > best) {
                choices = Some((a, b, distance));
            }
        }
    }
    let (a, b, _) = choices.unwrap();
    assert_ne!(a, b);
    let coordinator = Coordinator::start(source.clone(), 93, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(93);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Files,
            data: source.clone(),
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::new(BTreeSet::new()),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
    let mut first = source.files[a].clone();
    let mut second = source.files[b].clone();
    first.size = 110;
    second.size = 115;
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(vec![Change::UpsertFile(first), Change::UpsertFile(second)]),
            modes: vec![ModeBase {
                mode: View::Files,
                layout_generation: 1,
                texture,
                map,
                holes: Arc::new(vec![]),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 4096,
            }],
        }))
        .ok()
        .unwrap();
    let Event::Prepared(result) = receive(&coordinator) else {
        panic!("prepared")
    };
    assert!(result.rebuild.is_empty());
    let view = &result.views[0];
    let dirty: usize = view
        .patches
        .iter()
        .map(|p| p.image.size[0] * p.image.size[1])
        .sum();
    assert!(
        dirty < 640 * 400,
        "distant file updates marked entire frame dirty: {dirty}"
    );
    let entries: Vec<_> = view
        .map
        .entries()
        .iter()
        .copied()
        .filter(|entry| view.map.rect(entry.id).is_some())
        .collect();
    let expected = crate::render::raster_patch(
        &entries,
        view.map.platforms(),
        bounds,
        [640, 400],
        [0, 0, 640, 400],
        &token,
        1,
    )
    .unwrap();
    for patch in &view.patches {
        for y in 0..patch.image.size[1] {
            for x in 0..patch.image.size[0] {
                let gx = patch.origin[0] + x;
                let gy = patch.origin[1] + y;
                let at = gy * 640 + gx;
                actual_pixels[at] = patch.image.pixels[y * patch.image.size[0] + x];
                assert_eq!(patch.owner_at(x, y), expected.owner_at(gx, gy));
            }
        }
    }
    assert_eq!(
        actual_pixels, expected.image.pixels,
        "dirty and retained colors differ from reference"
    );
    for y in 0..400 {
        for x in 0..640 {
            assert_eq!(
                view.map.hit_test(x as f32 + 0.5, y as f32 + 0.5),
                expected.owner_at(x, y)
            );
        }
    }
}

#[test]
fn simultaneous_opposite_and_new_destination_moves_keep_other_root_branch_stable() {
    let source = Arc::new(scan::ScanResult {
        root: "/disk".into(),
        files: vec![
            file("/disk/a/f", "/disk/a", 11, 2, 40),
            file("/disk/b/g", "/disk/b", 12, 3, 40),
            file("/disk/c/h", "/disk/c", 13, 4, 40),
            file("/disk/d/i", "/disk/d", 14, 5, 200),
        ],
        directories: vec![
            fact("/disk", 1, true),
            fact("/disk/a", 2, false),
            fact("/disk/b", 3, false),
            fact("/disk/c", 4, false),
            fact("/disk/d", 5, false),
        ]
        .into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 600.,
        h: 400.,
    };
    let token = AtomicU64::new(1);
    let layout = directory_map::layout_with_partitions(
        [
            (Path::new("/disk/a"), vec![(0, 40)]),
            (Path::new("/disk/b"), vec![(1, 40)]),
            (Path::new("/disk/c"), vec![(2, 40)]),
            (Path::new("/disk/d"), vec![(3, 200)]),
        ],
        Path::new("/disk"),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [600, 400],
        &token,
        1,
    )
    .unwrap();
    let old_image = map.image.as_ref().unwrap().pixels.clone();
    let stable = map.rect(3);
    let coordinator = Coordinator::start(source.clone(), 94, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(94);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Files,
            data: source.clone(),
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::new(BTreeSet::new()),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    let Event::ModeReady(initial) = receive(&coordinator) else {
        panic!("mode ready")
    };
    let ids: Vec<_> = (0..3).map(|i| initial.projection.id(i).unwrap()).collect();
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(vec![
                Change::MoveFile {
                    from: "/disk/a/f".into(),
                    identity: id(11),
                    to: file("/disk/b/f", "/disk/b", 11, 3, 40),
                },
                Change::MoveFile {
                    from: "/disk/b/g".into(),
                    identity: id(12),
                    to: file("/disk/a/g", "/disk/a", 12, 2, 40),
                },
                Change::UpsertDirectory(fact("/disk/e", 6, false)),
                Change::MoveFile {
                    from: "/disk/c/h".into(),
                    identity: id(13),
                    to: file("/disk/e/h", "/disk/e", 13, 6, 40),
                },
            ]),
            modes: vec![ModeBase {
                mode: View::Files,
                layout_generation: 1,
                texture,
                map,
                holes: Arc::new(vec![]),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 4096,
            }],
        }))
        .ok()
        .unwrap();
    let Event::Prepared(result) = receive(&coordinator) else {
        panic!("prepared")
    };
    assert!(result.rebuild.is_empty());
    let view = &result.views[0];
    assert_eq!(view.map.rect(3), stable);
    for (tile_id, expected_path) in ids.into_iter().zip(["/disk/b/f", "/disk/a/g", "/disk/e/h"]) {
        let slot = (0..view.projection.len())
            .find(|&i| view.projection.id(i) == Some(tile_id))
            .unwrap();
        assert_eq!(
            view.projection.get(slot).unwrap().path.as_path(),
            Path::new(expected_path)
        );
    }
    let entries: Vec<_> = view
        .map
        .entries()
        .iter()
        .copied()
        .filter(|entry| view.map.rect(entry.id).is_some())
        .collect();
    let expected = crate::render::raster_patch(
        &entries,
        view.map.platforms(),
        bounds,
        [600, 400],
        [0, 0, 600, 400],
        &token,
        1,
    )
    .unwrap();
    let mut actual = old_image;
    for patch in &view.patches {
        for y in 0..patch.image.size[1] {
            for x in 0..patch.image.size[0] {
                let gx = patch.origin[0] + x;
                let gy = patch.origin[1] + y;
                actual[gy * 600 + gx] = patch.image.pixels[y * patch.image.size[0] + x];
                assert_eq!(patch.owner_at(x, y), expected.owner_at(gx, gy));
            }
        }
    }
    assert_eq!(actual, expected.image.pixels);
    for y in 0..400 {
        for x in 0..600 {
            assert_eq!(
                view.map.hit_test(x as f32 + 0.5, y as f32 + 0.5),
                expected.owner_at(x, y)
            );
        }
    }
}

fn check_empty_root_add_remove_readd(root: &str) {
    let path = Path::new(root).join("f");
    let path = path.to_str().unwrap();
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![],
        directories: vec![fact(root, 1, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 80.,
        h: 60.,
    };
    let layout = directory_map::layout_with_partitions(
        std::iter::empty::<(&Path, Vec<(usize, u64)>)>(),
        Path::new(root),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [80, 60],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let coordinator = Coordinator::start(source.clone(), 95, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(95);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Files,
            data: source,
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::default(),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
    let update = |base, changes, map, holes| {
        Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes: vec![ModeBase {
                mode: View::Files,
                layout_generation: 1,
                texture,
                map,
                holes,
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 256,
            }],
        })
    };
    let count = crate::scan_root_fold::PREPARED_EMPTY_ROOTS.load(Ordering::Relaxed);
    coordinator
        .try_send(update(
            base,
            vec![Change::UpsertFile(file(path, root, 2, 1, 50))],
            map,
            Arc::new(vec![]),
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut added) = receive(&coordinator) else {
        panic!("added")
    };
    assert!(added.rebuild.is_empty());
    let view = added.views.pop().expect("added view");
    assert!(crate::scan_root_fold::PREPARED_EMPTY_ROOTS.load(Ordering::Relaxed) > count);
    let first_id = view.projection.id(0).unwrap();
    check_frame_against_reference(&view, &mut pixels, [80, 60]);
    assert_eq!(view.map.hit_test(40., 30.), Some(0));
    coordinator
        .try_send(Command::Commit(added.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(update(
            added.version,
            vec![Change::RemoveFile {
                path: path.into(),
                identity: id(2),
            }],
            view.map,
            view.holes,
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut removed) = receive(&coordinator) else {
        panic!("removed")
    };
    assert!(removed.rebuild.is_empty());
    let view = removed.views.pop().expect("removed view");
    assert!(view.projection.get(0).is_none());
    check_frame_against_reference(&view, &mut pixels, [80, 60]);
    assert_eq!(view.map.hit_test(40., 30.), None);
    coordinator
        .try_send(Command::Commit(removed.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(update(
            removed.version,
            vec![Change::UpsertFile(file(path, root, 3, 1, 75))],
            view.map,
            view.holes,
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut readded) = receive(&coordinator) else {
        panic!("readded")
    };
    assert!(readded.rebuild.is_empty());
    let view = readded.views.pop().expect("readded view");
    assert!(
        view.projection.get(0).is_none(),
        "old tombstone was resurrected"
    );
    let slot = (0..view.projection.len())
        .find(|&i| view.projection.get(i).is_some())
        .unwrap();
    assert_ne!(view.projection.id(slot), Some(first_id));
    assert_eq!(
        view.projection.get(slot).unwrap().path.as_path(),
        Path::new(path)
    );
    check_frame_against_reference(&view, &mut pixels, [80, 60]);
    assert_eq!(view.map.hit_test(40., 30.), Some(slot));
    assert_eq!(view.texture, texture);
}

#[test]
fn empty_scene_add_remove_and_readd_preserves_tombstone_colors_and_owners() {
    check_empty_root_add_remove_readd("/disk");
    check_empty_root_add_remove_readd("/");
}

#[test]
fn root_files_and_subtrees_move_add_and_delete_in_one_atomic_transaction() {
    let source = Arc::new(scan::ScanResult {
        root: "/disk".into(),
        files: vec![
            scan::FileEntry {
                parent_is_leaf: false,
                ..file("/disk/to_a", "/disk", 11, 1, 30)
            },
            scan::FileEntry {
                parent_is_leaf: false,
                ..file("/disk/remove", "/disk", 12, 1, 25)
            },
            file("/disk/a/to_root", "/disk/a", 13, 2, 30),
            file("/disk/b/to_c", "/disk/b", 14, 3, 30),
            file("/disk/c/stay", "/disk/c", 15, 4, 30),
            file("/disk/d/stay", "/disk/d", 16, 5, 200),
        ],
        directories: vec![
            fact("/disk", 1, true),
            fact("/disk/a", 2, false),
            fact("/disk/b", 3, false),
            fact("/disk/c", 4, false),
            fact("/disk/d", 5, false),
        ]
        .into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 600.,
        h: 400.,
    };
    let token = AtomicU64::new(1);
    let layout = directory_map::layout_with_partitions(
        [
            (Path::new("/disk"), vec![(0, 30), (1, 25)]),
            (Path::new("/disk/a"), vec![(2, 30)]),
            (Path::new("/disk/b"), vec![(3, 30)]),
            (Path::new("/disk/c"), vec![(4, 30)]),
            (Path::new("/disk/d"), vec![(5, 200)]),
        ],
        Path::new("/disk"),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [600, 400],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let stable = map.rect(5);
    let coordinator = Coordinator::start(source.clone(), 96, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(96);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Files,
            data: source.clone(),
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::new(BTreeSet::new()),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    let Event::ModeReady(initial) = receive(&coordinator) else {
        panic!("mode ready")
    };
    let moved_ids: Vec<_> = [0, 2, 3]
        .into_iter()
        .map(|i| initial.projection.id(i).unwrap())
        .collect();
    let removed_id = initial.projection.id(1).unwrap();
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(vec![
                Change::MoveFile {
                    from: "/disk/to_a".into(),
                    identity: id(11),
                    to: file("/disk/a/from_root", "/disk/a", 11, 2, 30),
                },
                Change::MoveFile {
                    from: "/disk/a/to_root".into(),
                    identity: id(13),
                    to: file("/disk/from_a", "/disk", 13, 1, 30),
                },
                Change::MoveFile {
                    from: "/disk/b/to_c".into(),
                    identity: id(14),
                    to: file("/disk/c/from_b", "/disk/c", 14, 4, 30),
                },
                Change::RemoveFile {
                    path: "/disk/remove".into(),
                    identity: id(12),
                },
                Change::UpsertFile(file("/disk/new", "/disk", 17, 1, 25)),
            ]),
            modes: vec![ModeBase {
                mode: View::Files,
                layout_generation: 1,
                texture,
                map,
                holes: Arc::new(vec![]),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 4096,
            }],
        }))
        .ok()
        .unwrap();
    let Event::Prepared(result) = receive(&coordinator) else {
        panic!("prepared")
    };
    assert!(result.rebuild.is_empty());
    let view = &result.views[0];
    assert_eq!(view.map.rect(5), stable, "unrelated branch moved");
    for (tile_id, path) in
        moved_ids
            .into_iter()
            .zip(["/disk/a/from_root", "/disk/from_a", "/disk/c/from_b"])
    {
        let slot = (0..view.projection.len())
            .find(|&i| view.projection.id(i) == Some(tile_id))
            .unwrap();
        assert_eq!(
            view.projection.get(slot).unwrap().path.as_path(),
            Path::new(path)
        );
    }
    assert!((0..view.projection.len()).all(|i| view.projection.id(i) != Some(removed_id)));
    assert!(
        (0..view.projection.len())
            .filter_map(|i| view.projection.get(i))
            .any(|entry| entry.path == Path::new("/disk/new"))
    );
    check_frame_against_reference(view, &mut pixels, [600, 400]);
    assert_eq!(view.texture, texture);
}

#[test]
fn folders_mode_root_aggregate_gains_first_subtree_without_root_rebuild() {
    let root = "/coordinator-folder-root-only";
    let sub = "/coordinator-folder-root-only/sub";
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![file("/coordinator-folder-root-only/f", root, 2, 1, 100)],
        directories: vec![fact(root, 1, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let projected = Arc::new(scan::ScanResult {
        root: source.root.clone(),
        files: vec![scan::FileEntry {
            can_trash: false,
            parent_is_leaf: false,
            ..file(root, "/", 1, 0, 100)
        }],
        directories: source.directories.clone(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 200.,
        h: 120.,
    };
    let layout = directory_map::layout_with_partitions(
        [(Path::new("/"), vec![(0, 100)])],
        Path::new(root),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [200, 120],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let coordinator = Coordinator::start(source, 97, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(97);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Folders,
            data: projected,
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::default(),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    let Event::ModeReady(ready) = receive(&coordinator) else {
        panic!("mode ready")
    };
    assert!(
        ready.geometry.scan_root_frame(Path::new(root)).is_none(),
        "initial projection must lack root content frame"
    );
    let fallbacks_before = crate::incremental_update::ROOT_FALLBACKS
        .lock()
        .unwrap()
        .get(Path::new(root))
        .copied()
        .unwrap_or(0);
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(vec![
                Change::UpsertDirectory(fact(root, 1, true)),
                Change::UpsertDirectory(fact(sub, 3, false)),
                Change::UpsertFile(file("/coordinator-folder-root-only/sub/f", sub, 4, 3, 60)),
            ]),
            modes: vec![ModeBase {
                mode: View::Folders,
                layout_generation: 1,
                texture,
                map,
                holes: Arc::new(vec![]),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 512,
            }],
        }))
        .ok()
        .unwrap();
    let Event::Prepared(mut result) = receive(&coordinator) else {
        panic!("prepared")
    };
    assert!(result.rebuild.is_empty());
    let fallbacks_after = crate::incremental_update::ROOT_FALLBACKS
        .lock()
        .unwrap()
        .get(Path::new(root))
        .copied()
        .unwrap_or(0);
    assert_eq!(
        fallbacks_after, fallbacks_before,
        "unexpected full-root layout fallback"
    );
    let view = result.views.pop().unwrap();
    let paths: BTreeSet<_> = (0..view.projection.len())
        .filter_map(|i| view.projection.get(i))
        .map(|entry| entry.path.to_path_buf())
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from([PathBuf::from(root), PathBuf::from(sub)])
    );
    assert_eq!(view.texture, texture);
    check_frame_against_reference(&view, &mut pixels, [200, 120]);
}

#[test]
fn deep_directory_creation_updates_only_affected_branch() {
    let root = "/deep-scope-coordinator";
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![
            file(
                "/deep-scope-coordinator/Users/alice/f",
                "/deep-scope-coordinator/Users/alice",
                11,
                3,
                100,
            ),
            file(
                "/deep-scope-coordinator/Users/other/g",
                "/deep-scope-coordinator/Users/other",
                12,
                4,
                100,
            ),
            file(
                "/deep-scope-coordinator/else/h",
                "/deep-scope-coordinator/else",
                13,
                5,
                200,
            ),
        ],
        directories: vec![
            fact(root, 1, true),
            fact("/deep-scope-coordinator/Users", 2, true),
            fact("/deep-scope-coordinator/Users/alice", 3, false),
            fact("/deep-scope-coordinator/Users/other", 4, false),
            fact("/deep-scope-coordinator/else", 5, false),
        ]
        .into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 600.,
        h: 400.,
    };
    let layout = directory_map::layout_with_partitions(
        [
            (
                Path::new("/deep-scope-coordinator/Users/alice"),
                vec![(0, 100)],
            ),
            (
                Path::new("/deep-scope-coordinator/Users/other"),
                vec![(1, 100)],
            ),
            (Path::new("/deep-scope-coordinator/else"), vec![(2, 200)]),
        ],
        Path::new(root),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [600, 400],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let unchanged = [map.rect(1), map.rect(2)];
    let coordinator = Coordinator::start(source.clone(), 98, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(98);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Files,
            data: source,
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::default(),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
    let scope = Path::new("/deep-scope-coordinator/Users/alice");
    let scope_count = crate::deep_scope::hit_count(scope);
    let fallbacks = crate::incremental_update::ROOT_FALLBACKS
        .lock()
        .unwrap()
        .get(Path::new(root))
        .copied()
        .unwrap_or(0);
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(vec![
                Change::UpsertDirectory(fact("/deep-scope-coordinator/Users/alice", 3, true)),
                Change::UpsertDirectory(fact("/deep-scope-coordinator/Users/alice/sub", 6, false)),
                Change::UpsertFile(file(
                    "/deep-scope-coordinator/Users/alice/sub/new",
                    "/deep-scope-coordinator/Users/alice/sub",
                    14,
                    6,
                    35,
                )),
            ]),
            modes: vec![ModeBase {
                mode: View::Files,
                layout_generation: 1,
                texture,
                map,
                holes: Arc::new(vec![]),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 4096,
            }],
        }))
        .ok()
        .unwrap();
    let Event::Prepared(mut prepared) = receive(&coordinator) else {
        panic!("prepared")
    };
    assert!(prepared.rebuild.is_empty());
    assert_eq!(
        crate::incremental_update::ROOT_FALLBACKS
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0),
        fallbacks
    );
    assert!(
        crate::deep_scope::hit_count(scope) > scope_count,
        "must use alice scoped update"
    );
    let view = prepared.views.pop().unwrap();
    assert_eq!([view.map.rect(1), view.map.rect(2)], unchanged);
    assert!(
        view.patches
            .iter()
            .map(|patch| patch.image.size[0] * patch.image.size[1])
            .sum::<usize>()
            < 600 * 400
    );
    assert!(
        (0..view.projection.len())
            .filter_map(|i| view.projection.get(i))
            .any(|entry| entry.path == Path::new("/deep-scope-coordinator/Users/alice/sub/new"))
    );
    check_frame_against_reference(&view, &mut pixels, [600, 400]);
}

#[test]
fn deep_scope_does_not_drop_directory_facts_outside_affected_child() {
    let root = "/deep-proof-coordinator";
    let alice = "/deep-proof-coordinator/Users/alice";
    let other = "/deep-proof-coordinator/Users/other";
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![
            file("/deep-proof-coordinator/Users/alice/f", alice, 11, 3, 100),
            file("/deep-proof-coordinator/Users/other/g", other, 12, 4, 100),
            file(
                "/deep-proof-coordinator/else/h",
                "/deep-proof-coordinator/else",
                13,
                5,
                200,
            ),
        ],
        directories: vec![
            fact(root, 1, true),
            fact("/deep-proof-coordinator/Users", 2, true),
            fact(alice, 3, false),
            fact(other, 4, false),
            fact("/deep-proof-coordinator/else", 5, false),
        ]
        .into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 600.,
        h: 400.,
    };
    let layout = directory_map::layout_with_partitions(
        [
            (Path::new(alice), vec![(0, 100)]),
            (Path::new(other), vec![(1, 100)]),
            (Path::new("/deep-proof-coordinator/else"), vec![(2, 200)]),
        ],
        Path::new(root),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [600, 400],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let unrelated = map.rect(2);
    let coordinator = Coordinator::start(source.clone(), 99, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(99);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Files,
            data: source,
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::default(),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
    let alice_count = crate::deep_scope::hit_count(Path::new(alice));
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(vec![
                Change::UpsertDirectory(fact(alice, 3, true)),
                Change::UpsertDirectory(fact("/deep-proof-coordinator/Users/alice/sub", 6, false)),
                Change::UpsertFile(file(
                    "/deep-proof-coordinator/Users/alice/sub/new",
                    "/deep-proof-coordinator/Users/alice/sub",
                    14,
                    6,
                    35,
                )),
                Change::UpsertDirectory(fact(other, 4, true)),
                Change::UpsertDirectory(fact(
                    "/deep-proof-coordinator/Users/other/empty",
                    7,
                    false,
                )),
            ]),
            modes: vec![ModeBase {
                mode: View::Files,
                layout_generation: 1,
                texture,
                map,
                holes: Arc::new(vec![]),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 4096,
            }],
        }))
        .ok()
        .unwrap();
    let Event::Prepared(mut prepared) = receive(&coordinator) else {
        panic!("prepared")
    };
    assert!(prepared.rebuild.is_empty());
    assert_eq!(
        crate::deep_scope::hit_count(Path::new(alice)),
        alice_count,
        "out-of-scope fact must not use alice-only geometry"
    );
    let view = prepared.views.pop().unwrap();
    assert_eq!(view.map.rect(2), unrelated);
    check_frame_against_reference(&view, &mut pixels, [600, 400]);
    coordinator
        .try_send(Command::Commit(prepared.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(Command::ExportMode {
            base: prepared.version,
            mode: View::Hybrid,
        })
        .ok()
        .unwrap();
    let Event::ModeSnapshot { data, .. } = receive(&coordinator) else {
        panic!("hybrid snapshot")
    };
    assert!(
        data.files
            .iter()
            .any(|entry| entry.path == Path::new("/deep-proof-coordinator/Users/other/g")),
        "outside empty-directory fact must change other from a leaf aggregate to individual files"
    );
    assert!(
        !data
            .files
            .iter()
            .any(|entry| entry.path == Path::new(other))
    );
}

#[test]
fn empty_root_identity_replacement_keeps_canvas_and_new_identity() {
    let root = "/empty-root-identity-regression";
    let child = "/empty-root-identity-regression/empty";
    let path = "/empty-root-identity-regression/file";
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![],
        directories: vec![fact(root, 1, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 80.,
        h: 60.,
    };
    let layout = directory_map::layout_with_partitions(
        std::iter::empty::<(&Path, Vec<(usize, u64)>)>(),
        Path::new(root),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [80, 60],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let coordinator = Coordinator::start(source.clone(), 124, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(124);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Files,
            data: source,
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::default(),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
    let update = |base, changes, map, holes| {
        Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes: vec![ModeBase {
                mode: View::Files,
                layout_generation: 1,
                texture,
                map,
                holes,
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 256,
            }],
        })
    };
    let fallbacks = crate::incremental_update::ROOT_FALLBACKS
        .lock()
        .unwrap()
        .get(Path::new(root))
        .copied()
        .unwrap_or(0);
    // The new empty child also forces structural processing. The root itself
    // has no old content, so replacing its identity is a valid transaction.
    coordinator
        .try_send(update(
            base,
            vec![
                Change::UpsertDirectory(fact(root, 2, true)),
                Change::UpsertDirectory(fact(child, 3, false)),
            ],
            map,
            Arc::new(vec![]),
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut replacement) = receive(&coordinator) else {
        panic!("root replacement")
    };
    assert_eq!(replacement.base, base);
    assert_eq!(replacement.version.scene_version, base.scene_version + 1);
    assert!(replacement.rebuild.is_empty());
    assert_eq!(
        crate::incremental_update::ROOT_FALLBACKS
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0),
        fallbacks
    );
    let view = replacement.views.pop().unwrap();
    assert!(view.projection.is_empty());
    assert!(view.patches.is_empty());
    assert_eq!(view.texture, texture);
    check_frame_against_reference(&view, &mut pixels, [80, 60]);
    coordinator
        .try_send(Command::Commit(replacement.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(Command::ExportMode {
            base: replacement.version,
            mode: View::Files,
        })
        .ok()
        .unwrap();
    let Event::ModeSnapshot {
        base: exported,
        data,
        ..
    } = receive(&coordinator)
    else {
        panic!("replacement snapshot")
    };
    assert_eq!(exported, replacement.version);
    assert_eq!(
        data.directories
            .iter()
            .find(|fact| fact.path == Path::new(root))
            .unwrap()
            .identity,
        id(2)
    );
    coordinator
        .try_send(update(
            replacement.version,
            vec![Change::UpsertFile(file(path, root, 4, 2, 50))],
            view.map,
            view.holes,
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut added) = receive(&coordinator) else {
        panic!("new file")
    };
    assert_eq!(added.base, replacement.version);
    assert_eq!(
        added.version.scene_version,
        replacement.version.scene_version + 1
    );
    assert!(added.rebuild.is_empty());
    assert_eq!(
        crate::incremental_update::ROOT_FALLBACKS
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0),
        fallbacks
    );
    let view = added.views.pop().unwrap();
    let slot = (0..view.projection.len())
        .find(|&slot| view.projection.get(slot).is_some())
        .unwrap();
    let entry = view.projection.get(slot).unwrap();
    assert_eq!(entry.path, Path::new(path));
    assert_eq!(entry.parent_identity, id(2));
    assert_eq!(entry.identity, id(4));
    assert_eq!(view.texture, texture);
    check_frame_against_reference(&view, &mut pixels, [80, 60]);
}

#[test]
fn folders_root_aggregate_in_content_can_disappear_reappear_and_update() {
    let root = "/folder-aggregate-rebirth";
    let sub = "/folder-aggregate-rebirth/sub";
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![file("/folder-aggregate-rebirth/sub/f", sub, 4, 3, 100)],
        directories: vec![fact(root, 1, true), fact(sub, 3, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let projected = Arc::new(scan::ScanResult {
        root: source.root.clone(),
        files: vec![scan::FileEntry {
            parent_is_leaf: false,
            ..file(sub, root, 3, 0, 100)
        }],
        directories: source.directories.clone(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 200.,
        h: 120.,
    };
    let layout = directory_map::layout_with_partitions(
        [(Path::new(root), vec![(0, 100)])],
        Path::new(root),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [200, 120],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let coordinator = Coordinator::start(source, 100, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let texture = TextureId::Managed(100);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Folders,
            data: projected,
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::default(),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
    let update = |base, changes, map, holes| {
        Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes: vec![ModeBase {
                mode: View::Folders,
                layout_generation: 1,
                texture,
                map,
                holes,
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 512,
            }],
        })
    };
    let fallback = || {
        crate::incremental_update::ROOT_FALLBACKS
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0)
    };
    let initial_fallback = fallback();
    let root_file = || scan::FileEntry {
        parent_is_leaf: false,
        ..file("/folder-aggregate-rebirth/f", root, 2, 1, 40)
    };
    coordinator
        .try_send(update(
            base,
            vec![Change::UpsertFile(root_file())],
            map,
            Arc::new(vec![]),
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut added) = receive(&coordinator) else {
        panic!("added")
    };
    assert!(added.rebuild.is_empty());
    let view = added.views.pop().unwrap();
    let aggregate_slot = (0..view.projection.len())
        .find(|&i| {
            view.projection
                .get(i)
                .is_some_and(|entry| entry.path == Path::new(root))
        })
        .unwrap();
    let aggregate_id = view.projection.id(aggregate_slot).unwrap();
    assert_eq!(
        view.geometry
            .tile_layout_parent(aggregate_id, Path::new("/")),
        Path::new(root)
    );
    check_frame_against_reference(&view, &mut pixels, [200, 120]);
    coordinator
        .try_send(Command::Commit(added.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(update(
            added.version,
            vec![Change::RemoveFile {
                path: "/folder-aggregate-rebirth/f".into(),
                identity: id(2),
            }],
            view.map,
            view.holes,
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut removed) = receive(&coordinator) else {
        panic!("removed")
    };
    assert!(removed.rebuild.is_empty());
    let view = removed.views.pop().unwrap();
    assert!(view.projection.get(aggregate_slot).is_none());
    assert_eq!(
        view.geometry
            .tile_layout_parent(aggregate_id, Path::new("/")),
        Path::new("/"),
        "removed aggregate must clear layout override"
    );
    check_frame_against_reference(&view, &mut pixels, [200, 120]);
    coordinator
        .try_send(Command::Commit(removed.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(update(
            removed.version,
            vec![Change::UpsertFile(root_file())],
            view.map,
            view.holes,
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut reborn) = receive(&coordinator) else {
        panic!("reborn")
    };
    assert!(reborn.rebuild.is_empty());
    let view = reborn.views.pop().unwrap();
    let reborn_slot = (0..view.projection.len())
        .find(|&i| {
            view.projection
                .get(i)
                .is_some_and(|entry| entry.path == Path::new(root))
        })
        .unwrap();
    assert_eq!(view.projection.id(reborn_slot), Some(aggregate_id));
    assert_eq!(
        view.geometry
            .tile_layout_parent(aggregate_id, Path::new("/")),
        Path::new(root)
    );
    assert_ne!(reborn_slot, aggregate_slot);
    check_frame_against_reference(&view, &mut pixels, [200, 120]);
    coordinator
        .try_send(Command::Commit(reborn.version))
        .ok()
        .unwrap();
    let mut grown = root_file();
    grown.size = 60;
    coordinator
        .try_send(update(
            reborn.version,
            vec![Change::UpsertFile(grown)],
            view.map,
            view.holes,
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut grown) = receive(&coordinator) else {
        panic!("grown")
    };
    assert!(grown.rebuild.is_empty());
    let view = grown.views.pop().unwrap();
    assert_eq!(view.projection.get(reborn_slot).unwrap().size, 60);
    assert_eq!(view.texture, texture);
    check_frame_against_reference(&view, &mut pixels, [200, 120]);
    assert_eq!(
        fallback(),
        initial_fallback,
        "root fallback during aggregate lifecycle"
    );
}

#[test]
fn deterministic_multi_transaction_all_modes_match_actor_and_full_raster_without_root_fallback() {
    let root = "/deterministic-multimode-root";
    let a = "/deterministic-multimode-root/a";
    let b = "/deterministic-multimode-root/b";
    let mut root_file = file("/deterministic-multimode-root/root", root, 10, 1, 70);
    root_file.parent_is_leaf = false;
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![
            root_file,
            file("/deterministic-multimode-root/a/f", a, 11, 2, 50),
            file("/deterministic-multimode-root/b/g", b, 12, 3, 60),
        ],
        directories: vec![fact(root, 1, true), fact(a, 2, false), fact(b, 3, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let coordinator = Coordinator::start(source, 101, egui::Context::default());
    let Event::Ready(mut base) = receive(&coordinator) else {
        panic!("ready")
    };
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 128.,
        h: 80.,
    };
    let token = AtomicU64::new(1);
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        holes: Arc<Vec<Rect>>,
        pixels: Vec<eframe::egui::Color32>,
        projection: Arc<SparseProjection>,
    }
    let mut states = Vec::new();
    for (idx, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("snapshot")
        };
        let mut groups: BTreeMap<&Path, Vec<(usize, u64)>> = BTreeMap::new();
        for (slot, entry) in data.files.iter().enumerate() {
            groups
                .entry(entry.parent.as_ref())
                .or_default()
                .push((slot, entry.size.max(1)));
        }
        let layout =
            directory_map::layout_with_partitions(groups, Path::new(root), bounds, 1., &token, 1)
                .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [128, 80],
            &token,
            1,
        )
        .unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let texture = TextureId::Managed(101 + idx as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        let Event::ModeReady(ready) = receive(&coordinator) else {
            panic!("mode ready")
        };
        states.push(State {
            mode,
            texture,
            map,
            holes: Arc::new(vec![]),
            pixels,
            projection: ready.projection,
        });
    }
    let mut batches = vec![
        vec![Change::UpsertFile(file(
            "/deterministic-multimode-root/a/new",
            a,
            13,
            2,
            25,
        ))],
        vec![Change::MoveFile {
            from: "/deterministic-multimode-root/a/new".into(),
            identity: id(13),
            to: file("/deterministic-multimode-root/b/new", b, 13, 3, 25),
        }],
        vec![
            Change::UpsertDirectory(fact(a, 2, true)),
            Change::UpsertDirectory(fact("/deterministic-multimode-root/a/sub", 4, false)),
            Change::UpsertFile(file(
                "/deterministic-multimode-root/a/sub/f",
                "/deterministic-multimode-root/a/sub",
                14,
                4,
                35,
            )),
        ],
        vec![
            Change::UpsertDirectory(fact(b, 3, true)),
            Change::UpsertDirectory(fact("/deterministic-multimode-root/b/empty", 5, false)),
        ],
        vec![
            Change::RemoveDirectory {
                path: "/deterministic-multimode-root/b/empty".into(),
                identity: id(5),
            },
            Change::UpsertDirectory(fact(b, 3, false)),
        ],
        vec![Change::RemoveLeafDirectory {
            path: "/deterministic-multimode-root/a/sub".into(),
            identity: id(4),
            parent: fact(a, 2, false),
        }],
        vec![Change::MoveFile {
            from: "/deterministic-multimode-root/root".into(),
            identity: id(10),
            to: file("/deterministic-multimode-root/b/root", b, 10, 3, 70),
        }],
        vec![Change::UpsertFile(scan::FileEntry {
            parent_is_leaf: false,
            ..file("/deterministic-multimode-root/x", root, 15, 1, 40)
        })],
        vec![Change::RemoveFile {
            path: "/deterministic-multimode-root/b/new".into(),
            identity: id(13),
        }],
        vec![Change::MoveFile {
            from: "/deterministic-multimode-root/b/g".into(),
            identity: id(12),
            to: file("/deterministic-multimode-root/a/g", a, 12, 2, 60),
        }],
    ];
    // Fixed-seed stateful sequence: all generated moves/removals refer to
    // existing entries, and each identity follows at most one current path.
    let mut rng = 0x51a7_1234_u64;
    let mut locations: [Option<usize>; 6] = [None; 6];
    let parents = [root, a, b];
    for _ in 0..48 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let i = (rng as usize) % locations.len();
        let destination = ((rng >> 12) as usize) % parents.len();
        let name = format!("generated-{i}");
        let make = |place: usize| {
            let mut entry = file(
                &format!("{}/{}", parents[place], name),
                parents[place],
                100 + i as u64,
                1 + place as u64,
                20 + i as u64 * 7,
            );
            entry.parent_is_leaf = place != 0;
            entry
        };
        let change = match locations[i] {
            None => {
                locations[i] = Some(destination);
                Change::UpsertFile(make(destination))
            }
            Some(old) if (rng >> 23) & 1 == 0 => {
                locations[i] = None;
                Change::RemoveFile {
                    path: make(old).path,
                    identity: id(100 + i as u64),
                }
            }
            Some(old) => {
                let new = if old == destination {
                    (old + 1) % parents.len()
                } else {
                    destination
                };
                locations[i] = Some(new);
                Change::MoveFile {
                    from: make(old).path,
                    identity: id(100 + i as u64),
                    to: make(new),
                }
            }
        };
        batches.push(vec![change]);
    }
    for (step, changes) in batches.into_iter().enumerate() {
        states.rotate_left(1);
        let before = crate::incremental_update::ROOT_FALLBACKS
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0);
        let general_before = crate::incremental_update::GENERAL_FALLBACKS
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0);
        let description = format!("{changes:?}");
        let mode_inputs = states
            .iter()
            .map(|s| ModeBase {
                mode: s.mode,
                layout_generation: 1,
                texture: s.texture,
                map: s.map.snapshot_for_update(),
                holes: s.holes.clone(),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 1024,
            })
            .collect();
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(changes),
                modes: mode_inputs,
            }))
            .ok()
            .unwrap();
        let mut prepared = match receive(&coordinator) {
            Event::Prepared(value) => value,
            Event::Failed(reason) => panic!("step {step}: {reason:?}"),
            _ => panic!("step {step}: unexpected event"),
        };
        assert!(
            prepared.rebuild.is_empty(),
            "step {step}: failed modes {:?}; {description}",
            prepared.rebuild
        );
        let after = crate::incremental_update::ROOT_FALLBACKS
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0);
        assert_eq!(after, before, "step {step}: root fallback");
        assert_eq!(
            crate::incremental_update::GENERAL_FALLBACKS
                .lock()
                .unwrap()
                .get(Path::new(root))
                .copied()
                .unwrap_or(0),
            general_before,
            "step {step}: general fallback; {description}"
        );
        for state in &mut states {
            let index = prepared
                .views
                .iter()
                .position(|view| view.mode == state.mode)
                .unwrap();
            let view = prepared.views.remove(index);
            check_frame_against_reference(&view, &mut state.pixels, [128, 80]);
            state.map = view.map;
            state.holes = view.holes;
            state.projection = view.projection;
        }
        coordinator
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        base = prepared.version;
        for state in &states {
            coordinator
                .try_send(Command::ExportMode {
                    base,
                    mode: state.mode,
                })
                .ok()
                .unwrap();
            let Event::ModeSnapshot { data, .. } = receive(&coordinator) else {
                panic!("step {step}: snapshot")
            };
            let details = |entry: &scan::FileEntry| {
                (
                    entry.path.clone(),
                    entry.size,
                    entry.parent.to_path_buf(),
                    entry.identity,
                    entry.parent_identity,
                    entry.parent_is_leaf,
                    entry.can_trash,
                )
            };
            let mut expected: Vec<_> = data.files.iter().map(details).collect();
            let mut actual: Vec<_> = (0..state.projection.len())
                .filter_map(|i| state.projection.get(i))
                .map(details)
                .collect();
            expected.sort_by(|a, b| a.0.cmp(&b.0));
            actual.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                actual, expected,
                "step {step}: full actor projection mismatch"
            );
            assert_eq!(
                state
                    .map
                    .entries()
                    .iter()
                    .filter(|entry| state.map.rect(entry.id).is_some())
                    .count(),
                data.files.len(),
                "step {step}: rendered count"
            );
        }
    }
}

#[test]
fn hybrid_file_reappears_after_visiting_leaf_folder() {
    let root = "/hybrid-file-reappear";
    let folder = "/hybrid-file-reappear/b";
    let mut first = file("/hybrid-file-reappear/f", root, 11, 1, 20);
    first.parent_is_leaf = false;
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![
            first.clone(),
            file("/hybrid-file-reappear/b/g", folder, 12, 2, 80),
        ],
        directories: vec![fact(root, 1, true), fact(folder, 2, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let coordinator = Coordinator::start(source, 102, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    coordinator
        .try_send(Command::ExportMode {
            base,
            mode: View::Hybrid,
        })
        .ok()
        .unwrap();
    let Event::ModeSnapshot {
        data, source_ids, ..
    } = receive(&coordinator)
    else {
        panic!("snapshot")
    };
    let mut groups: BTreeMap<&Path, Vec<(usize, u64)>> = BTreeMap::new();
    for (slot, entry) in data.files.iter().enumerate() {
        groups
            .entry(entry.parent.as_ref())
            .or_default()
            .push((slot, entry.size.max(1)));
    }
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 160.,
        h: 100.,
    };
    let token = AtomicU64::new(1);
    let layout =
        directory_map::layout_with_partitions(groups, Path::new(root), bounds, 1., &token, 1)
            .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [160, 100],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let texture = TextureId::Managed(102);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Hybrid,
            data,
            source_ids,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::default(),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    let Event::ModeReady(ready) = receive(&coordinator) else {
        panic!("mode ready")
    };
    let first_slot = (0..ready.projection.len())
        .find(|&i| {
            ready
                .projection
                .get(i)
                .is_some_and(|entry| entry.path == first.path)
        })
        .unwrap();
    let first_id = ready.projection.id(first_slot).unwrap();
    let update = |base, changes, map, holes| {
        Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes: vec![ModeBase {
                mode: View::Hybrid,
                layout_generation: 1,
                texture,
                map,
                holes,
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 1024,
            }],
        })
    };
    coordinator
        .try_send(update(
            base,
            vec![Change::MoveFile {
                from: first.path.clone(),
                identity: first.identity,
                to: file("/hybrid-file-reappear/b/f", folder, 11, 2, 20),
            }],
            map,
            Arc::new(vec![]),
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut folded) = receive(&coordinator) else {
        panic!("folded")
    };
    assert!(folded.rebuild.is_empty());
    let view = folded.views.pop().unwrap();
    assert!(view.projection.get(first_slot).is_none());
    check_frame_against_reference(&view, &mut pixels, [160, 100]);
    coordinator
        .try_send(Command::Commit(folded.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(update(
            folded.version,
            vec![Change::MoveFile {
                from: "/hybrid-file-reappear/b/f".into(),
                identity: id(11),
                to: first,
            }],
            view.map,
            view.holes,
        ))
        .ok()
        .unwrap();
    let Event::Prepared(mut unfolded) = receive(&coordinator) else {
        panic!("unfolded")
    };
    assert!(
        unfolded.rebuild.is_empty(),
        "same file identity must reappear in hybrid: {:?}",
        unfolded.rebuild
    );
    let view = unfolded.views.pop().unwrap();
    assert!(
        view.projection.get(first_slot).is_none(),
        "old frame slot remains a tombstone"
    );
    let slot = (0..view.projection.len())
        .find(|&i| view.projection.id(i) == Some(first_id))
        .unwrap();
    assert_ne!(slot, first_slot);
    assert_eq!(
        view.projection.get(slot).unwrap().path,
        Path::new("/hybrid-file-reappear/f")
    );
    check_frame_against_reference(&view, &mut pixels, [160, 100]);
}

fn stress_random_root(root: &str, mut rng: u64, nested: bool) {
    let join =
        |parent: &str, name: &str| Path::new(parent).join(name).to_string_lossy().into_owned();
    let a = join(root, "a");
    let branch = join(root, "branch");
    let b = if nested {
        join(&branch, "b")
    } else {
        join(root, "b")
    };
    let anchor = join(root, "anchor");
    let mut anchor_file = file(&anchor, root, 11, 1, 60);
    anchor_file.parent_is_leaf = false;
    let mut directories = vec![fact(root, 1, true), fact(&a, 2, false), fact(&b, 3, false)];
    if nested {
        directories.push(fact(&branch, 4, true));
    }
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![
            anchor_file,
            file(&join(&a, "f"), &a, 12, 2, 40),
            file(&join(&b, "g"), &b, 13, 3, 50),
        ],
        directories: directories.into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let coordinator = Coordinator::start(source, 103, egui::Context::default());
    let Event::Ready(mut base) = receive(&coordinator) else {
        panic!("ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 96.,
        h: 60.,
    };
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        holes: Arc<Vec<Rect>>,
        pixels: Vec<eframe::egui::Color32>,
        projection: Arc<SparseProjection>,
    }
    let mut states = Vec::new();
    for (index, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("snapshot")
        };
        let mut groups: BTreeMap<&Path, Vec<(usize, u64)>> = BTreeMap::new();
        for (slot, entry) in data.files.iter().enumerate() {
            groups
                .entry(entry.parent.as_ref())
                .or_default()
                .push((slot, entry.size.max(1)));
        }
        let layout =
            directory_map::layout_with_partitions(groups, Path::new(root), bounds, 1., &token, 1)
                .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [96, 60],
            &token,
            1,
        )
        .unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let texture = TextureId::Managed(103 + index as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        let Event::ModeReady(ready) = receive(&coordinator) else {
            panic!("mode ready")
        };
        states.push(State {
            mode,
            texture,
            map,
            holes: Arc::new(vec![]),
            pixels,
            projection: ready.projection,
        });
    }
    let parents = [root, a.as_str(), b.as_str()];
    let empty = join(&a, "empty");
    let mut empty_id = None;
    let mut next_empty_id = 200;
    let mut anchor_id = 11;
    let mut live: [Option<(usize, u64)>; 5] = [None; 5];
    for step in 0..48 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let kind = rng % 8;
        let changes = if kind == 0 {
            if let Some(old) = empty_id.take() {
                vec![
                    Change::RemoveDirectory {
                        path: empty.clone().into(),
                        identity: id(old),
                    },
                    Change::UpsertDirectory(fact(&a, 2, false)),
                ]
            } else {
                let next = next_empty_id;
                next_empty_id += 1;
                empty_id = Some(next);
                vec![
                    Change::UpsertDirectory(fact(&a, 2, true)),
                    Change::UpsertDirectory(fact(&empty, next, false)),
                ]
            }
        } else if kind == 1 {
            anchor_id += 1;
            let mut entry = file(&anchor, root, anchor_id, 1, 60 + step);
            entry.parent_is_leaf = false;
            vec![Change::UpsertFile(entry)]
        } else {
            let i = ((rng >> 8) as usize) % live.len();
            let destination = ((rng >> 16) as usize) % 3;
            let name = format!("generated-{i}");
            let make = |place: usize, identity: u64| {
                let mut entry = file(
                    &join(parents[place], &name),
                    parents[place],
                    identity,
                    1 + place as u64,
                    20 + i as u64 * 7,
                );
                entry.parent_is_leaf = place != 0 && !(place == 1 && empty_id.is_some());
                entry
            };
            match live[i] {
                None => {
                    let identity = 1000 + step * 10 + i as u64;
                    live[i] = Some((destination, identity));
                    vec![Change::UpsertFile(make(destination, identity))]
                }
                Some((old, identity)) if kind == 2 => {
                    live[i] = None;
                    vec![Change::RemoveFile {
                        path: make(old, identity).path,
                        identity: id(identity),
                    }]
                }
                Some((old, _identity)) if kind == 3 => {
                    let next = 1000 + step * 10 + i as u64;
                    live[i] = Some((old, next));
                    vec![Change::UpsertFile(make(old, next))]
                }
                Some((old, identity)) => {
                    let new = if old == destination {
                        (old + 1) % 3
                    } else {
                        destination
                    };
                    live[i] = Some((new, identity));
                    vec![Change::MoveFile {
                        from: make(old, identity).path,
                        identity: id(identity),
                        to: make(new, identity),
                    }]
                }
            }
        };
        let description = format!("{changes:?}");
        states.rotate_left(1);
        let before = crate::incremental_update::ROOT_FALLBACKS
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0);
        let general_before = crate::incremental_update::GENERAL_FALLBACKS
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0);
        let modes = states
            .iter()
            .map(|state| ModeBase {
                mode: state.mode,
                layout_generation: 1,
                texture: state.texture,
                map: state.map.snapshot_for_update(),
                holes: state.holes.clone(),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 1024,
            })
            .collect();
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(changes),
                modes,
            }))
            .ok()
            .unwrap();
        let mut prepared = match receive(&coordinator) {
            Event::Prepared(value) => value,
            Event::Failed(reason) => panic!("root {root}, step {step}, {description}: {reason:?}"),
            _ => panic!("root {root}, step {step}: event"),
        };
        assert!(
            prepared.rebuild.is_empty(),
            "root {root}, step {step}, {description}: {:?}",
            prepared.rebuild
        );
        assert_eq!(
            crate::incremental_update::ROOT_FALLBACKS
                .lock()
                .unwrap()
                .get(Path::new(root))
                .copied()
                .unwrap_or(0),
            before,
            "root {root}, step {step}, {description}: fallback"
        );
        assert_eq!(
            crate::incremental_update::GENERAL_FALLBACKS
                .lock()
                .unwrap()
                .get(Path::new(root))
                .copied()
                .unwrap_or(0),
            general_before,
            "root {root}, step {step}, {description}: general fallback"
        );
        for state in &mut states {
            let index = prepared
                .views
                .iter()
                .position(|view| view.mode == state.mode)
                .unwrap();
            let view = prepared.views.remove(index);
            check_frame_against_reference(&view, &mut state.pixels, [96, 60]);
            state.map = view.map;
            state.holes = view.holes;
            state.projection = view.projection;
        }
        coordinator
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        base = prepared.version;
        for state in &states {
            coordinator
                .try_send(Command::ExportMode {
                    base,
                    mode: state.mode,
                })
                .ok()
                .unwrap();
            let Event::ModeSnapshot { data, .. } = receive(&coordinator) else {
                panic!("snapshot")
            };
            let details = |entry: &scan::FileEntry| {
                (
                    entry.path.clone(),
                    entry.size,
                    entry.parent.to_path_buf(),
                    entry.identity,
                    entry.parent_identity,
                    entry.parent_is_leaf,
                    entry.can_trash,
                )
            };
            let mut expected: Vec<_> = data.files.iter().map(details).collect();
            let mut actual: Vec<_> = (0..state.projection.len())
                .filter_map(|i| state.projection.get(i))
                .map(details)
                .collect();
            expected.sort_by(|a, b| a.0.cmp(&b.0));
            actual.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                actual, expected,
                "root {root}, step {step}, {description}: projection"
            );
        }
    }
}

#[test]
fn generated_transactions_across_scan_roots_and_directory_states_are_incremental() {
    stress_random_root("/", 0x1234_abcd, false);
    stress_random_root("/random-stress-root", 0x9876_5432, false);
    stress_random_root("/random-nested-root", 0x7102_efdc, true);
}

fn check_empty_root_replacement_populates_new_tree(deep: bool) {
    let root = if deep {
        "/replacement-deep-proof"
    } else {
        "/replacement-direct-proof"
    };
    let a = format!("{root}/a");
    let b = format!("{root}/a/b");
    let (parent, parent_id, path) = if deep {
        (b.as_str(), 4, format!("{b}/f"))
    } else {
        (root, 2, format!("{root}/f"))
    };
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![],
        directories: vec![fact(root, 1, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let coordinator = Coordinator::start(source, 130, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 80.,
        h: 60.,
    };
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        pixels: Vec<eframe::egui::Color32>,
    }
    let mut states = Vec::new();
    for (i, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("initial export")
        };
        let layout = directory_map::layout_with_partitions(
            std::iter::empty::<(&Path, Vec<(usize, u64)>)>(),
            Path::new(root),
            bounds,
            1.,
            &token,
            1,
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [80, 60],
            &token,
            1,
        )
        .unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let texture = TextureId::Managed(130 + i as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
        states.push(State {
            mode,
            texture,
            map,
            pixels,
        });
    }
    let get_count = |counter: &std::sync::Mutex<BTreeMap<PathBuf, usize>>| {
        counter
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0)
    };
    let root_before = get_count(&crate::incremental_update::ROOT_FALLBACKS);
    let general_before = get_count(&crate::incremental_update::GENERAL_FALLBACKS);
    let mut changes = vec![Change::UpsertDirectory(fact(root, 2, deep))];
    if deep {
        changes.push(Change::UpsertDirectory(fact(&a, 3, true)));
        changes.push(Change::UpsertDirectory(fact(&b, 4, false)));
    }
    changes.push(Change::UpsertFile(file(&path, parent, 5, parent_id, 55)));
    let modes = states
        .iter()
        .map(|state| ModeBase {
            mode: state.mode,
            layout_generation: 1,
            texture: state.texture,
            map: state.map.snapshot_for_update(),
            holes: Arc::default(),
            pixels_per_point: 1.,
            area_tolerance: 10000.,
            max_patch_pixels: 256,
        })
        .collect();
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes,
        }))
        .ok()
        .unwrap();
    let mut prepared = match receive(&coordinator) {
        Event::Prepared(prepared) => prepared,
        Event::Failed(failure) => panic!("replacement failed: {failure:?}"),
        _ => panic!("prepared"),
    };
    assert!(
        prepared.rebuild.is_empty(),
        "rebuild: {:?}",
        prepared.rebuild
    );
    assert_eq!(
        get_count(&crate::incremental_update::ROOT_FALLBACKS),
        root_before
    );
    assert_eq!(
        get_count(&crate::incremental_update::GENERAL_FALLBACKS),
        general_before
    );
    let mut projections = Vec::new();
    for state in &mut states {
        let i = prepared
            .views
            .iter()
            .position(|view| view.mode == state.mode)
            .unwrap();
        let view = prepared.views.remove(i);
        check_frame_against_reference(&view, &mut state.pixels, [80, 60]);
        assert_eq!(view.texture, state.texture);
        projections.push((state.mode, view.projection.clone()));
    }
    coordinator
        .try_send(Command::Commit(prepared.version))
        .ok()
        .unwrap();
    for (mode, projection) in projections {
        coordinator
            .try_send(Command::ExportMode {
                base: prepared.version,
                mode,
            })
            .ok()
            .unwrap();
        let Event::ModeSnapshot { data, .. } = receive(&coordinator) else {
            panic!("final export")
        };
        assert_eq!(
            data.directories
                .iter()
                .find(|fact| fact.path == Path::new(root))
                .unwrap()
                .identity,
            id(2)
        );
        let details = |entry: &scan::FileEntry| {
            (
                entry.path.clone(),
                entry.size,
                entry.parent.to_path_buf(),
                entry.identity,
                entry.parent_identity,
                entry.parent_is_leaf,
                entry.can_trash,
            )
        };
        let mut expected: Vec<_> = data.files.iter().map(details).collect();
        let mut actual: Vec<_> = (0..projection.len())
            .filter_map(|i| projection.get(i))
            .map(details)
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        actual.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            actual, expected,
            "mode projection differs after identity replacement"
        );
        assert!(!actual.is_empty());
        if mode == View::Files {
            assert_eq!(actual[0].4, id(parent_id));
        }
    }
}

#[test]
fn empty_root_identity_replacement_with_direct_file_is_incremental_in_all_modes() {
    check_empty_root_replacement_populates_new_tree(false);
}

#[test]
fn empty_root_identity_replacement_with_multilevel_subtree_is_incremental_in_all_modes() {
    check_empty_root_replacement_populates_new_tree(true);
}

#[test]
fn stale_parent_identity_after_root_replacement_remains_non_deletable() {
    let root = "/replacement-parent-conflict";
    let path = "/replacement-parent-conflict/f";
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![],
        directories: vec![fact(root, 1, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let coordinator = Coordinator::start(source.clone(), 140, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 80.,
        h: 60.,
    };
    let layout = directory_map::layout_with_partitions(
        std::iter::empty::<(&Path, Vec<(usize, u64)>)>(),
        Path::new(root),
        bounds,
        1.,
        &token,
        1,
    )
    .unwrap();
    let map = RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        [80, 60],
        &token,
        1,
    )
    .unwrap();
    let mut pixels = map.image.as_ref().unwrap().pixels.clone();
    let texture = TextureId::Managed(140);
    coordinator
        .try_send(Command::InitMode(ModeInput {
            base,
            mode: View::Folders,
            data: source,
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions)),
            retained: None,
            tombstones: Arc::default(),
            map: map.snapshot_for_update(),
            layout_generation: 1,
            texture,
        }))
        .ok()
        .unwrap();
    assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(vec![
                Change::UpsertDirectory(fact(root, 2, false)),
                Change::UpsertFile(file(path, root, 3, 1, 50)),
            ]),
            modes: vec![ModeBase {
                mode: View::Folders,
                layout_generation: 1,
                texture,
                map,
                holes: Arc::default(),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 256,
            }],
        }))
        .ok()
        .unwrap();
    let mut prepared = match receive(&coordinator) {
        Event::Prepared(value) => value,
        Event::Failed(reason) => panic!("replacement: {reason:?}"),
        _ => panic!("prepared"),
    };
    assert!(prepared.rebuild.is_empty());
    let view = prepared.views.pop().unwrap();
    let projected: Vec<_> = (0..view.projection.len())
        .filter_map(|i| view.projection.get(i))
        .collect();
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].path, Path::new(root));
    assert!(
        !projected[0].can_trash,
        "stale parent identity must prevent deleting the aggregate"
    );
    check_frame_against_reference(&view, &mut pixels, [80, 60]);
    coordinator
        .try_send(Command::Commit(prepared.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(Command::Update(UpdateInput {
            base: prepared.version,
            changes: Changes::Explicit(vec![Change::RemoveFile {
                path: path.into(),
                identity: id(999),
            }]),
            modes: vec![ModeBase {
                mode: View::Folders,
                layout_generation: 1,
                texture,
                map: view.map,
                holes: view.holes,
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 256,
            }],
        }))
        .ok()
        .unwrap();
    assert!(
        matches!(receive(&coordinator), Event::Failed(_)),
        "stale file identity must not be removed"
    );
    coordinator
        .try_send(Command::ExportMode {
            base: prepared.version,
            mode: View::Folders,
        })
        .ok()
        .unwrap();
    let Event::ModeSnapshot { data, .. } = receive(&coordinator) else {
        panic!("retained snapshot")
    };
    assert_eq!(data.files.len(), 1);
    assert_eq!(data.files[0].path, Path::new(root));
    assert!(!data.files[0].can_trash);
}

#[test]
fn removing_last_direct_file_then_adding_new_branch_preserves_all_mode_pixels() {
    let root = "/donor-after-empty-coordinator";
    let a = "/donor-after-empty-coordinator/a";
    let sub = "/donor-after-empty-coordinator/a/sub";
    let b = "/donor-after-empty-coordinator/b";
    let c = "/donor-after-empty-coordinator/c";
    let mut direct = file("/donor-after-empty-coordinator/a/direct", a, 11, 2, 50);
    direct.parent_is_leaf = false;
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![
            direct,
            file("/donor-after-empty-coordinator/a/sub/child", sub, 12, 3, 50),
            file("/donor-after-empty-coordinator/b/large", b, 13, 4, 180),
        ],
        directories: vec![
            fact(root, 1, true),
            fact(a, 2, true),
            fact(sub, 3, false),
            fact(b, 4, false),
            fact(c, 5, false),
        ]
        .into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let coordinator = Coordinator::start(source, 150, egui::Context::default());
    let Event::Ready(mut base) = receive(&coordinator) else {
        panic!("ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 200.,
        h: 120.,
    };
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        holes: Arc<Vec<Rect>>,
        pixels: Vec<eframe::egui::Color32>,
        initial_platforms: usize,
    }
    let mut states = Vec::new();
    for (i, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("export")
        };
        let mut groups: BTreeMap<&Path, Vec<(usize, u64)>> = BTreeMap::new();
        for (slot, entry) in data.files.iter().enumerate() {
            groups
                .entry(entry.parent.as_ref())
                .or_default()
                .push((slot, entry.size.max(1)));
        }
        let layout =
            directory_map::layout_with_partitions(groups, Path::new(root), bounds, 1., &token, 1)
                .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [200, 120],
            &token,
            1,
        )
        .unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let initial_platforms = map.platforms().len();
        let texture = TextureId::Managed(150 + i as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
        states.push(State {
            mode,
            texture,
            map,
            holes: Arc::default(),
            pixels,
            initial_platforms,
        });
    }
    let fallback = |counter: &std::sync::Mutex<BTreeMap<PathBuf, usize>>| {
        counter
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0)
    };
    let initial_root = fallback(&crate::incremental_update::ROOT_FALLBACKS);
    let initial_general = fallback(&crate::incremental_update::GENERAL_FALLBACKS);
    for (step, changes) in [
        vec![Change::RemoveFile {
            path: "/donor-after-empty-coordinator/a/direct".into(),
            identity: id(11),
        }],
        vec![Change::UpsertFile(file(
            "/donor-after-empty-coordinator/c/new",
            c,
            14,
            5,
            70,
        ))],
        vec![Change::UpsertFile(scan::FileEntry {
            parent_is_leaf: false,
            ..file("/donor-after-empty-coordinator/a/direct", a, 15, 2, 50)
        })],
    ]
    .into_iter()
    .enumerate()
    {
        let modes = states
            .iter()
            .map(|state| ModeBase {
                mode: state.mode,
                layout_generation: 1,
                texture: state.texture,
                map: state.map.snapshot_for_update(),
                holes: state.holes.clone(),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 512,
            })
            .collect();
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(changes),
                modes,
            }))
            .ok()
            .unwrap();
        let mut prepared = match receive(&coordinator) {
            Event::Prepared(value) => value,
            Event::Failed(failure) => panic!("step {step}: {failure:?}"),
            _ => panic!("prepared"),
        };
        assert!(
            prepared.rebuild.is_empty(),
            "step {step}: {:?}",
            prepared.rebuild
        );
        assert_eq!(
            fallback(&crate::incremental_update::ROOT_FALLBACKS),
            initial_root,
            "step {step}"
        );
        assert_eq!(
            fallback(&crate::incremental_update::GENERAL_FALLBACKS),
            initial_general,
            "step {step}"
        );
        for state in &mut states {
            let i = prepared
                .views
                .iter()
                .position(|view| view.mode == state.mode)
                .unwrap();
            let view = prepared.views.remove(i);
            check_frame_against_reference(&view, &mut state.pixels, [200, 120]);
            if state.mode == View::Files && step == 0 {
                assert_eq!(
                    view.map.platforms().len() + 1,
                    state.initial_platforms,
                    "direct-file removal must collapse unary parent platform"
                );
            }
            if state.mode == View::Files && step == 2 {
                assert!(
                    view.map.platforms().len() >= state.initial_platforms,
                    "re-adding the direct file must restore its parent platform"
                );
            }
            state.map = view.map;
            state.holes = view.holes;
        }
        coordinator
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        base = prepared.version;
    }
}

#[test]
fn removing_entire_root_branch_then_adding_another_reallocates_space_in_all_modes() {
    let root = "/root-branch-reuse-regression";
    let a = "/root-branch-reuse-regression/a";
    let b = "/root-branch-reuse-regression/b";
    let c = "/root-branch-reuse-regression/c";
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![
            file("/root-branch-reuse-regression/a/only", a, 11, 2, 80),
            file("/root-branch-reuse-regression/b/keep", b, 12, 3, 120),
        ],
        directories: vec![
            fact(root, 1, true),
            fact(a, 2, false),
            fact(b, 3, false),
            fact(c, 4, false),
        ]
        .into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let coordinator = Coordinator::start(source, 160, egui::Context::default());
    let Event::Ready(mut base) = receive(&coordinator) else {
        panic!("ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 200.,
        h: 120.,
    };
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        holes: Arc<Vec<Rect>>,
        pixels: Vec<eframe::egui::Color32>,
        frame: Rect,
        a_tile: Option<TileId>,
        b_slot: usize,
        b_rect: Rect,
    }
    let mut states = Vec::new();
    for (i, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("export")
        };
        let mut groups: BTreeMap<&Path, Vec<(usize, u64)>> = BTreeMap::new();
        for (slot, entry) in data.files.iter().enumerate() {
            groups
                .entry(entry.parent.as_ref())
                .or_default()
                .push((slot, entry.size.max(1)));
        }
        let a_slot = data
            .files
            .iter()
            .position(|entry| {
                if mode == View::Files {
                    entry.path == Path::new("/root-branch-reuse-regression/a/only")
                } else {
                    entry.path == Path::new(a)
                }
            })
            .unwrap();
        let b_slot = data
            .files
            .iter()
            .position(|entry| {
                if mode == View::Files {
                    entry.path == Path::new("/root-branch-reuse-regression/b/keep")
                } else {
                    entry.path == Path::new(b)
                }
            })
            .unwrap();
        let layout =
            directory_map::layout_with_partitions(groups, Path::new(root), bounds, 1., &token, 1)
                .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [200, 120],
            &token,
            1,
        )
        .unwrap();
        let b_rect = map.rect(b_slot).unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let texture = TextureId::Managed(160 + i as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        let Event::ModeReady(ready) = receive(&coordinator) else {
            panic!("mode ready")
        };
        let a_tile = (mode != View::Files).then(|| ready.projection.id(a_slot).unwrap());
        let frame = ready
            .geometry
            .root_fold_frame(Path::new(root), Path::new(a), a_tile)
            .unwrap();
        states.push(State {
            mode,
            texture,
            map,
            holes: Arc::default(),
            pixels,
            frame,
            a_tile,
            b_slot,
            b_rect,
        });
    }
    let fallback = |counter: &std::sync::Mutex<BTreeMap<PathBuf, usize>>| {
        counter
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0)
    };
    let initial_root = fallback(&crate::incremental_update::ROOT_FALLBACKS);
    let initial_general = fallback(&crate::incremental_update::GENERAL_FALLBACKS);
    for (step, changes) in [
        vec![Change::RemoveFile {
            path: "/root-branch-reuse-regression/a/only".into(),
            identity: id(11),
        }],
        vec![Change::UpsertFile(file(
            "/root-branch-reuse-regression/c/new",
            c,
            13,
            4,
            60,
        ))],
    ]
    .into_iter()
    .enumerate()
    {
        let modes = states
            .iter()
            .map(|state| ModeBase {
                mode: state.mode,
                layout_generation: 1,
                texture: state.texture,
                map: state.map.snapshot_for_update(),
                holes: state.holes.clone(),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 512,
            })
            .collect();
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(changes),
                modes,
            }))
            .ok()
            .unwrap();
        let mut prepared = match receive(&coordinator) {
            Event::Prepared(value) => value,
            Event::Failed(failure) => panic!("step {step}: {failure:?}"),
            _ => panic!("prepared"),
        };
        assert!(
            prepared.rebuild.is_empty(),
            "step {step}: {:?}",
            prepared.rebuild
        );
        assert_eq!(
            fallback(&crate::incremental_update::ROOT_FALLBACKS),
            initial_root,
            "step {step}"
        );
        assert_eq!(
            fallback(&crate::incremental_update::GENERAL_FALLBACKS),
            initial_general,
            "step {step}"
        );
        for state in &mut states {
            let i = prepared
                .views
                .iter()
                .position(|view| view.mode == state.mode)
                .unwrap();
            let view = prepared.views.remove(i);
            assert!(
                !view.transitions.is_empty(),
                "step {step} mode {:?} needs a visual transition",
                state.mode
            );
            if step == 0 {
                assert!(
                    view.geometry
                        .root_fold_frame(Path::new(root), Path::new(a), state.a_tile)
                        .is_none(),
                    "empty root branch must release its frame"
                );
            } else {
                let tile = if state.mode == View::Files {
                    None
                } else {
                    let i = (0..view.projection.len())
                        .find(|&slot| {
                            view.projection
                                .get(slot)
                                .is_some_and(|entry| entry.path == Path::new(c))
                        })
                        .unwrap();
                    view.projection.id(i)
                };
                let frame = view
                    .geometry
                    .root_fold_frame(Path::new(root), Path::new(c), tile)
                    .expect("new branch has an allocation");
                if view.map.rect(state.b_slot) == Some(state.b_rect) {
                    assert_eq!(
                        frame, state.frame,
                        "unchanged sibling implies vacant frame reuse"
                    );
                }
            }
            check_frame_against_reference(&view, &mut state.pixels, [200, 120]);
            state.map = view.map;
            state.holes = view.holes;
        }
        coordinator
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        base = prepared.version;
    }
}

fn check_incomplete_root_replacement(case: usize) {
    let root = format!("/replacement-incomplete-{case}");
    let a = format!("{root}/a");
    let hidden = format!("{a}/unknown");
    let mut old_root = fact(&root, 1, false);
    if case == 0 {
        old_root.enumeration_complete = false;
    }
    if case == 1 {
        old_root.excluded = true;
    }
    let mut new_root = fact(&root, 2, true);
    if case == 2 {
        new_root.enumeration_complete = false;
    }
    if case == 3 {
        new_root.excluded = true;
    }
    let mut unknown = fact(&hidden, 4, false);
    if case % 2 == 0 {
        unknown.enumeration_complete = false;
    } else {
        unknown.excluded = true;
    }
    let source = Arc::new(scan::ScanResult {
        root: root.clone().into(),
        files: vec![],
        directories: vec![old_root].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let coordinator = Coordinator::start(source, 170 + case as u64, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 80.,
        h: 60.,
    };
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        pixels: Vec<eframe::egui::Color32>,
    }
    let mut states = Vec::new();
    for (i, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("initial export")
        };
        let layout = directory_map::layout_with_partitions(
            std::iter::empty::<(&Path, Vec<(usize, u64)>)>(),
            Path::new(&root),
            bounds,
            1.,
            &token,
            1,
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [80, 60],
            &token,
            1,
        )
        .unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let texture = TextureId::Managed(170 + 3 * case as u64 + i as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
        states.push(State {
            mode,
            texture,
            map,
            pixels,
        });
    }
    let get_count = |counter: &std::sync::Mutex<BTreeMap<PathBuf, usize>>| {
        counter
            .lock()
            .unwrap()
            .get(Path::new(&root))
            .copied()
            .unwrap_or(0)
    };
    let root_before = get_count(&crate::incremental_update::ROOT_FALLBACKS);
    let general_before = get_count(&crate::incremental_update::GENERAL_FALLBACKS);
    let mut child_file = file(&format!("{a}/f"), &a, 5, 3, 55);
    child_file.parent_is_leaf = false;
    let changes = vec![
        Change::UpsertDirectory(new_root.clone()),
        Change::UpsertDirectory(fact(&a, 3, true)),
        Change::UpsertDirectory(unknown.clone()),
        Change::UpsertFile(child_file),
    ];
    let modes = states
        .iter()
        .map(|state| ModeBase {
            mode: state.mode,
            layout_generation: 1,
            texture: state.texture,
            map: state.map.snapshot_for_update(),
            holes: Arc::default(),
            pixels_per_point: 1.,
            area_tolerance: 10000.,
            max_patch_pixels: 256,
        })
        .collect();
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes,
        }))
        .ok()
        .unwrap();
    let mut prepared = match receive(&coordinator) {
        Event::Prepared(prepared) => prepared,
        Event::Failed(failure) => panic!("case {case}: {failure:?}"),
        _ => panic!("prepared"),
    };
    assert!(
        prepared.rebuild.is_empty(),
        "case {case}: {:?}",
        prepared.rebuild
    );
    assert_eq!(
        get_count(&crate::incremental_update::ROOT_FALLBACKS),
        root_before,
        "case {case}"
    );
    assert_eq!(
        get_count(&crate::incremental_update::GENERAL_FALLBACKS),
        general_before,
        "case {case}"
    );
    let mut projections = Vec::new();
    for state in &mut states {
        let i = prepared
            .views
            .iter()
            .position(|view| view.mode == state.mode)
            .unwrap();
        let view = prepared.views.remove(i);
        check_frame_against_reference(&view, &mut state.pixels, [80, 60]);
        projections.push((state.mode, view.projection.clone()));
    }
    coordinator
        .try_send(Command::Commit(prepared.version))
        .ok()
        .unwrap();
    for (mode, projection) in projections {
        coordinator
            .try_send(Command::ExportMode {
                base: prepared.version,
                mode,
            })
            .ok()
            .unwrap();
        let Event::ModeSnapshot { data, .. } = receive(&coordinator) else {
            panic!("final export")
        };
        let root_fact = data
            .directories
            .iter()
            .find(|fact| fact.path == Path::new(&root))
            .unwrap();
        assert_eq!(
            (
                root_fact.identity,
                root_fact.enumeration_complete,
                root_fact.excluded
            ),
            (id(2), new_root.enumeration_complete, new_root.excluded)
        );
        let child_fact = data
            .directories
            .iter()
            .find(|fact| fact.path == Path::new(&hidden))
            .unwrap();
        assert_eq!(
            (child_fact.enumeration_complete, child_fact.excluded),
            (unknown.enumeration_complete, unknown.excluded)
        );
        let details = |entry: &scan::FileEntry| {
            (
                entry.path.clone(),
                entry.size,
                entry.parent.to_path_buf(),
                entry.identity,
                entry.parent_identity,
                entry.parent_is_leaf,
                entry.can_trash,
            )
        };
        let mut expected: Vec<_> = data.files.iter().map(details).collect();
        let mut actual: Vec<_> = (0..projection.len())
            .filter_map(|i| projection.get(i))
            .map(details)
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        actual.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(actual, expected, "case {case}: projection");
        if mode == View::Folders {
            let aggregate = data
                .files
                .iter()
                .find(|entry| entry.path == Path::new(&a))
                .unwrap();
            assert!(
                !aggregate.can_trash,
                "case {case}: incomplete descendant protects directory"
            );
        }
    }
}

#[test]
fn incomplete_or_excluded_root_replacement_keeps_protection_and_is_incremental() {
    for case in 0..4 {
        check_incomplete_root_replacement(case);
    }
}

#[test]
fn stale_parent_identity_in_empty_root_replacement_is_safe_in_all_modes_without_fallback() {
    let root = "/replacement-parent-conflict-multimode";
    let path = "/replacement-parent-conflict-multimode/f";
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![],
        directories: vec![fact(root, 1, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let coordinator = Coordinator::start(source, 180, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 80.,
        h: 60.,
    };
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        pixels: Vec<eframe::egui::Color32>,
    }
    let mut states = Vec::new();
    for (i, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("initial export")
        };
        let layout = directory_map::layout_with_partitions(
            std::iter::empty::<(&Path, Vec<(usize, u64)>)>(),
            Path::new(root),
            bounds,
            1.,
            &token,
            1,
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [80, 60],
            &token,
            1,
        )
        .unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let texture = TextureId::Managed(180 + i as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
        states.push(State {
            mode,
            texture,
            map,
            pixels,
        });
    }
    let get_count = |counter: &std::sync::Mutex<BTreeMap<PathBuf, usize>>| {
        counter
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0)
    };
    let root_before = get_count(&crate::incremental_update::ROOT_FALLBACKS);
    let general_before = get_count(&crate::incremental_update::GENERAL_FALLBACKS);
    let changes = vec![
        Change::UpsertDirectory(fact(root, 2, false)),
        Change::UpsertFile(file(path, root, 3, 1, 50)),
    ];
    let modes = states
        .iter()
        .map(|state| ModeBase {
            mode: state.mode,
            layout_generation: 1,
            texture: state.texture,
            map: state.map.snapshot_for_update(),
            holes: Arc::default(),
            pixels_per_point: 1.,
            area_tolerance: 10000.,
            max_patch_pixels: 256,
        })
        .collect();
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes,
        }))
        .ok()
        .unwrap();
    let mut prepared = match receive(&coordinator) {
        Event::Prepared(prepared) => prepared,
        Event::Failed(failure) => panic!("replacement failed: {failure:?}"),
        _ => panic!("prepared"),
    };
    assert!(
        prepared.rebuild.is_empty(),
        "rebuild: {:?}",
        prepared.rebuild
    );
    assert_eq!(
        get_count(&crate::incremental_update::ROOT_FALLBACKS),
        root_before
    );
    assert_eq!(
        get_count(&crate::incremental_update::GENERAL_FALLBACKS),
        general_before
    );
    let mut mode_inputs = Vec::new();
    for state in &mut states {
        let i = prepared
            .views
            .iter()
            .position(|view| view.mode == state.mode)
            .unwrap();
        let view = prepared.views.remove(i);
        let entries: Vec<_> = (0..view.projection.len())
            .filter_map(|i| view.projection.get(i))
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(
            !entries[0].can_trash,
            "mode {:?} must protect identity conflict",
            state.mode
        );
        check_frame_against_reference(&view, &mut state.pixels, [80, 60]);
        mode_inputs.push(ModeBase {
            mode: state.mode,
            layout_generation: 1,
            texture: state.texture,
            map: view.map,
            holes: view.holes,
            pixels_per_point: 1.,
            area_tolerance: 10000.,
            max_patch_pixels: 256,
        });
    }
    coordinator
        .try_send(Command::Commit(prepared.version))
        .ok()
        .unwrap();
    coordinator
        .try_send(Command::Update(UpdateInput {
            base: prepared.version,
            changes: Changes::Explicit(vec![Change::RemoveFile {
                path: path.into(),
                identity: id(999),
            }]),
            modes: mode_inputs,
        }))
        .ok()
        .unwrap();
    assert!(
        matches!(receive(&coordinator), Event::Failed(_)),
        "wrong identity removal must be rejected"
    );
    coordinator
        .try_send(Command::ExportMode {
            base: prepared.version,
            mode: View::Files,
        })
        .ok()
        .unwrap();
    let Event::ModeSnapshot { data, .. } = receive(&coordinator) else {
        panic!("retained export")
    };
    assert_eq!(data.files.len(), 1);
    assert_eq!(data.files[0].identity, id(3));
    assert_eq!(data.files[0].parent_identity, id(1));
}

#[derive(Clone)]
struct ExhaustiveWorld {
    files: Vec<scan::FileEntry>,
    directories: Vec<scan::DirectoryFact>,
}
impl ExhaustiveWorld {
    fn snapshot(&self, root: &str) -> Arc<scan::ScanResult> {
        let mut files = self.files.clone();
        for file in &mut files {
            file.parent_is_leaf = self
                .directories
                .iter()
                .find(|fact| fact.path == file.parent.as_ref())
                .is_some_and(|fact| fact.enumeration_complete && !fact.has_subdirectories);
        }
        Arc::new(scan::ScanResult {
            root: root.into(),
            files,
            directories: self.directories.clone().into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        })
    }
}

fn check_exhaustive_case(case: usize, occupancy: u8, shape: u8, scenario: u8) {
    use crate::scene_model::Scene;
    let root = format!("/exhaustive-small-{case}");
    let a = format!("{root}/a");
    let b = format!("{root}/b");
    let empty = format!("{a}/empty");
    let root_slot = format!("{root}/slot");
    let a_base = format!("{a}/base");
    let b_base = format!("{b}/base");
    let root_to_a = format!("{a}/slot");
    let b_to_a = format!("{a}/moved");
    let mut world = ExhaustiveWorld {
        files: Vec::new(),
        directories: vec![
            fact(&root, 1, true),
            fact(&a, 2, shape != 0),
            fact(&b, 3, false),
        ],
    };
    if shape != 0 {
        let mut child = fact(&empty, 4, false);
        child.enumeration_complete = shape != 2;
        world.directories.push(child);
    }
    if occupancy & 1 != 0 {
        world.files.push(file(&root_slot, &root, 10, 1, 30));
    }
    if occupancy & 2 != 0 {
        world.files.push(file(&a_base, &a, 11, 2, 40));
    }
    if occupancy & 4 != 0 {
        world.files.push(file(&b_base, &b, 12, 3, 50));
    }
    let source = world.snapshot(&root);
    let mut batches: Vec<(Vec<Change>, ExhaustiveWorld)> = Vec::new();
    match scenario {
        0 => {
            let mut added = file(&root_slot, &root, 100, 1, 35);
            added.parent_is_leaf = false;
            world.files.retain(|f| f.path != Path::new(&root_slot));
            world.files.push(added.clone());
            batches.push((vec![Change::UpsertFile(added)], world.clone()));
            let mut moved = file(&root_to_a, &a, 100, 2, 35);
            moved.parent_is_leaf = shape == 0;
            world.files.retain(|f| f.path != Path::new(&root_slot));
            world.files.push(moved.clone());
            batches.push((
                vec![Change::MoveFile {
                    from: root_slot.into(),
                    identity: id(100),
                    to: moved,
                }],
                world.clone(),
            ));
        }
        1 => {
            if occupancy & 2 != 0 {
                world.files.retain(|f| f.path != Path::new(&a_base));
                batches.push((
                    vec![Change::RemoveFile {
                        path: a_base.clone().into(),
                        identity: id(11),
                    }],
                    world.clone(),
                ));
                let mut entry = file(&a_base, &a, 101, 2, 43);
                entry.parent_is_leaf = shape == 0;
                world.files.push(entry.clone());
                batches.push((vec![Change::UpsertFile(entry)], world.clone()));
            } else {
                let mut entry = file(&a_base, &a, 101, 2, 43);
                entry.parent_is_leaf = shape == 0;
                world.files.push(entry.clone());
                batches.push((vec![Change::UpsertFile(entry)], world.clone()));
                world.files.retain(|f| f.path != Path::new(&a_base));
                batches.push((
                    vec![Change::RemoveFile {
                        path: a_base.clone().into(),
                        identity: id(101),
                    }],
                    world.clone(),
                ));
            }
        }
        2 => {
            if shape == 0 {
                world
                    .directories
                    .iter_mut()
                    .find(|d| d.path == Path::new(&a))
                    .unwrap()
                    .has_subdirectories = true;
                world.directories.push(fact(&empty, 4, false));
                batches.push((
                    vec![
                        Change::UpsertDirectory(fact(&a, 2, true)),
                        Change::UpsertDirectory(fact(&empty, 4, false)),
                    ],
                    world.clone(),
                ));
                world.directories.retain(|d| d.path != Path::new(&empty));
                world
                    .directories
                    .iter_mut()
                    .find(|d| d.path == Path::new(&a))
                    .unwrap()
                    .has_subdirectories = false;
                batches.push((
                    vec![
                        Change::RemoveDirectory {
                            path: empty.into(),
                            identity: id(4),
                        },
                        Change::UpsertDirectory(fact(&a, 2, false)),
                    ],
                    world.clone(),
                ));
            } else {
                world.directories.retain(|d| d.path != Path::new(&empty));
                world
                    .directories
                    .iter_mut()
                    .find(|d| d.path == Path::new(&a))
                    .unwrap()
                    .has_subdirectories = false;
                batches.push((
                    vec![
                        Change::RemoveDirectory {
                            path: empty.clone().into(),
                            identity: id(4),
                        },
                        Change::UpsertDirectory(fact(&a, 2, false)),
                    ],
                    world.clone(),
                ));
                let mut child = fact(&empty, 5, false);
                child.enumeration_complete = shape != 2;
                world.directories.push(child.clone());
                world
                    .directories
                    .iter_mut()
                    .find(|d| d.path == Path::new(&a))
                    .unwrap()
                    .has_subdirectories = true;
                batches.push((
                    vec![
                        Change::UpsertDirectory(fact(&a, 2, true)),
                        Change::UpsertDirectory(child),
                    ],
                    world.clone(),
                ));
            }
        }
        _ => {
            if occupancy & 4 == 0 {
                let entry = file(&b_base, &b, 102, 3, 55);
                world.files.push(entry.clone());
                batches.push((vec![Change::UpsertFile(entry)], world.clone()));
            }
            let old_id = if occupancy & 4 == 0 { 102 } else { 12 };
            let mut moved = file(
                &b_to_a,
                &a,
                old_id,
                2,
                if occupancy & 4 == 0 { 55 } else { 50 },
            );
            moved.parent_is_leaf = shape == 0;
            world.files.retain(|f| f.path != Path::new(&b_base));
            world.files.push(moved.clone());
            batches.push((
                vec![Change::MoveFile {
                    from: b_base.into(),
                    identity: id(old_id),
                    to: moved,
                }],
                world.clone(),
            ));
        }
    }
    let coordinator = Coordinator::start(source, 300 + case as u64, egui::Context::default());
    let Event::Ready(mut base) = receive(&coordinator) else {
        panic!("case {case}: ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 48.,
        h: 32.,
    };
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        holes: Arc<Vec<Rect>>,
        pixels: Vec<eframe::egui::Color32>,
    }
    let mut states = Vec::new();
    for (index, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("case {case}: export")
        };
        let mut groups: BTreeMap<&Path, Vec<(usize, u64)>> = BTreeMap::new();
        for (slot, entry) in data.files.iter().enumerate() {
            groups
                .entry(entry.parent.as_ref())
                .or_default()
                .push((slot, entry.size.max(1)));
        }
        let layout =
            directory_map::layout_with_partitions(groups, Path::new(&root), bounds, 1., &token, 1)
                .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [48, 32],
            &token,
            1,
        )
        .unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let texture = TextureId::Managed(300 + (case * 3 + index) as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(
            matches!(receive(&coordinator), Event::ModeReady(_)),
            "case {case}: mode ready"
        );
        states.push(State {
            mode,
            texture,
            map,
            holes: Arc::default(),
            pixels,
        });
    }
    for (step, (changes, expected_world)) in batches.into_iter().enumerate() {
        let description = format!("{changes:?}");
        let get = |counter: &std::sync::Mutex<BTreeMap<PathBuf, usize>>| {
            counter
                .lock()
                .unwrap()
                .get(Path::new(&root))
                .copied()
                .unwrap_or(0)
        };
        let counts = (
            get(&crate::incremental_update::ROOT_FALLBACKS),
            get(&crate::incremental_update::GENERAL_FALLBACKS),
        );
        let modes = states
            .iter()
            .map(|s| ModeBase {
                mode: s.mode,
                texture: s.texture,
                layout_generation: 1,
                map: s.map.snapshot_for_update(),
                holes: s.holes.clone(),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 256,
            })
            .collect();
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(changes),
                modes,
            }))
            .ok()
            .unwrap();
        let mut prepared = match receive(&coordinator) {
            Event::Prepared(value) => value,
            Event::Failed(failure) => panic!("case {case} step {step}, {description}: {failure:?}"),
            _ => panic!("case {case}: event"),
        };
        assert!(
            prepared.rebuild.is_empty(),
            "case {case} step {step}, {description}: {:?}",
            prepared.rebuild
        );
        assert_eq!(
            (
                get(&crate::incremental_update::ROOT_FALLBACKS),
                get(&crate::incremental_update::GENERAL_FALLBACKS)
            ),
            counts,
            "case {case} step {step}, {description}: fallback"
        );
        let expected_scene = Scene::from_scan(expected_world.snapshot(&root));
        for state in &mut states {
            let i = prepared
                .views
                .iter()
                .position(|view| view.mode == state.mode)
                .unwrap();
            let view = prepared.views.remove(i);
            let details = |entry: &scan::FileEntry| {
                (
                    entry.path.clone(),
                    entry.size,
                    entry.parent.to_path_buf(),
                    entry.identity,
                    entry.parent_identity,
                    entry.parent_is_leaf,
                    entry.can_trash,
                )
            };
            let mut expected: Vec<_> = expected_scene
                .project(state.mode)
                .into_iter()
                .map(|t| details(&t.to_entry()))
                .collect();
            let mut actual: Vec<_> = (0..view.projection.len())
                .filter_map(|i| view.projection.get(i))
                .map(details)
                .collect();
            expected.sort_by(|a, b| a.0.cmp(&b.0));
            actual.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                actual, expected,
                "case {case} step {step}, {description}, {:?}: projection",
                state.mode
            );
            check_frame_against_reference(&view, &mut state.pixels, [48, 32]);
            state.map = view.map;
            state.holes = view.holes;
        }
        coordinator
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        base = prepared.version;
    }
}

#[test]
fn bounded_small_tree_transactions_match_fresh_projection_and_raster() {
    let mut case = 0;
    for occupancy in 0..8 {
        for shape in 0..3 {
            for scenario in 0..4 {
                check_exhaustive_case(case, occupancy, shape, scenario);
                case += 1;
            }
        }
    }
}

#[test]
fn empty_root_replacement_then_move_delete_recreate_matches_fresh_scene() {
    use crate::scene_model::Scene;
    let root = "/exhaustive-root-replacement";
    let sub = "/exhaustive-root-replacement/a";
    let inside = "/exhaustive-root-replacement/a/f";
    let outside = "/exhaustive-root-replacement/f";
    let mut world = ExhaustiveWorld {
        files: vec![],
        directories: vec![fact(root, 1, false)],
    };
    let source = world.snapshot(root);
    world.directories = vec![fact(root, 2, true), fact(sub, 3, false)];
    let initial_file = file(inside, sub, 4, 3, 40);
    world.files.push(initial_file.clone());
    let mut batches = vec![(
        vec![
            Change::UpsertDirectory(fact(root, 2, true)),
            Change::UpsertDirectory(fact(sub, 3, false)),
            Change::UpsertFile(initial_file),
        ],
        world.clone(),
    )];
    let mut moved = file(outside, root, 4, 2, 40);
    moved.parent_is_leaf = false;
    world.files = vec![moved.clone()];
    batches.push((
        vec![Change::MoveFile {
            from: inside.into(),
            identity: id(4),
            to: moved,
        }],
        world.clone(),
    ));
    batches.push((
        vec![Change::UpsertDirectory(fact(root, 2, true))],
        world.clone(),
    ));
    world.files.clear();
    batches.push((
        vec![Change::RemoveFile {
            path: outside.into(),
            identity: id(4),
        }],
        world.clone(),
    ));
    let mut replacement = file(outside, root, 5, 2, 55);
    replacement.parent_is_leaf = false;
    world.files.push(replacement.clone());
    batches.push((vec![Change::UpsertFile(replacement)], world));
    let coordinator = Coordinator::start(source, 600, egui::Context::default());
    let Event::Ready(mut base) = receive(&coordinator) else {
        panic!("ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 64.,
        h: 48.,
    };
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        holes: Arc<Vec<Rect>>,
        pixels: Vec<eframe::egui::Color32>,
    }
    let mut states = Vec::new();
    for (i, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("export")
        };
        let layout = directory_map::layout_with_partitions(
            std::iter::empty::<(&Path, Vec<(usize, u64)>)>(),
            Path::new(root),
            bounds,
            1.,
            &token,
            1,
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [64, 48],
            &token,
            1,
        )
        .unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let texture = TextureId::Managed(600 + i as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
        states.push(State {
            mode,
            texture,
            map,
            holes: Arc::default(),
            pixels,
        });
    }
    for (step, (changes, expected_world)) in batches.into_iter().enumerate() {
        let get = |counter: &std::sync::Mutex<BTreeMap<PathBuf, usize>>| {
            counter
                .lock()
                .unwrap()
                .get(Path::new(root))
                .copied()
                .unwrap_or(0)
        };
        let counts = (
            get(&crate::incremental_update::ROOT_FALLBACKS),
            get(&crate::incremental_update::GENERAL_FALLBACKS),
        );
        let modes = states
            .iter()
            .map(|s| ModeBase {
                mode: s.mode,
                texture: s.texture,
                layout_generation: 1,
                map: s.map.snapshot_for_update(),
                holes: s.holes.clone(),
                pixels_per_point: 1.,
                area_tolerance: 10000.,
                max_patch_pixels: 256,
            })
            .collect();
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(changes),
                modes,
            }))
            .ok()
            .unwrap();
        let mut prepared = match receive(&coordinator) {
            Event::Prepared(value) => value,
            Event::Failed(failure) => panic!("step {step}: {failure:?}"),
            _ => panic!("event"),
        };
        assert!(
            prepared.rebuild.is_empty(),
            "step {step}: {:?}",
            prepared.rebuild
        );
        assert_eq!(
            (
                get(&crate::incremental_update::ROOT_FALLBACKS),
                get(&crate::incremental_update::GENERAL_FALLBACKS)
            ),
            counts,
            "step {step}: fallback"
        );
        let expected_scene = Scene::from_scan(expected_world.snapshot(root));
        for state in &mut states {
            let i = prepared
                .views
                .iter()
                .position(|view| view.mode == state.mode)
                .unwrap();
            let view = prepared.views.remove(i);
            let details = |entry: &scan::FileEntry| {
                (
                    entry.path.clone(),
                    entry.size,
                    entry.parent.to_path_buf(),
                    entry.identity,
                    entry.parent_identity,
                    entry.parent_is_leaf,
                    entry.can_trash,
                )
            };
            let mut expected: Vec<_> = expected_scene
                .project(state.mode)
                .into_iter()
                .map(|t| details(&t.to_entry()))
                .collect();
            let mut actual: Vec<_> = (0..view.projection.len())
                .filter_map(|i| view.projection.get(i))
                .map(details)
                .collect();
            expected.sort_by(|a, b| a.0.cmp(&b.0));
            actual.sort_by(|a, b| a.0.cmp(&b.0));
            assert_eq!(
                actual, expected,
                "step {step}, {:?}: projection",
                state.mode
            );
            check_frame_against_reference(&view, &mut state.pixels, [64, 48]);
            state.map = view.map;
            state.holes = view.holes;
        }
        coordinator
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        base = prepared.version;
    }
}

#[test]
fn empty_root_replacement_with_missing_intermediate_facts_preserves_scene_and_protection() {
    use crate::scene_model::Scene;
    let root = "/replacement-missing-ancestor";
    let deep = "/replacement-missing-ancestor/unknown/deep";
    let path = "/replacement-missing-ancestor/unknown/deep/f";
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![],
        directories: vec![fact(root, 1, false)].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let mut partial_fact = fact(deep, 4, false);
    partial_fact.enumeration_complete = false;
    let new_file = file(path, deep, 5, 4, 55);
    let expected_scan = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![new_file.clone()],
        directories: vec![fact(root, 2, true), partial_fact.clone()].into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let expected_scene = Scene::from_scan(expected_scan);
    let coordinator = Coordinator::start(source, 190, egui::Context::default());
    let Event::Ready(base) = receive(&coordinator) else {
        panic!("ready")
    };
    let token = AtomicU64::new(1);
    let bounds = Rect {
        x: 0.,
        y: 0.,
        w: 80.,
        h: 60.,
    };
    struct State {
        mode: View,
        texture: TextureId,
        map: RenderMap,
        pixels: Vec<eframe::egui::Color32>,
    }
    let mut states = Vec::new();
    for (i, mode) in [View::Files, View::Folders, View::Hybrid]
        .into_iter()
        .enumerate()
    {
        coordinator
            .try_send(Command::ExportMode { base, mode })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            data, source_ids, ..
        } = receive(&coordinator)
        else {
            panic!("initial export")
        };
        let layout = directory_map::layout_with_partitions(
            std::iter::empty::<(&Path, Vec<(usize, u64)>)>(),
            Path::new(root),
            bounds,
            1.,
            &token,
            1,
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [80, 60],
            &token,
            1,
        )
        .unwrap();
        let pixels = map.image.as_ref().unwrap().pixels.clone();
        let texture = TextureId::Managed(190 + i as u64);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode,
                data,
                source_ids,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::default(),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(receive(&coordinator), Event::ModeReady(_)));
        states.push(State {
            mode,
            texture,
            map,
            pixels,
        });
    }
    let get_count = |counter: &std::sync::Mutex<BTreeMap<PathBuf, usize>>| {
        counter
            .lock()
            .unwrap()
            .get(Path::new(root))
            .copied()
            .unwrap_or(0)
    };
    let root_before = get_count(&crate::incremental_update::ROOT_FALLBACKS);
    let general_before = get_count(&crate::incremental_update::GENERAL_FALLBACKS);
    let changes = vec![
        Change::UpsertDirectory(fact(root, 2, true)),
        Change::UpsertDirectory(partial_fact),
        Change::UpsertFile(new_file),
    ];
    let modes = states
        .iter()
        .map(|state| ModeBase {
            mode: state.mode,
            layout_generation: 1,
            texture: state.texture,
            map: state.map.snapshot_for_update(),
            holes: Arc::default(),
            pixels_per_point: 1.,
            area_tolerance: 10000.,
            max_patch_pixels: 256,
        })
        .collect();
    coordinator
        .try_send(Command::Update(UpdateInput {
            base,
            changes: Changes::Explicit(changes),
            modes,
        }))
        .ok()
        .unwrap();
    let mut prepared = match receive(&coordinator) {
        Event::Prepared(prepared) => prepared,
        Event::Failed(failure) => panic!("replacement failed: {failure:?}"),
        _ => panic!("prepared"),
    };
    assert!(
        prepared.rebuild.is_empty(),
        "rebuild: {:?}",
        prepared.rebuild
    );
    assert_eq!(
        get_count(&crate::incremental_update::ROOT_FALLBACKS),
        root_before
    );
    assert_eq!(
        get_count(&crate::incremental_update::GENERAL_FALLBACKS),
        general_before
    );
    for state in &mut states {
        let i = prepared
            .views
            .iter()
            .position(|view| view.mode == state.mode)
            .unwrap();
        let view = prepared.views.remove(i);
        let details = |entry: &scan::FileEntry| {
            (
                entry.path.clone(),
                entry.size,
                entry.parent.to_path_buf(),
                entry.identity,
                entry.parent_identity,
                entry.parent_is_leaf,
                entry.can_trash,
            )
        };
        let mut expected: Vec<_> = expected_scene
            .project(state.mode)
            .into_iter()
            .map(|t| details(&t.to_entry()))
            .collect();
        let mut actual: Vec<_> = (0..view.projection.len())
            .filter_map(|i| view.projection.get(i))
            .map(details)
            .collect();
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        actual.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            actual, expected,
            "mode {:?}: projection/protection",
            state.mode
        );
        if state.mode == View::Folders {
            assert!(
                !actual[0].6,
                "unreadable descendant folder must not be deletable"
            );
        }
        check_frame_against_reference(&view, &mut state.pixels, [80, 60]);
    }
}
