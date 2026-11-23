//! Group independent root-child changes without splitting a moved tile.
//!
//! The groups are prepared against private successive scene versions and must
//! be published together. A move joining two root children connects both
//! children; its before/after values must never be applied separately because
//! display slots use generational identities and retain tombstones.

use crate::{
    incremental_geometry::IncrementalGeometry, scene_actor::TileDelta, scene_model::TileId,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

pub struct RootChangeGroup {
    pub scope: PathBuf,
    pub deltas: Vec<TileDelta>,
}

/// Every directory fact touched by a transaction, including empty
/// directories without a visible tile, must belong to one represented
/// component. A changed root fact is shared by all components and therefore
/// prevents independent preparation.
pub fn covers_directory_facts(
    root: &Path,
    groups: &[RootChangeGroup],
    structural_paths: &[PathBuf],
    root_facts_touched: bool,
) -> bool {
    if root_facts_touched || groups.len() < 2 {
        return false;
    }
    let mut owner = BTreeMap::<PathBuf, usize>::new();
    for (index, group) in groups.iter().enumerate() {
        for tile in group
            .deltas
            .iter()
            .flat_map(|delta| delta.before.iter().chain(delta.after.iter()))
        {
            let Ok(relative) = tile.path.strip_prefix(root) else {
                return false;
            };
            let Some(first) = relative.components().next() else {
                return false;
            };
            let pivot = root.join(first.as_os_str());
            if let Some(previous) = owner.insert(pivot, index)
                && previous != index
            {
                return false;
            }
        }
    }
    structural_paths.iter().all(|path| {
        let Ok(relative) = path.strip_prefix(root) else {
            return false;
        };
        let Some(first) = relative.components().next() else {
            return false;
        };
        owner.contains_key(&root.join(first.as_os_str()))
    })
}

/// As above, but allows proven nonvisual empty-leaf facts and root metadata
/// changes when the root's slot and filesystem identity are unchanged and no
/// root aggregate changed in this view. The root itself draws no platform;
/// changes to a visible root aggregate always appear in the supplied deltas.
pub fn covers_directory_facts_relaxed(
    root: &Path,
    groups: &[RootChangeGroup],
    structural_paths: &[PathBuf],
    root_identity_stable: bool,
    visual_noop_paths: &[PathBuf],
) -> bool {
    if !root_identity_stable || groups.len() < 2 {
        return false;
    }
    if groups
        .iter()
        .flat_map(|g| &g.deltas)
        .flat_map(|d| d.before.iter().chain(d.after.iter()))
        .any(|tile| tile.path == root)
    {
        return false;
    }
    let noops: HashSet<_> = visual_noop_paths.iter().map(PathBuf::as_path).collect();
    let filtered: Vec<_> = structural_paths
        .iter()
        .filter(|path| path.as_path() != root && !noops.contains(path.as_path()))
        .cloned()
        .collect();
    covers_directory_facts(root, groups, &filtered, false)
}

/// Ordinary geometry updates are safe only if a caller has independently
/// established that no directory facts changed in this component. In
/// particular, an empty subdirectory may change platform topology without
/// producing any projected tile delta at all. Every affected parent must have
/// at least one direct tile both before and after the transaction; otherwise
/// its unary/platform state can change and requires structural processing.
pub fn can_prepare_files(
    geometry: &IncrementalGeometry,
    deltas: &[TileDelta],
    topology_unchanged: bool,
) -> bool {
    if !topology_unchanged || deltas.is_empty() {
        return false;
    }
    let mut counts = BTreeMap::<PathBuf, (usize, usize)>::new();
    for delta in deltas {
        if delta.before.is_none() && delta.after.is_none() {
            return false;
        }
        if let Some(tile) = &delta.before {
            if !matches!(tile.id, TileId::File(_)) {
                return false;
            }
            let value = &mut counts.entry(tile.parent.clone()).or_default().0;
            *value = value.saturating_add(1);
        }
        if let Some(tile) = &delta.after {
            if !matches!(tile.id, TileId::File(_)) {
                return false;
            }
            let value = &mut counts.entry(tile.parent.clone()).or_default().1;
            *value = value.saturating_add(1);
        }
    }
    counts.into_iter().all(|(path, (removed, added))| {
        let Some(before) = geometry.direct_tile_count_up_to(&path, removed) else {
            return false;
        };
        before > 0 && before.saturating_add(added) > removed
    })
}

/// Return independent connected components only when there are at least two.
/// A root tile, out-of-root path, duplicate before/after identity, or empty
/// delta cannot be decomposed safely and is left to the general transaction.
pub fn independent_root_changes(root: &Path, deltas: &[TileDelta]) -> Option<Vec<RootChangeGroup>> {
    let mut pivots = BTreeMap::<PathBuf, usize>::new();
    let mut per_delta = Vec::with_capacity(deltas.len());
    let mut seen_before = HashSet::new();
    let mut seen_after = HashSet::new();
    let mut id_owner = HashMap::new();
    for (delta_index, delta) in deltas.iter().enumerate() {
        if delta.before.is_none() && delta.after.is_none() {
            return None;
        }
        if delta
            .before
            .as_ref()
            .is_some_and(|tile| !seen_before.insert(tile.id))
            || delta
                .after
                .as_ref()
                .is_some_and(|tile| !seen_after.insert(tile.id))
        {
            return None;
        }
        let mut members = Vec::with_capacity(2);
        for tile in delta.before.iter().chain(delta.after.iter()) {
            if let Some(owner) = id_owner.insert(tile.id, delta_index)
                && owner != delta_index
            {
                return None;
            }
            if !tile.parent.starts_with(root) {
                return None;
            }
            let relative = tile.path.strip_prefix(root).ok()?;
            let first = relative.components().next()?;
            let pivot = root.join(first.as_os_str());
            let next_index = pivots.len();
            let index = *pivots.entry(pivot).or_insert(next_index);
            members.push(index);
        }
        per_delta.push(members);
    }
    let mut parents: Vec<usize> = (0..pivots.len()).collect();
    fn representative(parents: &mut [usize], mut index: usize) -> usize {
        while parents[index] != index {
            parents[index] = parents[parents[index]];
            index = parents[index];
        }
        index
    }
    for members in &per_delta {
        let first = representative(&mut parents, members[0]);
        for &index in &members[1..] {
            let other = representative(&mut parents, index);
            if first != other {
                parents[other] = first;
            }
        }
    }
    let mut component_key = HashMap::<usize, PathBuf>::new();
    for (path, &index) in &pivots {
        let rep = representative(&mut parents, index);
        component_key.entry(rep).or_insert_with(|| path.clone());
    }
    if component_key.len() < 2 {
        return None;
    }
    let mut groups = BTreeMap::<PathBuf, RootChangeGroup>::new();
    for (delta, members) in deltas.iter().zip(per_delta) {
        let rep = representative(&mut parents, members[0]);
        let key = component_key.get(&rep)?.clone();
        let initial = delta
            .before
            .as_ref()
            .or(delta.after.as_ref())?
            .parent
            .clone();
        let group = groups.entry(key).or_insert_with(|| RootChangeGroup {
            scope: initial,
            deltas: Vec::new(),
        });
        for tile in delta.before.iter().chain(delta.after.iter()) {
            while !tile.parent.starts_with(&group.scope) {
                group.scope = group.scope.parent()?.to_path_buf();
                if !group.scope.starts_with(root) {
                    return None;
                }
            }
        }
        group.deltas.push(delta.clone());
    }
    Some(groups.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        scan::FileIdentity,
        scene_model::{EntryId, Tile, TileId},
    };

    fn tile(slot: u32, path: &str) -> Tile {
        let path = PathBuf::from(path);
        Tile {
            id: TileId::File(EntryId {
                slot,
                generation: 0,
            }),
            parent: path.parent().unwrap().into(),
            path,
            size: 1,
            identity: FileIdentity::default(),
            parent_identity: FileIdentity::default(),
            parent_is_leaf: false,
            can_trash: true,
        }
    }

    #[test]
    fn cross_subtree_move_stays_atomic_while_unrelated_changes_split() {
        let changes = vec![
            TileDelta {
                before: Some(tile(1, "/r/a/f")),
                after: Some(tile(1, "/r/b/f")),
            },
            TileDelta {
                before: Some(tile(2, "/r/a/old")),
                after: None,
            },
            TileDelta {
                before: None,
                after: Some(tile(3, "/r/c/new")),
            },
        ];
        let groups = independent_root_changes(Path::new("/r"), &changes).unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].scope, Path::new("/r"));
        assert_eq!(groups[1].scope, Path::new("/r/c"));
        assert!(covers_directory_facts(
            Path::new("/r"),
            &groups,
            &[PathBuf::from("/r/a/empty")],
            false
        ));
        assert!(!covers_directory_facts(
            Path::new("/r"),
            &groups,
            &[PathBuf::from("/r/unrepresented/empty")],
            false
        ));
        assert!(!covers_directory_facts(
            Path::new("/r"),
            &groups,
            &[PathBuf::from("/r")],
            false
        ));
        assert!(!covers_directory_facts(Path::new("/r"), &groups, &[], true));
        let empty = PathBuf::from("/r/unrepresented/empty");
        assert!(covers_directory_facts_relaxed(
            Path::new("/r"),
            &groups,
            &[PathBuf::from("/r"), empty.clone()],
            true,
            &[empty.clone()]
        ));
        assert!(!covers_directory_facts_relaxed(
            Path::new("/r"),
            &groups,
            &[empty.clone()],
            false,
            &[empty.clone()]
        ));
        assert!(!covers_directory_facts_relaxed(
            Path::new("/r"),
            &groups,
            &[empty.clone()],
            true,
            &[]
        ));
        assert_eq!(groups[0].deltas.len(), 2);
        assert_eq!(
            groups[0].deltas[0].before.as_ref().unwrap().id,
            groups[0].deltas[0].after.as_ref().unwrap().id
        );
        assert_eq!(
            groups[1].deltas[0].after.as_ref().unwrap().path,
            Path::new("/r/c/new")
        );
    }

    #[test]
    fn connected_moves_do_not_split_and_invalid_paths_are_rejected() {
        let moves = vec![
            TileDelta {
                before: Some(tile(1, "/r/a/f")),
                after: Some(tile(1, "/r/b/f")),
            },
            TileDelta {
                before: Some(tile(2, "/r/b/g")),
                after: Some(tile(2, "/r/c/g")),
            },
        ];
        assert!(independent_root_changes(Path::new("/r"), &moves).is_none());
        let outside = vec![TileDelta {
            before: None,
            after: Some(tile(3, "/other/x")),
        }];
        assert!(independent_root_changes(Path::new("/r"), &outside).is_none());
        let detached_move = vec![
            TileDelta {
                before: Some(tile(4, "/r/a/f")),
                after: None,
            },
            TileDelta {
                before: None,
                after: Some(tile(4, "/r/b/f")),
            },
        ];
        assert!(independent_root_changes(Path::new("/r"), &detached_move).is_none());
    }

    #[test]
    fn file_move_requires_known_unchanged_directory_topology_and_nonempty_parents() {
        use crate::{directory_map, treemap::Rect};
        use std::sync::atomic::AtomicU64;
        let ids = [
            tile(1, "/r/a/f").id,
            tile(2, "/r/a/g").id,
            tile(3, "/r/b/h").id,
        ];
        let groups = [
            (Path::new("/r/a"), vec![(0, 1), (1, 1)]),
            (Path::new("/r/b"), vec![(2, 1)]),
        ];
        let token = AtomicU64::new(1);
        let layout = directory_map::layout_with_partitions(
            groups,
            Path::new("/r"),
            Rect {
                x: 0.0,
                y: 0.0,
                w: 120.0,
                h: 100.0,
            },
            1.0,
            &token,
            1,
        )
        .unwrap();
        let geometry = IncrementalGeometry::from_layout(&layout.partitions, &ids).unwrap();
        let moved = TileDelta {
            before: Some(tile(1, "/r/a/f")),
            after: Some(tile(1, "/r/b/f")),
        };
        assert!(!can_prepare_files(&geometry, &[moved.clone()], false));
        assert!(can_prepare_files(&geometry, &[moved], true));
        let last = TileDelta {
            before: Some(tile(3, "/r/b/h")),
            after: Some(tile(3, "/r/a/h")),
        };
        assert!(!can_prepare_files(&geometry, &[last], true));
        let missing = TileDelta {
            before: Some(tile(1, "/r/a/f")),
            after: Some(tile(1, "/r/new/f")),
        };
        assert!(!can_prepare_files(&geometry, &[missing], true));
    }
}
