//! Integration checks for a scan root that is itself the filesystem root.
use crate::{
    directory_map,
    incremental_geometry::IncrementalGeometry,
    render::{self, RenderMap},
    root_transaction, scan,
    scene_actor::TileDelta,
    scene_model::{Scene, TileId},
    sparse_projection::SparseProjection,
    treemap::Rect,
};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

#[test]
fn root_content_can_be_created_beside_aggregate_without_mutating_snapshot() {
    let root = Path::new("/disk");
    let token = AtomicU64::new(1);
    let file = scan::FileEntry {
        path: root.join("f"),
        parent: Arc::from(root),
        size: 50,
        identity: Default::default(),
        parent_identity: Default::default(),
        parent_is_leaf: true,
        can_trash: true,
    };
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![file],
        directories: Vec::new().into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let scene = Scene::from_scan(source);
    let folder = scene
        .tile(TileId::Directory(scene.directory_id(root).unwrap()))
        .unwrap();
    let bounds = Rect {
        x: 0.0,
        y: 0.0,
        w: 300.0,
        h: 200.0,
    };
    let layout = directory_map::layout_with_partitions(
        [(Path::new("/"), vec![(0, 50)])],
        root,
        bounds,
        1.0,
        &token,
        1,
    )
    .unwrap();
    let old = IncrementalGeometry::from_layout(&layout.partitions, &[folder.id]).unwrap();
    assert!(old.scan_root_frame(root).is_none());
    let (staged, frame, donor) = old
        .bootstrap_root_beside_aggregate(root, folder.id, 40)
        .unwrap()
        .unwrap();
    let (old_frame, aggregate_frame) = donor.unwrap();
    assert_eq!(old_frame, bounds);
    assert_eq!(staged.scan_root_frame(root), Some(frame));
    assert!(old.scan_root_frame(root).is_none());
    assert!(
        (frame.w * frame.h + aggregate_frame.w * aggregate_frame.h - bounds.w * bounds.h).abs()
            < 0.1
    );
    let path = root.join("a");
    let (staged, assigned, new_donor) = staged.allocate_root_child(root, &path, None, 40).unwrap();
    assert_eq!(assigned, frame);
    assert!(new_donor.is_none());
    assert_eq!(staged.root_fold_frame(root, &path, None), Some(frame));
}

#[test]
fn filesystem_root_aggregate_and_child_changes_are_atomic() {
    for root in ["/", "/disk"] {
        root_aggregate_and_child_changes_are_atomic(root);
    }
}

fn root_aggregate_and_child_changes_are_atomic(root: &str) {
    let token = AtomicU64::new(1);
    let root = Path::new(root);
    let file = |path: &Path, parent: &Path, size| scan::FileEntry {
        path: path.into(),
        parent: Arc::from(parent),
        size,
        identity: Default::default(),
        parent_identity: Default::default(),
        parent_is_leaf: true,
        can_trash: true,
    };
    let source = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![
            file(&root.join("root-file"), root, 50),
            file(&root.join("a/f"), &root.join("a"), 100),
            file(&root.join("b/g"), &root.join("b"), 10),
        ],
        directories: Vec::new().into(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let scene = Scene::from_scan(source.clone());
    let root_aggregate = scene
        .tile(TileId::Directory(scene.directory_id(root).unwrap()))
        .unwrap();
    let a = scene
        .tile(TileId::File(scene.file_id(&root.join("a/f")).unwrap()))
        .unwrap();
    let b = scene
        .tile(TileId::File(scene.file_id(&root.join("b/g")).unwrap()))
        .unwrap();
    let mut changed_a = a.clone();
    changed_a.size += 20;
    let projected = Arc::new(scan::ScanResult {
        root: root.into(),
        files: vec![a.to_entry(), b.to_entry()],
        directories: source.directories.clone(),
        skipped: 0,
        errors: vec![],
        elapsed: Duration::ZERO,
    });
    let old = SparseProjection::new(projected, vec![a.id, b.id]).unwrap();
    let bounds = Rect {
        x: 0.0,
        y: 0.0,
        w: 300.0,
        h: 200.0,
    };
    let layout = directory_map::layout_with_partitions(
        [
            (a.parent.as_path(), vec![(0, 100)]),
            (b.parent.as_path(), vec![(1, 10)]),
        ],
        root,
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
        [300, 200],
        &token,
        1,
    )
    .unwrap();
    let neighbor = map.rect(1);
    let mut next = old.clone();
    next.replace(
        &[a.id],
        vec![
            (changed_a.id, changed_a.to_entry()),
            (root_aggregate.id, root_aggregate.to_entry()),
        ],
    )
    .unwrap();
    let deltas = [
        TileDelta {
            before: Some(a.clone()),
            after: Some(changed_a.clone()),
        },
        TileDelta {
            before: None,
            after: Some(root_aggregate.clone()),
        },
    ];
    let added = root_transaction::prepare(
        &geometry,
        &map,
        &old,
        &next,
        &[],
        &deltas,
        4096,
        &token,
        1,
        true,
    )
    .unwrap()
    .expect("root aggregate added locally");
    assert_eq!(added.map.rect(1), neighbor);
    assert!(
        added
            .map
            .rect(next.slot(root_aggregate.id).unwrap())
            .is_some()
    );
    assert_eq!(
        added
            .geometry
            .tile_layout_parent(root_aggregate.id, root.parent().unwrap_or(root)),
        root
    );
    check_pixels(&map, &added.map, &added.patches, bounds, &token);

    let mut bigger = root_aggregate.clone();
    bigger.size += 25;
    let mut next2 = next.clone();
    next2
        .replace(
            &[root_aggregate.id, changed_a.id],
            vec![(bigger.id, bigger.to_entry()), (a.id, a.to_entry())],
        )
        .unwrap();
    let deltas = [
        TileDelta {
            before: Some(root_aggregate.clone()),
            after: Some(bigger.clone()),
        },
        TileDelta {
            before: Some(changed_a.clone()),
            after: Some(a.clone()),
        },
    ];
    let changed = root_transaction::prepare(
        &added.geometry,
        &added.map,
        &next,
        &next2,
        &added.holes,
        &deltas,
        4096,
        &token,
        1,
        true,
    )
    .unwrap()
    .expect("root aggregate updated locally");
    assert_eq!(changed.map.rect(1), neighbor);
    check_pixels(&added.map, &changed.map, &changed.patches, bounds, &token);

    let mut next3 = next2.clone();
    next3
        .replace(
            &[bigger.id, a.id],
            vec![(changed_a.id, changed_a.to_entry())],
        )
        .unwrap();
    let deltas = [
        TileDelta {
            before: Some(bigger),
            after: None,
        },
        TileDelta {
            before: Some(a),
            after: Some(changed_a),
        },
    ];
    let removed = root_transaction::prepare(
        &changed.geometry,
        &changed.map,
        &next2,
        &next3,
        &changed.holes,
        &deltas,
        4096,
        &token,
        1,
        true,
    )
    .unwrap()
    .expect("root aggregate removed locally");
    assert_eq!(removed.map.rect(1), neighbor);
    assert!(
        removed
            .map
            .rect(next2.slot(root_aggregate.id).unwrap())
            .is_none()
    );
    check_pixels(&changed.map, &removed.map, &removed.patches, bounds, &token);
}

fn check_pixels(
    old: &RenderMap,
    next: &RenderMap,
    patches: &[render::RasterPatch],
    bounds: Rect,
    token: &AtomicU64,
) {
    let raster = |map: &RenderMap| {
        let entries: Vec<_> = map
            .entries()
            .iter()
            .copied()
            .filter(|entry| map.rect(entry.id).is_some())
            .collect();
        render::raster_patch(
            &entries,
            map.platforms(),
            bounds,
            [300, 200],
            [0, 0, 300, 200],
            token,
            1,
        )
        .unwrap()
    };
    let before = raster(old);
    let after = raster(next);
    let mut covered = vec![false; 300 * 200];
    for patch in patches {
        for y in 0..patch.image.size[1] {
            for x in 0..patch.image.size[0] {
                let gx = x + patch.origin[0];
                let gy = y + patch.origin[1];
                let i = gy * 300 + gx;
                covered[i] = true;
                assert_eq!(
                    patch.image.pixels[y * patch.image.size[0] + x],
                    after.image.pixels[i]
                );
                assert_eq!(patch.owner_at(x, y), after.owner_at(gx, gy));
            }
        }
    }
    for (i, marked) in covered.into_iter().enumerate() {
        if !marked {
            assert_eq!(before.image.pixels[i], after.image.pixels[i]);
            assert_eq!(
                before.owner_at(i % 300, i / 300),
                after.owner_at(i % 300, i / 300)
            );
        }
    }
}
