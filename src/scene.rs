//! Single, closed persistent scene tree and incremental layout engine.
//!
//! Invariant: `LayoutTree` always holds the scan root container with the full
//! canvas bounds. Every filesystem mutation (delete, add, resize, directory
//! collapse/expand, rescan diff, or FSEvents batch) walks up from dirty
//! directories to the nearest active container with fixed bounds (at worst root)
//! and executes the exact same `layout_subtree` function. There is no separate
//! full-map rebuild path and no fallback.
use crate::{
    file_actions,
    render::{self, LIGHT_REACH, Platform, RasterPatch, RenderEntry},
    scan::{self, DirectorySnapshot, FileIdentity, ScanResult},
    treemap::{self, Rect},
};
use eframe::egui::Color32;
use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct NodeId(pub u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    Files,
    Folders,
    #[default]
    Hybrid,
}

#[derive(Clone, Debug)]
pub struct TileInfo {
    pub id: NodeId,
    pub path: PathBuf,
    pub size: u64,
    pub identity: FileIdentity,
    pub is_folder: bool,
    pub can_trash: bool,
    pub rect: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionKind {
    Move,
    FadeIn,
    FadeOut,
}

#[derive(Clone, Copy, Debug)]
pub struct TransitionItem {
    pub from: Rect,
    pub to: Rect,
    pub color: Color32,
    pub kind: TransitionKind,
}

#[derive(Clone, Debug)]
pub struct TransitionBatch {
    pub dirty_rects: Vec<Rect>,
    pub items: Vec<TransitionItem>,
}

const MAX_ANIMATED_TILES: usize = 1024;

#[derive(Clone, Debug)]
struct FileNode {
    id: NodeId,
    name: Box<OsStr>,
    size: u64,
    identity: FileIdentity,
    can_trash: bool,
}

#[derive(Clone, Debug)]
struct DirNode {
    id: NodeId,
    path: PathBuf,
    parent: Option<usize>,
    identity: FileIdentity,
    enumeration_complete: bool,
    excluded: bool,
    files: Vec<FileNode>,
    subdirs: Vec<usize>,
    // Bottom-up cached aggregates.
    direct_file_bytes: u64,
    total_bytes: u64,
    total_files: usize,
    is_leaf: bool,
    direct_can_trash: bool,
}

impl DirNode {
    fn file_path(&self, file: &FileNode) -> PathBuf {
        self.path.join(&*file.name)
    }
}

pub struct SceneTree {
    pub root: PathBuf,
    pub real_root: PathBuf,
    #[cfg(test)]
    pub root_identity: FileIdentity,
    pub skipped: u64,
    pub errors: Vec<String>,
    pub elapsed: Duration,
    dirs: Vec<Option<DirNode>>,
    dir_by_path: HashMap<PathBuf, usize>,
    /// O(1) map from `NodeId.0` to owning `dir_idx` (or `u32::MAX` if retired).
    id_to_dir: Vec<u32>,
    next_id: u32,
}

impl SceneTree {
    fn alloc_id(&mut self, dir_idx: usize) -> NodeId {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).expect("NodeId space exhausted");
        if self.id_to_dir.len() <= id as usize {
            self.id_to_dir.resize(id as usize + 1, u32::MAX);
        }
        self.id_to_dir[id as usize] = dir_idx as u32;
        NodeId(id)
    }

    fn alloc_dir(
        &mut self,
        path: PathBuf,
        parent: Option<usize>,
        identity: FileIdentity,
        enumeration_complete: bool,
        excluded: bool,
    ) -> usize {
        let idx = self.dirs.len();
        let id = self.alloc_id(idx);
        let node = DirNode {
            id,
            path: path.clone(),
            parent,
            identity,
            enumeration_complete,
            excluded,
            files: Vec::new(),
            subdirs: Vec::new(),
            direct_file_bytes: 0,
            total_bytes: 0,
            total_files: 0,
            is_leaf: false,
            direct_can_trash: false,
        };
        self.dirs.push(Some(node));
        self.dir_by_path.insert(path, idx);
        if let Some(p) = parent {
            if let Some(parent_node) = self.dirs[p].as_mut() {
                if !parent_node.subdirs.contains(&idx) {
                    parent_node.subdirs.push(idx);
                }
            }
        }
        idx
    }

    pub fn from_scan(result: ScanResult) -> Self {
        let real_root = if result.root == Path::new(crate::demo::VIRTUAL_ROOT) {
            result.root.clone()
        } else {
            std::fs::canonicalize(&result.root).unwrap_or_else(|_| result.root.clone())
        };
        let root_fact = result
            .directories
            .iter()
            .find(|d| d.path == result.root)
            .cloned();
        let root_identity = root_fact.as_ref().map_or_else(
            || {
                if result.root == Path::new(crate::demo::VIRTUAL_ROOT) {
                    FileIdentity::default()
                } else {
                    std::fs::symlink_metadata(&result.root)
                        .ok()
                        .and_then(|m| FileIdentity::from_path(&result.root, &m).ok())
                        .unwrap_or_default()
                }
            },
            |f| f.identity,
        );

        let mut tree = Self {
            root: result.root.clone(),
            real_root,
            #[cfg(test)]
            root_identity,
            skipped: result.skipped,
            errors: result.errors,
            elapsed: result.elapsed,
            dirs: Vec::with_capacity(result.directories.len().max(1)),
            dir_by_path: HashMap::with_capacity(result.directories.len().max(1)),
            id_to_dir: Vec::with_capacity(result.files.len() + result.directories.len() + 16),
            next_id: 1,
        };

        // Ensure root is always index 0.
        tree.alloc_dir(
            result.root.clone(),
            None,
            root_identity,
            root_fact.as_ref().is_none_or(|f| f.enumeration_complete),
            root_fact.as_ref().is_some_and(|f| f.excluded),
        );

        // Sort directory facts by depth so parents exist before children.
        let mut facts: Vec<_> = result.directories.iter().cloned().collect();
        facts.sort_unstable_by(|a, b| {
            a.path
                .components()
                .count()
                .cmp(&b.path.components().count())
                .then_with(|| a.path.cmp(&b.path))
        });

        for fact in facts {
            if fact.path == tree.root {
                if let Some(root_node) = tree.dirs[0].as_mut() {
                    root_node.identity = fact.identity;
                    root_node.enumeration_complete = fact.enumeration_complete;
                    root_node.excluded = fact.excluded;
                }
                continue;
            }
            if !fact.path.starts_with(&tree.root) {
                continue;
            }
            let idx = tree.ensure_dir_chain(&fact.path);
            if let Some(dir) = tree.dirs[idx].as_mut() {
                dir.identity = fact.identity;
                dir.enumeration_complete = fact.enumeration_complete;
                dir.excluded = fact.excluded;
            }
        }

        // Insert all files into their parent directory nodes.
        for entry in result.files {
            if !entry.path.starts_with(&tree.root) {
                continue;
            }
            let parent_path = entry.parent.as_ref();
            let dir_idx = tree.ensure_dir_chain(parent_path);
            let name = entry
                .path
                .file_name()
                .unwrap_or_else(|| entry.path.as_os_str())
                .to_os_string()
                .into_boxed_os_str();
            let file_id = tree.alloc_id(dir_idx);
            if let Some(dir) = tree.dirs[dir_idx].as_mut() {
                if dir.identity == FileIdentity::default()
                    && entry.parent_identity != FileIdentity::default()
                {
                    dir.identity = entry.parent_identity;
                }
                if entry.parent_is_leaf && !dir.excluded {
                    dir.enumeration_complete = true;
                }
                dir.files.push(FileNode {
                    id: file_id,
                    name,
                    size: entry.size,
                    identity: entry.identity,
                    can_trash: entry.can_trash,
                });
            }
        }

        tree.recompute_all_aggregates();
        tree
    }

    fn ensure_dir_chain(&mut self, path: &Path) -> usize {
        if let Some(&idx) = self.dir_by_path.get(path) {
            return idx;
        }
        let mut missing = Vec::new();
        let mut cur = Some(path);
        let mut parent_idx = 0usize;
        while let Some(p) = cur {
            if let Some(&idx) = self.dir_by_path.get(p) {
                parent_idx = idx;
                break;
            }
            missing.push(p.to_path_buf());
            if p == self.root {
                break;
            }
            cur = p.parent();
        }
        for p in missing.into_iter().rev() {
            parent_idx = self.alloc_dir(p, Some(parent_idx), FileIdentity::default(), false, false);
        }
        parent_idx
    }

    fn recompute_dir_local(&mut self, idx: usize) {
        let Some(dir) = self.dirs[idx].as_ref() else {
            return;
        };
        let mut direct_bytes = 0u64;
        let mut all_files_trashable = true;
        for f in &dir.files {
            direct_bytes = direct_bytes.saturating_add(f.size);
            all_files_trashable &= f.can_trash;
        }
        let mut total_bytes = direct_bytes;
        let mut total_files = dir.files.len();
        for &child in &dir.subdirs {
            if let Some(c) = self.dirs[child].as_ref() {
                total_bytes = total_bytes.saturating_add(c.total_bytes);
                total_files = total_files.saturating_add(c.total_files);
            }
        }
        let is_root = idx == 0;
        let is_leaf = dir.enumeration_complete && !dir.excluded && dir.subdirs.is_empty();
        let direct_can_trash = !is_root
            && all_files_trashable
            && !dir.excluded
            && file_actions::can_move_target(&dir.path, file_actions::TargetKind::Directory);
        if let Some(dir) = self.dirs[idx].as_mut() {
            dir.direct_file_bytes = direct_bytes;
            dir.total_bytes = total_bytes;
            dir.total_files = total_files;
            dir.is_leaf = is_leaf;
            dir.direct_can_trash = direct_can_trash;
        }
    }

    fn recompute_all_aggregates(&mut self) {
        // Post-order traversal via iterative DFS so children update before parents.
        let mut order = Vec::with_capacity(self.dirs.len());
        let mut stack = vec![0usize];
        while let Some(idx) = stack.pop() {
            if let Some(dir) = self.dirs[idx].as_ref() {
                order.push(idx);
                for &child in &dir.subdirs {
                    stack.push(child);
                }
            }
        }
        for idx in order.into_iter().rev() {
            self.recompute_dir_local(idx);
        }
    }

    fn recompute_upwards(&mut self, start: usize) {
        let mut cur = Some(start);
        while let Some(idx) = cur {
            self.recompute_dir_local(idx);
            cur = self.dirs[idx].as_ref().and_then(|d| d.parent);
        }
    }

    pub fn total_files(&self) -> usize {
        self.dirs[0].as_ref().map_or(0, |d| d.total_files)
    }

    pub fn total_bytes(&self) -> u64 {
        self.dirs[0].as_ref().map_or(0, |d| d.total_bytes)
    }

    /// Remove a single file after Trash confirmation. Returns dirty dir indices.
    pub fn remove_file(&mut self, path: &Path, identity: FileIdentity) -> Vec<usize> {
        let Some(parent) = path.parent() else {
            return Vec::new();
        };
        let Some(&dir_idx) = self.dir_by_path.get(parent) else {
            return Vec::new();
        };
        let Some(name) = path.file_name() else {
            return Vec::new();
        };
        let removed_id = {
            let Some(dir) = self.dirs[dir_idx].as_mut() else {
                return Vec::new();
            };
            let Some(pos) = dir.files.iter().position(|f| {
                f.name.as_ref() == name
                    && (identity == FileIdentity::default() || f.identity == identity)
            }) else {
                return Vec::new();
            };
            let removed = dir.files.swap_remove(pos);
            removed.id
        };
        self.id_to_dir[removed_id.0 as usize] = u32::MAX;
        self.recompute_upwards(dir_idx);
        vec![dir_idx]
    }

    /// Remove an entire directory subtree after Trash confirmation.
    pub fn remove_directory(&mut self, path: &Path, identity: FileIdentity) -> Vec<usize> {
        let Some(&dir_idx) = self.dir_by_path.get(path) else {
            return Vec::new();
        };
        if dir_idx == 0 {
            return Vec::new();
        }
        let Some(dir) = self.dirs[dir_idx].as_ref() else {
            return Vec::new();
        };
        if identity != FileIdentity::default()
            && dir.identity != FileIdentity::default()
            && dir.identity != identity
        {
            return Vec::new();
        }
        let parent_idx = dir.parent;
        self.unlink_subtree(dir_idx);
        if let Some(p) = parent_idx {
            if let Some(parent_node) = self.dirs[p].as_mut() {
                parent_node.subdirs.retain(|&c| c != dir_idx);
            }
            self.recompute_upwards(p);
            vec![p]
        } else {
            vec![0]
        }
    }

    fn unlink_subtree(&mut self, root_idx: usize) {
        let mut stack = vec![root_idx];
        while let Some(idx) = stack.pop() {
            if let Some(node) = self.dirs[idx].take() {
                self.dir_by_path.remove(&node.path);
                self.id_to_dir[node.id.0 as usize] = u32::MAX;
                for f in &node.files {
                    self.id_to_dir[f.id.0 as usize] = u32::MAX;
                }
                for child in node.subdirs {
                    stack.push(child);
                }
            }
        }
    }

    /// Apply one shallow directory snapshot (from FSEvents incremental rescan).
    /// Returns `(dirty_dir_indices, new_subdirs_needing_scan)`.
    pub fn apply_dir_snapshot(
        &mut self,
        path: &Path,
        snapshot: Option<DirectorySnapshot>,
    ) -> (Vec<usize>, Vec<PathBuf>) {
        let Some(snapshot) = snapshot else {
            // Directory disappeared or became non-directory.
            if let Some(&idx) = self.dir_by_path.get(path) {
                if idx == 0 {
                    // Root vanished or became unreadable; clear children.
                    let children = self.dirs[0]
                        .as_ref()
                        .map(|d| d.subdirs.clone())
                        .unwrap_or_default();
                    for c in children {
                        self.unlink_subtree(c);
                    }
                    if let Some(root_node) = self.dirs[0].as_mut() {
                        for f in &root_node.files {
                            self.id_to_dir[f.id.0 as usize] = u32::MAX;
                        }
                        root_node.files.clear();
                        root_node.subdirs.clear();
                    }
                    self.recompute_dir_local(0);
                    return (vec![0], Vec::new());
                }
                let parent = self.dirs[idx].as_ref().and_then(|d| d.parent).unwrap_or(0);
                self.unlink_subtree(idx);
                if let Some(p) = self.dirs[parent].as_mut() {
                    p.subdirs.retain(|&c| c != idx);
                }
                self.recompute_upwards(parent);
                return (vec![parent], Vec::new());
            }
            return (Vec::new(), Vec::new());
        };

        if !path.starts_with(&self.root) {
            return (Vec::new(), Vec::new());
        }

        let dir_idx = self.ensure_dir_chain(path);
        let mut changed = false;
        let mut new_subdirs = Vec::new();

        // If directory is now positively excluded (e.g. moved into Trash), clear its contents.
        if snapshot.excluded {
            let old_children = self.dirs[dir_idx]
                .as_ref()
                .map(|d| d.subdirs.clone())
                .unwrap_or_default();
            for c in old_children {
                self.unlink_subtree(c);
                changed = true;
            }
            if let Some(dir) = self.dirs[dir_idx].as_mut() {
                if !dir.files.is_empty() || !dir.excluded {
                    changed = true;
                }
                for f in &dir.files {
                    self.id_to_dir[f.id.0 as usize] = u32::MAX;
                }
                dir.files.clear();
                dir.subdirs.clear();
                dir.excluded = true;
                dir.enumeration_complete = false;
            }
            if changed {
                self.recompute_upwards(dir_idx);
                return (vec![dir_idx], Vec::new());
            }
            return (Vec::new(), Vec::new());
        }

        // Update directory metadata.
        if let Some(dir) = self.dirs[dir_idx].as_mut() {
            if dir.identity != snapshot.identity
                || dir.enumeration_complete != snapshot.enumeration_complete
                || dir.excluded
            {
                dir.identity = snapshot.identity;
                dir.enumeration_complete = snapshot.enumeration_complete;
                dir.excluded = false;
                changed = true;
            }
        }

        // Diff direct files, preserving NodeIds for files with matching name & identity.
        let mut existing_files: HashMap<Box<OsStr>, FileNode> = {
            let Some(dir) = self.dirs[dir_idx].as_mut() else {
                return (Vec::new(), Vec::new());
            };
            dir.files.drain(..).map(|f| (f.name.clone(), f)).collect()
        };
        let mut next_files = Vec::with_capacity(snapshot.files.len());
        for entry in snapshot.files {
            let name = entry
                .path
                .file_name()
                .unwrap_or_else(|| entry.path.as_os_str())
                .to_os_string()
                .into_boxed_os_str();
            if let Some(mut old) = existing_files.remove(&name) {
                if old.identity == entry.identity {
                    if old.size != entry.size || old.can_trash != entry.can_trash {
                        old.size = entry.size;
                        old.can_trash = entry.can_trash;
                        changed = true;
                    }
                    next_files.push(old);
                    continue;
                }
                // Replaced at same path with new identity: retire old NodeId.
                self.id_to_dir[old.id.0 as usize] = u32::MAX;
                changed = true;
            } else {
                changed = true;
            }
            let id = self.alloc_id(dir_idx);
            next_files.push(FileNode {
                id,
                name,
                size: entry.size,
                identity: entry.identity,
                can_trash: entry.can_trash,
            });
        }
        // Any remaining files in `existing_files` were deleted.
        for (_, old) in existing_files {
            self.id_to_dir[old.id.0 as usize] = u32::MAX;
            changed = true;
        }
        if let Some(dir) = self.dirs[dir_idx].as_mut() {
            dir.files = next_files;
        }

        // Diff direct subdirectories.
        let new_subdir_map: HashMap<PathBuf, FileIdentity> = snapshot.subdirs.into_iter().collect();
        let existing_subdirs = self.dirs[dir_idx]
            .as_ref()
            .map(|d| d.subdirs.clone())
            .unwrap_or_default();
        let mut kept_subdirs = Vec::new();
        for child_idx in existing_subdirs {
            let Some((child_path, child_id)) = self.dirs[child_idx]
                .as_ref()
                .map(|c| (c.path.clone(), c.identity))
            else {
                continue;
            };
            match new_subdir_map.get(&child_path) {
                Some(&new_id) if child_id == FileIdentity::default() || child_id == new_id => {
                    kept_subdirs.push(child_idx);
                }
                Some(_) => {
                    // Replaced by a different directory at the same path.
                    self.unlink_subtree(child_idx);
                    changed = true;
                }
                None => {
                    if snapshot.enumeration_complete {
                        self.unlink_subtree(child_idx);
                        changed = true;
                    } else {
                        kept_subdirs.push(child_idx);
                    }
                }
            }
        }
        if let Some(dir) = self.dirs[dir_idx].as_mut() {
            dir.subdirs = kept_subdirs;
        }
        for (sub_path, sub_id) in new_subdir_map {
            if !self.dir_by_path.contains_key(&sub_path) {
                self.alloc_dir(sub_path.clone(), Some(dir_idx), sub_id, false, false);
                new_subdirs.push(sub_path);
                changed = true;
            }
        }

        if changed {
            self.recompute_upwards(dir_idx);
            (vec![dir_idx], new_subdirs)
        } else {
            (Vec::new(), new_subdirs)
        }
    }

    /// Reconcile a full rescan of the same root in-place, preserving `NodeId`s
    /// of surviving tiles and returning only the changed directory indices.
    pub fn reconcile_scan(&mut self, result: ScanResult) -> Option<Vec<usize>> {
        if result.root != self.root {
            return None;
        }
        self.skipped = result.skipped;
        self.errors = result.errors;
        self.elapsed = result.elapsed;

        let mut facts_by_path: HashMap<PathBuf, scan::DirectoryFact> = HashMap::new();
        for fact in result.directories.iter() {
            facts_by_path.insert(fact.path.clone(), fact.clone());
        }
        let mut files_by_dir: HashMap<PathBuf, Vec<scan::FileEntry>> = HashMap::new();
        for file in result.files {
            files_by_dir
                .entry(file.parent.to_path_buf())
                .or_default()
                .push(file);
        }
        let mut subdirs_by_dir: HashMap<PathBuf, Vec<(PathBuf, FileIdentity)>> = HashMap::new();
        for fact in facts_by_path.values() {
            if fact.path != self.root
                && let Some(parent) = fact.path.parent()
                && parent.starts_with(&self.root)
                && !fact.excluded
            {
                subdirs_by_dir
                    .entry(parent.to_path_buf())
                    .or_default()
                    .push((fact.path.clone(), fact.identity));
            }
        }

        let mut all_paths: Vec<PathBuf> = facts_by_path
            .keys()
            .chain(files_by_dir.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        all_paths.sort_unstable_by_key(|p| p.components().count());

        let mut dirty = Vec::new();
        for path in all_paths {
            let fact = facts_by_path.get(&path);
            let snap = DirectorySnapshot {
                identity: fact.map_or_else(FileIdentity::default, |f| f.identity),
                enumeration_complete: fact.is_none_or(|f| f.enumeration_complete),
                excluded: fact.is_some_and(|f| f.excluded),
                files: files_by_dir.remove(&path).unwrap_or_default(),
                subdirs: subdirs_by_dir.remove(&path).unwrap_or_default(),
            };
            let (d, _) = self.apply_dir_snapshot(&path, Some(snap));
            dirty.extend(d);
        }

        // Remove any directories that disappeared completely from the scan.
        let stale: Vec<PathBuf> = self
            .dir_by_path
            .keys()
            .filter(|p| {
                p.as_path() != self.root.as_path()
                    && !facts_by_path.contains_key(*p)
                    && !subdirs_by_dir.contains_key(*p)
            })
            .cloned()
            .collect();
        for p in stale {
            let (d, _) = self.apply_dir_snapshot(&p, None);
            dirty.extend(d);
        }

        Some(dirty)
    }

    /// Apply an in-memory virtual mutation for Demo animation testing.
    pub fn apply_demo_action(
        &mut self,
        action: crate::demo::DemoAction,
        seed: u64,
        mode: ViewMode,
    ) -> Vec<usize> {
        let live_dirs: Vec<usize> = self
            .dirs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| d.as_ref().map(|_| i))
            .collect();
        if live_dirs.is_empty() {
            return Vec::new();
        }
        match action {
            crate::demo::DemoAction::AddFile => {
                // Pick a non-root directory if available so local subtree motion is visible.
                let candidates: Vec<usize> =
                    live_dirs.iter().copied().filter(|&i| i != 0).collect();
                let pool = if candidates.is_empty() {
                    &live_dirs
                } else {
                    &candidates
                };
                let dir_idx = pool[(seed as usize) % pool.len()];
                let id = self.alloc_id(dir_idx);
                let size = (8 + (seed.wrapping_mul(37) % 160)) * 1024 * 1024;
                let name =
                    std::ffi::OsString::from(format!("新增文件-{seed:04}.dat")).into_boxed_os_str();
                if let Some(dir) = self.dirs[dir_idx].as_mut() {
                    dir.files.push(FileNode {
                        id,
                        name,
                        size,
                        identity: FileIdentity {
                            device: 0xDE00_0001,
                            inode: (1_000_000 + seed).into(),
                        },
                        can_trash: true,
                    });
                }
                self.recompute_upwards(dir_idx);
                vec![dir_idx]
            }
            crate::demo::DemoAction::ResizeRandom => {
                let with_files: Vec<usize> = live_dirs
                    .iter()
                    .copied()
                    .filter(|&i| self.dirs[i].as_ref().is_some_and(|d| !d.files.is_empty()))
                    .collect();
                if with_files.is_empty() {
                    return Vec::new();
                }
                let dir_idx = with_files[(seed as usize) % with_files.len()];
                if let Some(dir) = self.dirs[dir_idx].as_mut() {
                    let f_idx = (seed.wrapping_mul(17) as usize) % dir.files.len();
                    let file = &mut dir.files[f_idx];
                    if seed % 2 == 0 {
                        file.size = file.size.saturating_mul(5).min(512 * 1024 * 1024);
                    } else {
                        file.size = (file.size / 5).max(256 * 1024);
                    }
                }
                self.recompute_upwards(dir_idx);
                vec![dir_idx]
            }
            crate::demo::DemoAction::DeleteRandom => {
                // In Hybrid/Folders mode, alternate deleting leaf folders and files.
                if mode != ViewMode::Files && seed % 2 == 0 {
                    let folders: Vec<PathBuf> = live_dirs
                        .iter()
                        .filter_map(|&i| {
                            if i == 0 {
                                return None;
                            }
                            let d = self.dirs[i].as_ref()?;
                            (d.total_files > 0 && d.is_leaf).then(|| d.path.clone())
                        })
                        .collect();
                    if !folders.is_empty() {
                        let target = &folders[(seed as usize) % folders.len()];
                        return self.remove_directory(target, FileIdentity::default());
                    }
                }
                let with_files: Vec<usize> = live_dirs
                    .iter()
                    .copied()
                    .filter(|&i| self.dirs[i].as_ref().is_some_and(|d| !d.files.is_empty()))
                    .collect();
                if with_files.is_empty() {
                    return Vec::new();
                }
                let dir_idx = with_files[(seed as usize) % with_files.len()];
                let target = self.dirs[dir_idx].as_ref().and_then(|d| {
                    let f = d
                        .files
                        .get((seed.wrapping_mul(13) as usize) % d.files.len())?;
                    Some((d.file_path(f), f.identity))
                });
                if let Some((path, id)) = target {
                    self.remove_file(&path, id)
                } else {
                    Vec::new()
                }
            }
        }
    }

    pub fn tile_info(&self, id: NodeId, mode: ViewMode, rect: Rect) -> Option<TileInfo> {
        let dir_idx = *self.id_to_dir.get(id.0 as usize)?;
        if dir_idx == u32::MAX {
            return None;
        }
        let dir = self.dirs.get(dir_idx as usize)?.as_ref()?;
        if dir.id == id {
            let size = match mode {
                ViewMode::Folders | ViewMode::Hybrid => dir.direct_file_bytes,
                ViewMode::Files => dir.total_bytes,
            };
            return Some(TileInfo {
                id,
                path: dir.path.clone(),
                size,
                identity: dir.identity,
                is_folder: true,
                can_trash: dir.direct_can_trash,
                rect,
            });
        }
        let file = dir.files.iter().find(|f| f.id == id)?;
        Some(TileInfo {
            id,
            path: dir.file_path(file),
            size: file.size,
            identity: file.identity,
            is_folder: false,
            can_trash: file.can_trash,
            rect,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct LaidOutTile {
    id: NodeId,
    rect: Rect,
    color: Color32,
}

#[derive(Clone, Debug)]
struct DirLayout {
    bounds: Rect,
    platform: Option<Platform>,
    depth: u8,
    tiles: Vec<LaidOutTile>,
    children: Vec<(usize, Rect)>,
}

/// Result of an incremental `LayoutTree::apply_dirty` call.
pub struct LayoutDiff {
    pub dirty_rects: Vec<Rect>,
    pub removed_ids: Vec<NodeId>,
    pub upserted_tiles: Vec<TileInfo>,
    pub transitions: TransitionBatch,
}

pub struct LayoutTree {
    pub mode: ViewMode,
    pub bounds: Rect,
    pub pixel_size: f32,
    dirs: HashMap<usize, DirLayout>,
}

impl LayoutTree {
    /// Build the initial layout tree for `mode`. Uses the exact same
    /// `layout_subtree` routine as every incremental update.
    pub fn build(
        scene: &SceneTree,
        mode: ViewMode,
        bounds: Rect,
        pixel_size: f32,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        let mut tree = Self {
            mode,
            bounds,
            pixel_size,
            dirs: HashMap::new(),
        };
        if !tree.layout_subtree(scene, 0, bounds, 0, token, generation) {
            return None;
        }
        Some(tree)
    }

    /// Incrementally update ONLY the affected subtrees for `dirty_dirs`.
    /// Mathematically closed: because root (0) always owns `self.bounds`, every
    /// mutation resolves to one or more disjoint fixed-bounds containers with
    /// zero fallback paths.
    pub fn apply_dirty(
        &mut self,
        scene: &SceneTree,
        dirty_dirs: &[usize],
        pixel_size: f32,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<LayoutDiff> {
        self.pixel_size = pixel_size;
        if dirty_dirs.is_empty() {
            return Some(LayoutDiff {
                dirty_rects: Vec::new(),
                removed_ids: Vec::new(),
                upserted_tiles: Vec::new(),
                transitions: TransitionBatch {
                    dirty_rects: Vec::new(),
                    items: Vec::new(),
                },
            });
        }

        // Step 1: Map every dirty directory to the nearest existing container
        // in `self.dirs` whose outer bounds stay fixed.
        let mut candidates = HashSet::new();
        for &d in dirty_dirs {
            let scope = self.resolve_scope(scene, d);
            candidates.insert(scope);
        }

        // Step 2: Keep only topmost disjoint scopes (drop any scope whose
        // ancestor is also in `candidates`).
        let mut scopes = Vec::new();
        for &s in &candidates {
            let mut ancestor = scene
                .dirs
                .get(s)
                .and_then(|d| d.as_ref())
                .and_then(|d| d.parent);
            let mut covered = false;
            while let Some(a) = ancestor {
                if candidates.contains(&a) {
                    covered = true;
                    break;
                }
                ancestor = scene
                    .dirs
                    .get(a)
                    .and_then(|d| d.as_ref())
                    .and_then(|d| d.parent);
            }
            if !covered {
                scopes.push(s);
            }
        }
        scopes.sort_unstable();

        // Step 3: For each disjoint scope, snapshot before-tiles, re-layout in
        // its existing fixed bounds, snapshot after-tiles, and diff.
        let mut dirty_rects = Vec::with_capacity(scopes.len());
        let mut before_tiles = HashMap::<NodeId, (Rect, Color32)>::new();
        let mut after_tiles = HashMap::<NodeId, (Rect, Color32)>::new();

        for &scope in &scopes {
            let (scope_bounds, scope_depth) = self
                .dirs
                .get(&scope)
                .map_or((self.bounds, 0), |l| (l.bounds, l.depth));
            self.collect_subtree_tiles(scope, &mut before_tiles);
            self.remove_subtree_layouts(scope);
            if !self.layout_subtree(scene, scope, scope_bounds, scope_depth, token, generation) {
                return None;
            }
            self.collect_subtree_tiles(scope, &mut after_tiles);
            dirty_rects.push(scope_bounds);
        }

        // Step 4: Compute removed IDs, upserted TileInfos (only visible ones),
        // and GPU transition items.
        let mut removed_ids = Vec::new();
        let mut transitions = Vec::new();
        for (&id, &(from, color)) in &before_tiles {
            if let Some(&(to, new_color)) = after_tiles.get(&id) {
                if !rects_approx_eq(from, to) {
                    transitions.push(TransitionItem {
                        from,
                        to,
                        color: new_color,
                        kind: TransitionKind::Move,
                    });
                }
            } else {
                removed_ids.push(id);
                let center = Rect {
                    x: from.x + from.w * 0.5,
                    y: from.y + from.h * 0.5,
                    w: 0.0,
                    h: 0.0,
                };
                transitions.push(TransitionItem {
                    from,
                    to: center,
                    color,
                    kind: TransitionKind::FadeOut,
                });
            }
        }
        for (&id, &(to, color)) in &after_tiles {
            if !before_tiles.contains_key(&id) {
                let center = Rect {
                    x: to.x + to.w * 0.5,
                    y: to.y + to.h * 0.5,
                    w: 0.0,
                    h: 0.0,
                };
                transitions.push(TransitionItem {
                    from: center,
                    to,
                    color,
                    kind: TransitionKind::FadeIn,
                });
            }
        }

        // Keep the largest visible animated tiles within our fast GPU budget.
        let min_dim = self.pixel_size.max(0.5);
        transitions.retain(|t| {
            let max_w = t.from.w.max(t.to.w);
            let max_h = t.from.h.max(t.to.h);
            max_w >= min_dim && max_h >= min_dim
        });
        if transitions.len() > MAX_ANIMATED_TILES {
            transitions.sort_unstable_by(|a, b| {
                let area_a = (a.from.w * a.from.h).max(a.to.w * a.to.h);
                let area_b = (b.from.w * b.from.h).max(b.to.w * b.to.h);
                area_b.total_cmp(&area_a)
            });
            transitions.truncate(MAX_ANIMATED_TILES);
        }

        let mut upserted_tiles = Vec::new();
        for (&id, &(rect, _)) in &after_tiles {
            if rect.w >= self.pixel_size * 0.25
                && rect.h >= self.pixel_size * 0.25
                && let Some(info) = scene.tile_info(id, self.mode, rect)
            {
                upserted_tiles.push(info);
            }
        }

        Some(LayoutDiff {
            dirty_rects: dirty_rects.clone(),
            removed_ids,
            upserted_tiles,
            transitions: TransitionBatch {
                dirty_rects,
                items: transitions,
            },
        })
    }

    /// Walk up from `dir_idx` to the nearest active container in `self.dirs`
    /// whose outer bounds can remain unchanged. Always terminates at `0` (root).
    fn resolve_scope(&self, scene: &SceneTree, dir_idx: usize) -> usize {
        let mut cur = dir_idx;
        loop {
            if cur == 0 {
                return 0;
            }
            let Some(dir) = scene.dirs.get(cur).and_then(|d| d.as_ref()) else {
                return 0;
            };
            let parent = dir.parent.unwrap_or(0);
            let Some(existing) = self.dirs.get(&cur) else {
                // Not currently a container (new dir or previously collapsed).
                cur = parent;
                continue;
            };
            // If this directory became empty, collapsed into a single leaf tile
            // (in Hybrid mode), or toggled its unary/platform eligibility, lift
            // to its parent so sibling allocations stay consistent.
            let is_empty = dir.total_files == 0;
            let collapsed_hybrid = self.mode == ViewMode::Hybrid && dir.is_leaf;
            if is_empty || collapsed_hybrid {
                cur = parent;
                continue;
            }
            let visible_children = count_visible_children(scene, dir, self.mode);
            let has_direct_tile = has_direct_visual_tiles(dir, self.mode);
            let unary = visible_children == 1 && !has_direct_tile;
            let wants_platform = !unary
                && Platform::new(
                    existing.bounds,
                    platform_color(existing.depth),
                    existing.depth,
                    self.pixel_size,
                )
                .is_some();
            if wants_platform != existing.platform.is_some() {
                cur = parent;
                continue;
            }
            return cur;
        }
    }

    fn collect_subtree_tiles(&self, root_idx: usize, out: &mut HashMap<NodeId, (Rect, Color32)>) {
        let mut stack = vec![root_idx];
        while let Some(idx) = stack.pop() {
            if let Some(layout) = self.dirs.get(&idx) {
                for t in &layout.tiles {
                    out.insert(t.id, (t.rect, t.color));
                }
                for &(child, _) in &layout.children {
                    stack.push(child);
                }
            }
        }
    }

    fn remove_subtree_layouts(&mut self, root_idx: usize) {
        let mut stack = vec![root_idx];
        while let Some(idx) = stack.pop() {
            if let Some(layout) = self.dirs.remove(&idx) {
                for (child, _) in layout.children {
                    stack.push(child);
                }
            }
        }
    }

    /// The single recursive layout function used for BOTH initial build and all
    /// incremental subtree updates.
    fn layout_subtree(
        &mut self,
        scene: &SceneTree,
        root_idx: usize,
        root_bounds: Rect,
        root_depth: u8,
        token: &AtomicU64,
        generation: u64,
    ) -> bool {
        let mut queue = VecDeque::new();
        queue.push_back((root_idx, root_bounds, root_depth));

        while let Some((dir_idx, bounds, depth)) = queue.pop_front() {
            if token.load(Ordering::Relaxed) != generation {
                return false;
            }
            let Some(dir) = scene.dirs.get(dir_idx).and_then(|d| d.as_ref()) else {
                if dir_idx == 0 {
                    self.dirs.insert(
                        0,
                        DirLayout {
                            bounds,
                            platform: None,
                            depth: 0,
                            tiles: Vec::new(),
                            children: Vec::new(),
                        },
                    );
                }
                continue;
            };

            let below_root = dir_idx != 0;
            // In Hybrid mode, a leaf directory collapses into one tile inside its
            // parent — except when root itself is a leaf, in which case root
            // draws its single aggregated folder tile directly in `bounds`.
            if !below_root && self.mode == ViewMode::Hybrid && dir.is_leaf {
                let tiles = if !dir.files.is_empty() {
                    vec![LaidOutTile {
                        id: dir.id,
                        rect: bounds,
                        color: crate::color(&dir.path),
                    }]
                } else {
                    Vec::new()
                };
                self.dirs.insert(
                    0,
                    DirLayout {
                        bounds,
                        platform: None,
                        depth: 0,
                        tiles,
                        children: Vec::new(),
                    },
                );
                continue;
            }

            // Collect visual items inside this directory for the active mode.
            #[derive(Clone, Copy)]
            enum VisualItem {
                File(NodeId, Color32),
                FolderTile(NodeId, Color32),
                SubdirContainer(usize),
            }
            let mut items: Vec<(VisualItem, u64)> = Vec::new();
            let dir_color = crate::color(&dir.path);

            match self.mode {
                ViewMode::Files => {
                    for f in &dir.files {
                        items.push((VisualItem::File(f.id, dir_color), f.size.max(1)));
                    }
                    for &child in &dir.subdirs {
                        if let Some(c) = scene.dirs[child].as_ref()
                            && c.total_files > 0
                        {
                            items.push((VisualItem::SubdirContainer(child), c.total_bytes.max(1)));
                        }
                    }
                }
                ViewMode::Folders => {
                    if !dir.files.is_empty() {
                        items.push((
                            VisualItem::FolderTile(dir.id, dir_color),
                            dir.direct_file_bytes.max(1),
                        ));
                    }
                    for &child in &dir.subdirs {
                        if let Some(c) = scene.dirs[child].as_ref()
                            && c.total_files > 0
                        {
                            // A leaf subdirectory with only direct files can be
                            // drawn directly as a folder tile without an extra wrapper.
                            let child_has_nonempty_sub = c.subdirs.iter().any(|&gc| {
                                scene.dirs[gc].as_ref().is_some_and(|g| g.total_files > 0)
                            });
                            if !child_has_nonempty_sub && !c.files.is_empty() {
                                items.push((
                                    VisualItem::FolderTile(c.id, crate::color(&c.path)),
                                    c.direct_file_bytes.max(1),
                                ));
                            } else {
                                items.push((
                                    VisualItem::SubdirContainer(child),
                                    c.total_bytes.max(1),
                                ));
                            }
                        }
                    }
                }
                ViewMode::Hybrid => {
                    for f in &dir.files {
                        items.push((VisualItem::File(f.id, dir_color), f.size.max(1)));
                    }
                    for &child in &dir.subdirs {
                        if let Some(c) = scene.dirs[child].as_ref()
                            && c.total_files > 0
                        {
                            if c.is_leaf {
                                items.push((
                                    VisualItem::FolderTile(c.id, crate::color(&c.path)),
                                    c.direct_file_bytes.max(1),
                                ));
                            } else {
                                items.push((
                                    VisualItem::SubdirContainer(child),
                                    c.total_bytes.max(1),
                                ));
                            }
                        }
                    }
                }
            }

            let container_count = items
                .iter()
                .filter(|(it, _)| matches!(it, VisualItem::SubdirContainer(_)))
                .count();
            let tile_count = items.len() - container_count;
            let unary = container_count == 1 && tile_count == 0;
            let platform = (below_root && !unary)
                .then(|| Platform::new(bounds, platform_color(depth), depth, self.pixel_size))
                .flatten();
            let content = platform.map_or(bounds, |p| p.top);
            let next_depth = if below_root {
                depth.saturating_add(u8::from(platform.is_some()))
            } else {
                0
            };

            let mut laid_tiles = Vec::with_capacity(tile_count);
            let mut laid_children = Vec::with_capacity(container_count);

            if !items.is_empty() && render::valid(content) {
                let weights: Vec<(usize, u64)> = items
                    .iter()
                    .enumerate()
                    .map(|(i, (_, w))| (i, *w))
                    .collect();
                let Some(rects) = treemap::layout_with_cancel(&weights, content, token, generation)
                else {
                    return false;
                };
                for (slot, rect) in rects {
                    match items[slot].0 {
                        VisualItem::File(id, color) | VisualItem::FolderTile(id, color) => {
                            laid_tiles.push(LaidOutTile { id, rect, color });
                        }
                        VisualItem::SubdirContainer(child_idx) => {
                            laid_children.push((child_idx, rect));
                            queue.push_back((child_idx, rect, next_depth));
                        }
                    }
                }
            }

            self.dirs.insert(
                dir_idx,
                DirLayout {
                    bounds,
                    platform,
                    depth,
                    tiles: laid_tiles,
                    children: laid_children,
                },
            );
        }
        true
    }

    /// Collect ONLY entries and platforms intersecting `region` (expanded by
    /// `LIGHT_REACH` for hill-shading continuity), pruning untouched subtrees.
    pub fn collect_region(&self, region: Rect) -> (Vec<RenderEntry>, Vec<Platform>) {
        let query = Rect {
            x: region.x - LIGHT_REACH,
            y: region.y - LIGHT_REACH,
            w: region.w + LIGHT_REACH * 2.0,
            h: region.h + LIGHT_REACH * 2.0,
        };
        let mut entries = Vec::new();
        let mut platforms = Vec::new();
        let mut queue = VecDeque::new();
        queue.push_back(0usize);
        while let Some(idx) = queue.pop_front() {
            let Some(layout) = self.dirs.get(&idx) else {
                continue;
            };
            if !rects_intersect(layout.bounds, query) {
                continue;
            }
            if let Some(p) = layout.platform {
                platforms.push(p);
            }
            for t in &layout.tiles {
                if rects_intersect(t.rect, region) {
                    entries.push(RenderEntry {
                        id: t.id.0,
                        rect: t.rect,
                        color: t.color,
                    });
                }
            }
            for &(child, _) in &layout.children {
                queue.push_back(child);
            }
        }
        (entries, platforms)
    }

    /// Collect all visible `TileInfo`s for the UI lookup map after a full build
    /// or mode switch. Skips subpixel tiles that cannot own a pixel.
    pub fn collect_visible_tiles(&self, scene: &SceneTree) -> HashMap<NodeId, TileInfo> {
        let mut out = HashMap::new();
        let min_side = self.pixel_size * 0.25;
        for layout in self.dirs.values() {
            for t in &layout.tiles {
                if t.rect.w >= min_side
                    && t.rect.h >= min_side
                    && let Some(info) = scene.tile_info(t.id, self.mode, t.rect)
                {
                    out.insert(t.id, info);
                }
            }
        }
        out
    }

    /// Rasterize the entire current layout into one full-canvas `RasterPatch`.
    pub fn raster_full(
        &self,
        physical_size: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<RasterPatch> {
        let size = render::capped_size(physical_size);
        let (entries, platforms) = self.collect_region(self.bounds);
        render::raster_region(
            &entries,
            &platforms,
            self.bounds,
            size,
            [0, 0, size[0], size[1]],
            token,
            generation,
        )
    }

    /// Rasterize ONLY the dirty world rectangles into regional `RasterPatch`es.
    pub fn raster_dirty(
        &self,
        dirty_rects: &[Rect],
        physical_size: [usize; 2],
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Vec<RasterPatch>> {
        let size = render::capped_size(physical_size);
        let mut patches = Vec::with_capacity(dirty_rects.len());
        for &dirty in dirty_rects {
            let Some(pixel_rect) =
                render::world_to_pixel_rect(dirty, self.bounds, size, LIGHT_REACH)
            else {
                continue;
            };
            let world_rect = Rect {
                x: self.bounds.x + pixel_rect[0] as f32 * self.bounds.w / size[0] as f32,
                y: self.bounds.y + pixel_rect[1] as f32 * self.bounds.h / size[1] as f32,
                w: (pixel_rect[2] - pixel_rect[0]) as f32 * self.bounds.w / size[0] as f32,
                h: (pixel_rect[3] - pixel_rect[1]) as f32 * self.bounds.h / size[1] as f32,
            };
            let (entries, platforms) = self.collect_region(world_rect);
            let patch = render::raster_region(
                &entries,
                &platforms,
                self.bounds,
                size,
                pixel_rect,
                token,
                generation,
            )?;
            patches.push(patch);
        }
        Some(patches)
    }
}

fn platform_color(depth: u8) -> Color32 {
    let factor = 1.0 + (depth as f32 * 0.035).min(0.14);
    let channel = |v: u8| (v as f32 * factor).min(255.0) as u8;
    Color32::from_rgb(
        channel(crate::PANEL.r()),
        channel(crate::PANEL.g()),
        channel(crate::PANEL.b()),
    )
}

fn count_visible_children(scene: &SceneTree, dir: &DirNode, mode: ViewMode) -> usize {
    dir.subdirs
        .iter()
        .filter(|&&c| {
            let Some(child) = scene.dirs[c].as_ref() else {
                return false;
            };
            if child.total_files == 0 {
                return false;
            }
            match mode {
                ViewMode::Files => true,
                ViewMode::Hybrid => !child.is_leaf,
                ViewMode::Folders => child
                    .subdirs
                    .iter()
                    .any(|&gc| scene.dirs[gc].as_ref().is_some_and(|g| g.total_files > 0)),
            }
        })
        .count()
}

fn has_direct_visual_tiles(dir: &DirNode, mode: ViewMode) -> bool {
    match mode {
        ViewMode::Files => !dir.files.is_empty(),
        ViewMode::Folders => !dir.files.is_empty() || !dir.subdirs.is_empty(),
        ViewMode::Hybrid => !dir.files.is_empty() || !dir.subdirs.is_empty(),
    }
}

fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y
}

fn rects_approx_eq(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < 0.01
        && (a.y - b.y).abs() < 0.01
        && (a.w - b.w).abs() < 0.01
        && (a.h - b.h).abs() < 0.01
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn make_scan(files: &[(&str, u64, bool)]) -> ScanResult {
        let root = PathBuf::from("/disk");
        let mut dir_set = BTreeSet::new();
        dir_set.insert(root.clone());
        let mut entries = Vec::new();
        for &(p, size, leaf) in files {
            let path = PathBuf::from(p);
            let parent = path.parent().unwrap().to_path_buf();
            let mut cur = parent.as_path();
            while cur.starts_with(&root) {
                dir_set.insert(cur.to_path_buf());
                if cur == root {
                    break;
                }
                cur = cur.parent().unwrap();
            }
            entries.push(scan::FileEntry {
                path: path.clone(),
                parent: Arc::from(parent.as_path()),
                size,
                identity: FileIdentity {
                    device: 1,
                    inode: entries.len() as u64 + 100,
                },
                parent_identity: FileIdentity {
                    device: 1,
                    inode: 10,
                },
                parent_is_leaf: leaf,
                can_trash: true,
            });
        }
        let directories: Vec<_> = dir_set
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let has_sub = dir_set
                    .iter()
                    .any(|other| other.parent() == Some(p.as_path()));
                scan::DirectoryFact {
                    path: p.clone(),
                    identity: FileIdentity {
                        device: 1,
                        inode: i as u64 + 1,
                    },
                    enumeration_complete: true,
                    has_subdirectories: has_sub,
                    excluded: false,
                }
            })
            .collect();
        ScanResult {
            root,
            files: entries,
            directories: directories.into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn local_file_deletion_updates_only_parent_scope_and_animates_siblings() {
        let scan = make_scan(&[
            ("/disk/a/f1", 500, true),
            ("/disk/a/f2", 500, true),
            ("/disk/b/g1", 1000, true),
        ]);
        let mut scene = SceneTree::from_scan(scan);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        let token = AtomicU64::new(1);
        let mut layout =
            LayoutTree::build(&scene, ViewMode::Files, bounds, 1.0, &token, 1).unwrap();
        let before_b = layout.collect_visible_tiles(&scene);
        let g1_before = before_b
            .values()
            .find(|t| t.path == Path::new("/disk/b/g1"))
            .unwrap()
            .rect;

        let dirty = scene.remove_file(Path::new("/disk/a/f1"), FileIdentity::default());
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        assert_eq!(diff.dirty_rects.len(), 1);
        // Only folder `a` was dirty, NOT the full canvas!
        assert!(diff.dirty_rects[0].w < bounds.w || diff.dirty_rects[0].h < bounds.h);
        // Sibling `f2` moved/expanded and `f1` faded out.
        assert!(
            diff.transitions
                .items
                .iter()
                .any(|t| t.kind == TransitionKind::Move)
        );
        assert!(
            diff.transitions
                .items
                .iter()
                .any(|t| t.kind == TransitionKind::FadeOut)
        );
        // Unrelated folder `b/g1` did not move by a single pixel.
        let after_b = layout.collect_visible_tiles(&scene);
        let g1_after = after_b
            .values()
            .find(|t| t.path == Path::new("/disk/b/g1"))
            .unwrap()
            .rect;
        assert_eq!(g1_before, g1_after);
    }

    #[test]
    fn hybrid_folder_delete_and_empty_to_nonempty_never_fail() {
        let scan = make_scan(&[("/disk/leaf1/x", 400, true), ("/disk/leaf2/y", 600, true)]);
        let mut scene = SceneTree::from_scan(scan);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        let token = AtomicU64::new(1);
        let mut layout =
            LayoutTree::build(&scene, ViewMode::Hybrid, bounds, 1.0, &token, 1).unwrap();

        // Delete leaf1 -> leaf2 expands smoothly across root.
        let dirty = scene.remove_directory(Path::new("/disk/leaf1"), FileIdentity::default());
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        assert!(
            diff.transitions
                .items
                .iter()
                .any(|t| t.kind == TransitionKind::Move)
        );
        assert_eq!(scene.total_files(), 1);

        // Delete last remaining folder -> empty root cleanly handled.
        let dirty = scene.remove_directory(Path::new("/disk/leaf2"), FileIdentity::default());
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        assert_eq!(scene.total_files(), 0);
        assert!(diff.upserted_tiles.is_empty());

        // Add a brand-new file into the empty root via directory snapshot!
        let snap = DirectorySnapshot {
            identity: scene.root_identity,
            enumeration_complete: true,
            excluded: false,
            files: vec![scan::FileEntry {
                path: PathBuf::from("/disk/reborn.bin"),
                parent: Arc::from(Path::new("/disk")),
                size: 1024,
                identity: FileIdentity {
                    device: 1,
                    inode: 999,
                },
                parent_identity: scene.root_identity,
                parent_is_leaf: true,
                can_trash: true,
            }],
            subdirs: Vec::new(),
        };
        let (dirty, _) = scene.apply_dir_snapshot(Path::new("/disk"), Some(snap));
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        assert_eq!(scene.total_files(), 1);
        assert_eq!(diff.upserted_tiles.len(), 1);
        assert!(
            diff.transitions
                .items
                .iter()
                .any(|t| t.kind == TransitionKind::FadeIn)
        );
    }

    #[test]
    fn hybrid_leaf_unfold_and_refold_handles_topology_change_cleanly() {
        let scan = make_scan(&[("/disk/a/f1", 300, true), ("/disk/b/g1", 700, true)]);
        let mut scene = SceneTree::from_scan(scan);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        let token = AtomicU64::new(1);
        let mut layout =
            LayoutTree::build(&scene, ViewMode::Hybrid, bounds, 1.0, &token, 1).unwrap();
        assert_eq!(layout.collect_visible_tiles(&scene).len(), 2);

        // Create a subdirectory inside `a` -> `a` stops being a leaf and unfolds!
        let snap_a = DirectorySnapshot {
            identity: FileIdentity {
                device: 1,
                inode: 2,
            },
            enumeration_complete: true,
            excluded: false,
            files: vec![scan::FileEntry {
                path: PathBuf::from("/disk/a/f1"),
                parent: Arc::from(Path::new("/disk/a")),
                size: 300,
                identity: FileIdentity {
                    device: 1,
                    inode: 100,
                },
                parent_identity: FileIdentity {
                    device: 1,
                    inode: 2,
                },
                parent_is_leaf: false,
                can_trash: true,
            }],
            subdirs: vec![(
                PathBuf::from("/disk/a/sub"),
                FileIdentity {
                    device: 1,
                    inode: 500,
                },
            )],
        };
        let (mut dirty, new_subs) = scene.apply_dir_snapshot(Path::new("/disk/a"), Some(snap_a));
        assert_eq!(new_subs, vec![PathBuf::from("/disk/a/sub")]);
        let snap_sub = DirectorySnapshot {
            identity: FileIdentity {
                device: 1,
                inode: 500,
            },
            enumeration_complete: true,
            excluded: false,
            files: vec![scan::FileEntry {
                path: PathBuf::from("/disk/a/sub/deep"),
                parent: Arc::from(Path::new("/disk/a/sub")),
                size: 200,
                identity: FileIdentity {
                    device: 1,
                    inode: 501,
                },
                parent_identity: FileIdentity {
                    device: 1,
                    inode: 500,
                },
                parent_is_leaf: true,
                can_trash: true,
            }],
            subdirs: Vec::new(),
        };
        let (d2, _) = scene.apply_dir_snapshot(Path::new("/disk/a/sub"), Some(snap_sub));
        dirty.extend(d2);
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        assert!(!diff.dirty_rects.is_empty());
        // Now visible tiles should be: `/disk/a/f1`, `/disk/a/sub` (leaf folder), `/disk/b` (leaf folder).
        let tiles = layout.collect_visible_tiles(&scene);
        assert_eq!(tiles.len(), 3);
    }

    #[test]
    fn incremental_patch_matches_full_raster_after_deletion() {
        let scan = make_scan(&[
            ("/disk/a/f1", 300, true),
            ("/disk/a/f2", 500, true),
            ("/disk/b/g1", 800, true),
        ]);
        let mut scene = SceneTree::from_scan(scan);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 300.0,
        };
        let size = [400, 300];
        let token = AtomicU64::new(1);
        let mut layout =
            LayoutTree::build(&scene, ViewMode::Files, bounds, 1.0, &token, 1).unwrap();
        let initial = layout.raster_full(size, &token, 1).unwrap();
        let mut grid = render::OwnerGrid::from_patch(bounds, &initial);

        let dirty = scene.remove_file(Path::new("/disk/a/f1"), FileIdentity::default());
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        let patches = layout
            .raster_dirty(&diff.dirty_rects, size, &token, 1)
            .unwrap();
        for patch in &patches {
            grid.apply_patch(patch);
        }

        // Compare patched owner grid against a fresh raster of the same layout.
        let expected = layout.raster_full(size, &token, 1).unwrap();
        for y in 0..size[1] {
            for x in 0..size[0] {
                let wx = x as f32 + 0.5;
                let wy = y as f32 + 0.5;
                let got = grid.hit_test(wx, wy);
                let exp = expected.owners[y * size[0] + x];
                let exp = (exp != render::NONE).then_some(exp);
                assert_eq!(got, exp, "owner mismatch at ({x}, {y})");
            }
        }
    }
}
