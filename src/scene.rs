//! Single, closed persistent scene tree and stable incremental layout engine.
//!
//! Invariant: `LayoutTree` always holds the scan root container with the full
//! canvas bounds and a persistent binary space-partitioning `SplitTree` per
//! directory. Every filesystem mutation (delete, add, resize, directory
//! collapse/expand, rescan diff, or FSEvents batch) propagates exact byte
//! weights up to root and re-evaluates through the SAME split trees so:
//! 1. Every file and folder reflects its true proportion of the total size.
//! 2. Surviving items never reorder or flip orientation, preserving spatial memory.
use crate::{
    file_actions,
    render::{self, LIGHT_REACH, Platform, RasterPatch, RenderEntry},
    scan::{self, DirectorySnapshot, FileIdentity, ScanResult},
    treemap::{self, Rect, SplitTree, SyncEntry, SyncStatus},
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
    color: Color32,
    parent: Option<usize>,
    identity: FileIdentity,
    enumeration_complete: bool,
    has_subdirectories: bool,
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

#[derive(Clone, Copy, Debug)]
struct NodeLoc {
    dir_idx: u32,
    file_idx: u32,
}

impl NodeLoc {
    const RETIRED: Self = Self {
        dir_idx: u32::MAX,
        file_idx: u32::MAX,
    };

    const fn dir(dir_idx: usize) -> Self {
        Self {
            dir_idx: dir_idx as u32,
            file_idx: u32::MAX,
        }
    }

    const fn file(dir_idx: usize, file_idx: usize) -> Self {
        Self {
            dir_idx: dir_idx as u32,
            file_idx: file_idx as u32,
        }
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
    /// O(1) map from `NodeId.0` to owning `(dir_idx, file_idx)` (or `RETIRED`).
    id_to_loc: Vec<NodeLoc>,
    next_id: u32,
}

impl SceneTree {
    fn alloc_id(&mut self, loc: NodeLoc) -> NodeId {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).expect("NodeId space exhausted");
        if self.id_to_loc.len() <= id as usize {
            self.id_to_loc.resize(id as usize + 1, NodeLoc::RETIRED);
        }
        self.id_to_loc[id as usize] = loc;
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
        let id = self.alloc_id(NodeLoc::dir(idx));
        let color = crate::color(&path);
        let node = DirNode {
            id,
            path: path.clone(),
            color,
            parent,
            identity,
            enumeration_complete,
            has_subdirectories: false,
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
                parent_node.has_subdirectories = true;
            }
        }
        idx
    }

    /// Create an empty `SceneTree` rooted at `root` for progressive initial-scan streaming.
    pub fn empty_for_stream(
        root: PathBuf,
        real_root: PathBuf,
        root_identity: FileIdentity,
    ) -> Self {
        let mut tree = Self {
            root: root.clone(),
            real_root,
            #[cfg(test)]
            root_identity,
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
            dirs: Vec::with_capacity(256),
            dir_by_path: HashMap::with_capacity(256),
            id_to_loc: Vec::with_capacity(4096),
            next_id: 1,
        };
        tree.alloc_dir(root, None, root_identity, false, false);
        tree.recompute_dir_local(0);
        tree
    }

    /// Incrementally absorb one streaming `ScanBatch` during an initial scan,
    /// returning the directory indices touched by this batch.
    pub fn apply_stream_batch(&mut self, batch: scan::ScanBatch) -> Vec<usize> {
        self.skipped = self.skipped.max(batch.skipped);
        self.elapsed = self.elapsed.max(batch.elapsed);
        let remain = 24usize.saturating_sub(self.errors.len());
        self.errors.extend(batch.errors.into_iter().take(remain));

        let mut facts = batch.directories;
        facts.sort_unstable_by(|a, b| {
            a.path
                .components()
                .count()
                .cmp(&b.path.components().count())
                .then_with(|| a.path.cmp(&b.path))
        });

        let mut touched = HashSet::new();
        for fact in facts {
            if fact.path == self.root {
                if let Some(root_node) = self.dirs[0].as_mut() {
                    root_node.identity = fact.identity;
                    root_node.enumeration_complete = fact.enumeration_complete;
                    root_node.has_subdirectories |= fact.has_subdirectories;
                    root_node.excluded = fact.excluded;
                }
                touched.insert(0usize);
                continue;
            }
            if !fact.path.starts_with(&self.root) {
                continue;
            }
            let idx = self.ensure_dir_chain(&fact.path);
            if let Some(dir) = self.dirs[idx].as_mut() {
                dir.identity = fact.identity;
                dir.enumeration_complete = fact.enumeration_complete;
                dir.has_subdirectories |= fact.has_subdirectories;
                dir.excluded = fact.excluded;
            }
            touched.insert(idx);
        }

        for entry in batch.files {
            if !entry.path.starts_with(&self.root) {
                continue;
            }
            let dir_idx = self.ensure_dir_chain(entry.parent.as_ref());
            let name = entry
                .path
                .file_name()
                .unwrap_or_else(|| entry.path.as_os_str())
                .to_os_string()
                .into_boxed_os_str();
            let file_idx = self.dirs[dir_idx].as_ref().map_or(0, |d| d.files.len());
            let file_id = self.alloc_id(NodeLoc::file(dir_idx, file_idx));
            if let Some(dir) = self.dirs[dir_idx].as_mut() {
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
            touched.insert(dir_idx);
        }

        if touched.is_empty() {
            return Vec::new();
        }

        // Recompute aggregates bottom-up across only touched dirs and their ancestors.
        let mut affected = HashSet::with_capacity(touched.len() * 2);
        for &d in &touched {
            let mut cur = Some(d);
            while let Some(idx) = cur {
                if !affected.insert(idx) {
                    break;
                }
                cur = self.dirs[idx].as_ref().and_then(|n| n.parent);
            }
        }
        let mut order: Vec<(usize, usize)> = affected
            .into_iter()
            .filter_map(|idx| {
                self.dirs[idx]
                    .as_ref()
                    .map(|d| (d.path.components().count(), idx))
            })
            .collect();
        order.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        for (_, idx) in order {
            self.recompute_dir_local(idx);
        }

        touched.into_iter().collect()
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
            id_to_loc: Vec::with_capacity(result.files.len() + result.directories.len() + 16),
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
                    root_node.has_subdirectories |= fact.has_subdirectories;
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
                dir.has_subdirectories |= fact.has_subdirectories;
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
            let file_idx = tree.dirs[dir_idx].as_ref().map_or(0, |d| d.files.len());
            let file_id = tree.alloc_id(NodeLoc::file(dir_idx, file_idx));
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
        let is_leaf = dir.enumeration_complete
            && !dir.excluded
            && !dir.has_subdirectories
            && dir.subdirs.is_empty();
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
        let (removed_id, swapped_id_pos) = {
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
            let swapped = (pos < dir.files.len()).then(|| (dir.files[pos].id, pos));
            (removed.id, swapped)
        };
        self.id_to_loc[removed_id.0 as usize] = NodeLoc::RETIRED;
        if let Some((swapped_id, pos)) = swapped_id_pos {
            self.id_to_loc[swapped_id.0 as usize] = NodeLoc::file(dir_idx, pos);
        }
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
                parent_node.has_subdirectories = !parent_node.subdirs.is_empty();
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
                self.id_to_loc[node.id.0 as usize] = NodeLoc::RETIRED;
                for f in &node.files {
                    self.id_to_loc[f.id.0 as usize] = NodeLoc::RETIRED;
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
                            self.id_to_loc[f.id.0 as usize] = NodeLoc::RETIRED;
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
                    p.has_subdirectories = !p.subdirs.is_empty();
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
                    self.id_to_loc[f.id.0 as usize] = NodeLoc::RETIRED;
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
            let file_idx = next_files.len();
            if let Some(mut old) = existing_files.remove(&name) {
                if old.identity == entry.identity {
                    if old.size != entry.size || old.can_trash != entry.can_trash {
                        old.size = entry.size;
                        old.can_trash = entry.can_trash;
                        changed = true;
                    }
                    self.id_to_loc[old.id.0 as usize] = NodeLoc::file(dir_idx, file_idx);
                    next_files.push(old);
                    continue;
                }
                // Replaced at same path with new identity: retire old NodeId.
                self.id_to_loc[old.id.0 as usize] = NodeLoc::RETIRED;
                changed = true;
            } else {
                changed = true;
            }
            let id = self.alloc_id(NodeLoc::file(dir_idx, file_idx));
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
            self.id_to_loc[old.id.0 as usize] = NodeLoc::RETIRED;
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
            dir.has_subdirectories = !dir.subdirs.is_empty();
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
    pub fn apply_demo_action(&mut self, action: crate::demo::DemoAction, seed: u64) -> Vec<usize> {
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
                let file_idx = self.dirs[dir_idx].as_ref().map_or(0, |d| d.files.len());
                let id = self.alloc_id(NodeLoc::file(dir_idx, file_idx));
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
                // Alternate deleting leaf folders and files.
                if seed.is_multiple_of(2) {
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

    pub fn tile_info(&self, id: NodeId, rect: Rect) -> Option<TileInfo> {
        let loc = *self.id_to_loc.get(id.0 as usize)?;
        if loc.dir_idx == u32::MAX {
            return None;
        }
        let dir = self.dirs.get(loc.dir_idx as usize)?.as_ref()?;
        if loc.file_idx == u32::MAX {
            return Some(TileInfo {
                id,
                path: dir.path.clone(),
                size: dir.total_bytes,
                identity: dir.identity,
                is_folder: true,
                can_trash: dir.is_leaf && dir.direct_can_trash,
                rect,
            });
        }
        let file = dir.files.get(loc.file_idx as usize)?;
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

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum SlotKey {
    File(NodeId),
    SelfFiles,
    Subdir(u32),
}

#[derive(Clone, Copy)]
enum VisualItem {
    File(NodeId, Color32),
    FolderTile(NodeId, Color32),
    SubdirContainer(usize),
}

#[derive(Clone, Copy, Debug)]
struct LaidOutTile {
    key: SlotKey,
    id: NodeId,
    rect: Rect,
    color: Color32,
}

#[derive(Clone, Debug)]
struct DirLayout {
    bounds: Rect,
    platform: Option<Platform>,
    depth: u8,
    split_tree: SplitTree<SlotKey>,
    tiles: Vec<LaidOutTile>,
    children: Vec<(usize, Rect)>,
}

/// Result of an incremental `LayoutTree::apply_dirty` call.
pub struct LayoutDiff {
    pub dirty_rects: Vec<Rect>,
    pub removed_ids: Vec<NodeId>,
    pub upserted_tiles: Vec<TileInfo>,
}

pub struct LayoutTree {
    pub bounds: Rect,
    pub pixel_size: f32,
    dirs: HashMap<usize, DirLayout>,
}

impl LayoutTree {
    /// Build the initial hybrid layout tree.
    pub fn build(
        scene: &SceneTree,
        bounds: Rect,
        pixel_size: f32,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        let mut tree = Self {
            bounds,
            pixel_size,
            dirs: HashMap::new(),
        };
        if !tree.build_subtree(scene, 0, bounds, 0, token, generation) {
            return None;
        }
        Some(tree)
    }

    fn build_subtree(
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
            if scene.dirs.get(dir_idx).and_then(|d| d.as_ref()).is_none() {
                if dir_idx == 0 {
                    self.dirs.insert(
                        0,
                        DirLayout {
                            bounds,
                            platform: None,
                            depth: 0,
                            split_tree: SplitTree::empty(),
                            tiles: Vec::new(),
                            children: Vec::new(),
                        },
                    );
                }
                continue;
            }

            let items = collect_dir_items(scene, dir_idx);
            let (platform, content, next_depth) =
                compute_platform(dir_idx, bounds, depth, self.pixel_size, &items);

            let mut laid_tiles = Vec::new();
            let mut laid_children = Vec::new();
            let split_tree = if !items.is_empty() && render::valid(content) {
                let weights: Vec<(SlotKey, u64)> = items.iter().map(|&(k, _, w)| (k, w)).collect();
                let Some(tree) = SplitTree::build_with_cancel(&weights, content, token, generation)
                else {
                    return false;
                };
                let meta: HashMap<SlotKey, VisualItem> =
                    items.iter().map(|&(k, v, _)| (k, v)).collect();
                for (key, rect) in tree.evaluate(content) {
                    match resolve_visual_item(scene, meta[&key], rect, self.pixel_size) {
                        VisualItem::File(id, color) | VisualItem::FolderTile(id, color) => {
                            laid_tiles.push(LaidOutTile {
                                key,
                                id,
                                rect,
                                color,
                            });
                        }
                        VisualItem::SubdirContainer(child_idx) => {
                            laid_children.push((child_idx, rect));
                            queue.push_back((child_idx, rect, next_depth));
                        }
                    }
                }
                tree
            } else {
                SplitTree::empty()
            };

            self.dirs.insert(
                dir_idx,
                DirLayout {
                    bounds,
                    platform,
                    depth,
                    split_tree,
                    tiles: laid_tiles,
                    children: laid_children,
                },
            );
        }
        true
    }

    /// Incrementally update the layout tree for `dirty_dirs` while preserving
    /// exact global area proportions AND strict persistent split-tree spatial order.
    pub fn apply_dirty(
        &mut self,
        scene: &SceneTree,
        dirty_dirs: &[usize],
        pixel_size: f32,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<LayoutDiff> {
        self.apply_dirty_inner(scene, dirty_dirs, pixel_size, false, token, generation)
    }

    /// Initial streaming scan update: maintains near-square Squarified aspect
    /// ratios continuously while directories grow 100x–10,000x during cold scan,
    /// without a jarring full-tree rebuild when the scan finishes. Once scanning
    /// completes, all subsequent mutations use `apply_dirty` (strictly locked order).
    pub fn apply_dirty_streaming(
        &mut self,
        scene: &SceneTree,
        dirty_dirs: &[usize],
        pixel_size: f32,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<LayoutDiff> {
        self.apply_dirty_inner(scene, dirty_dirs, pixel_size, true, token, generation)
    }

    fn apply_dirty_inner(
        &mut self,
        scene: &SceneTree,
        dirty_dirs: &[usize],
        pixel_size: f32,
        allow_streaming_resquarify: bool,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<LayoutDiff> {
        self.pixel_size = pixel_size;
        if dirty_dirs.is_empty() {
            return Some(LayoutDiff {
                dirty_rects: Vec::new(),
                removed_ids: Vec::new(),
                upserted_tiles: Vec::new(),
            });
        }

        // Step 1: Collect every dirty directory and all of its ancestors up to root (0).
        let mut dirty_set = HashSet::new();
        dirty_set.insert(0usize);
        for &d in dirty_dirs {
            let mut cur = Some(d);
            while let Some(idx) = cur {
                if !dirty_set.insert(idx) {
                    break;
                }
                cur = scene
                    .dirs
                    .get(idx)
                    .and_then(|n| n.as_ref())
                    .and_then(|n| n.parent);
            }
        }

        let skip_tol = (self.pixel_size * 0.005).max(0.005);
        let visible_min = self.pixel_size * 0.25;

        struct UpdateTask {
            dir_idx: usize,
            old_bounds: Rect,
            new_bounds: Rect,
            depth: u8,
            collapsing: bool,
        }

        let mut queue = VecDeque::new();
        queue.push_back(UpdateTask {
            dir_idx: 0,
            old_bounds: self.bounds,
            new_bounds: self.bounds,
            depth: 0,
            collapsing: false,
        });

        let mut raster_dirty_rects: Vec<Rect> = Vec::new();
        let mut removed_ids: Vec<NodeId> = Vec::new();
        let mut upserted_tiles: Vec<TileInfo> = Vec::new();

        while let Some(task) = queue.pop_front() {
            if token.load(Ordering::Relaxed) != generation {
                return None;
            }
            let UpdateTask {
                dir_idx,
                old_bounds,
                new_bounds,
                depth,
                collapsing,
            } = task;

            // Case 1: Existing directory container is collapsing (deleted, emptied, or refolded).
            if collapsing {
                let Some(old_layout) = self.dirs.remove(&dir_idx) else {
                    continue;
                };
                let dirty_union = union_rects(old_bounds, new_bounds);
                if render::valid(dirty_union) {
                    raster_dirty_rects.push(dirty_union);
                }
                for t in &old_layout.tiles {
                    removed_ids.push(t.id);
                }
                for &(child_idx, child_old) in &old_layout.children {
                    queue.push_back(UpdateTask {
                        dir_idx: child_idx,
                        old_bounds: child_old,
                        new_bounds: center_rect(new_bounds),
                        depth: depth.saturating_add(1),
                        collapsing: true,
                    });
                }
                continue;
            }

            // Case 2: Live directory container.
            let is_dirty = dirty_set.contains(&dir_idx);
            let items = collect_dir_items(scene, dir_idx);
            let (platform, new_content, next_depth) =
                compute_platform(dir_idx, new_bounds, depth, self.pixel_size, &items);

            if let Some(existing) = self.dirs.get(&dir_idx) {
                let bounds_same = bounds_within_tol(existing.bounds, new_bounds, skip_tol);
                let platform_same =
                    existing.platform.is_some() == platform.is_some() && existing.depth == depth;
                if !is_dirty && bounds_same && platform_same {
                    // Completely untouched subtree: zero work, zero raster.
                    continue;
                }
            }

            let mut existing_opt = self.dirs.remove(&dir_idx);
            let Some(existing) = existing_opt.as_mut() else {
                // Subcase 2c: Brand-new directory container (newly created or unfolded leaf).
                let weights: Vec<(SlotKey, u64)> = items.iter().map(|&(k, _, w)| (k, w)).collect();
                let meta: HashMap<SlotKey, VisualItem> =
                    items.iter().map(|&(k, v, _)| (k, v)).collect();
                let split_tree = if !weights.is_empty() && render::valid(new_content) {
                    SplitTree::build_with_cancel(&weights, new_content, token, generation)?
                } else {
                    SplitTree::empty()
                };

                let dirty_union = union_rects(old_bounds, new_bounds);
                if render::valid(dirty_union) {
                    raster_dirty_rects.push(dirty_union);
                }

                let mut laid_tiles = Vec::new();
                let mut laid_children = Vec::new();

                for (key, to) in split_tree.evaluate(new_content) {
                    match resolve_visual_item(scene, meta[&key], to, self.pixel_size) {
                        VisualItem::File(id, color) | VisualItem::FolderTile(id, color) => {
                            laid_tiles.push(LaidOutTile {
                                key,
                                id,
                                rect: to,
                                color,
                            });
                            if to.w >= visible_min
                                && to.h >= visible_min
                                && let Some(info) = scene.tile_info(id, to)
                            {
                                upserted_tiles.push(info);
                            }
                        }
                        VisualItem::SubdirContainer(child_idx) => {
                            laid_children.push((child_idx, to));
                            queue.push_back(UpdateTask {
                                dir_idx: child_idx,
                                old_bounds: to,
                                new_bounds: to,
                                depth: next_depth,
                                collapsing: false,
                            });
                        }
                    }
                }

                self.dirs.insert(
                    dir_idx,
                    DirLayout {
                        bounds: new_bounds,
                        platform,
                        depth,
                        split_tree,
                        tiles: laid_tiles,
                        children: laid_children,
                    },
                );
                continue;
            };

            // Subcase 2b: Existing directory container being synchronized in-place.
            let old_platform = existing.platform;
            let old_content = old_platform.map_or(existing.bounds, |p| p.top);
            let outer_moved = !bounds_within_tol(existing.bounds, new_bounds, skip_tol)
                || old_platform.is_some() != platform.is_some()
                || existing.depth != depth;

            if dir_idx != 0 && outer_moved {
                let dirty_union = union_rects(existing.bounds, new_bounds);
                if render::valid(dirty_union) {
                    raster_dirty_rects.push(dirty_union);
                }
            }

            let eval_old_content =
                if dir_idx == 0 && existing.split_tree.is_empty() && !items.is_empty() {
                    center_rect(self.bounds)
                } else {
                    old_content
                };
            let eval_new_content =
                if dir_idx == 0 && !existing.split_tree.is_empty() && items.is_empty() {
                    center_rect(self.bounds)
                } else {
                    new_content
                };

            let old_tiles_by_key: HashMap<SlotKey, NodeId> =
                existing.tiles.iter().map(|t| (t.key, t.id)).collect();
            let weights: Vec<(SlotKey, u64)> = items.iter().map(|&(k, _, w)| (k, w)).collect();
            let meta: HashMap<SlotKey, VisualItem> =
                items.iter().map(|&(k, v, _)| (k, v)).collect();

            let entries = existing.split_tree.sync_and_evaluate(
                &weights,
                eval_old_content,
                eval_new_content,
                token,
                generation,
            )?;

            // During the initial cold streaming scan ONLY, keep each growing
            // container continuously Squarified using area-weighted average
            // aspect ratio (with hysteresis). Big visible blocks cannot be
            // masked by a 2px corner sliver, while already-square containers
            // (weighted avg <= 1.45 or within 12% of optimal) stay locked so
            // scan completion never jumps.
            let entries = if allow_streaming_resquarify
                && (is_dirty || outer_moved)
                && weights.len() > 1
                && render::valid(eval_new_content)
            {
                let old_wa = weighted_aspect(
                    entries
                        .iter()
                        .filter(|e| e.status != SyncStatus::Removed)
                        .map(|e| e.to),
                );
                if old_wa > 1.45 {
                    let candidate = SplitTree::build_with_cancel(
                        &weights,
                        eval_new_content,
                        token,
                        generation,
                    )?;
                    let cand_rects = candidate.evaluate(eval_new_content);
                    let new_wa = weighted_aspect(cand_rects.iter().map(|&(_, r)| r));
                    if new_wa < old_wa * 0.88 {
                        let new_map: HashMap<SlotKey, Rect> = cand_rects.into_iter().collect();
                        existing.split_tree = candidate;
                        entries
                            .into_iter()
                            .map(|mut e| {
                                if let Some(&new_to) = new_map.get(&e.key) {
                                    e.to = new_to;
                                    if e.status == SyncStatus::Added {
                                        e.from = center_rect(new_to);
                                    }
                                }
                                e
                            })
                            .collect::<Vec<SyncEntry<SlotKey>>>()
                    } else {
                        entries
                    }
                } else {
                    entries
                }
            } else {
                entries
            };

            let mut laid_tiles = Vec::new();
            let mut laid_children = Vec::new();

            for entry in entries {
                match entry.status {
                    SyncStatus::Removed => {
                        if let SlotKey::Subdir(c) = entry.key
                            && self.dirs.contains_key(&(c as usize))
                        {
                            queue.push_back(UpdateTask {
                                dir_idx: c as usize,
                                old_bounds: entry.from,
                                new_bounds: entry.to,
                                depth: next_depth,
                                collapsing: true,
                            });
                        } else if let Some(&id) = old_tiles_by_key.get(&entry.key) {
                            removed_ids.push(id);
                            let dirty_union = union_rects(entry.from, entry.to);
                            if render::valid(dirty_union) {
                                raster_dirty_rects.push(dirty_union);
                            }
                        }
                    }
                    SyncStatus::Added => {
                        match resolve_visual_item(
                            scene,
                            meta[&entry.key],
                            entry.to,
                            self.pixel_size,
                        ) {
                            VisualItem::File(id, color) | VisualItem::FolderTile(id, color) => {
                                laid_tiles.push(LaidOutTile {
                                    key: entry.key,
                                    id,
                                    rect: entry.to,
                                    color,
                                });
                                let dirty_union = union_rects(entry.from, entry.to);
                                if render::valid(dirty_union) {
                                    raster_dirty_rects.push(dirty_union);
                                }
                                if entry.to.w >= visible_min
                                    && entry.to.h >= visible_min
                                    && let Some(info) = scene.tile_info(id, entry.to)
                                {
                                    upserted_tiles.push(info);
                                }
                            }
                            VisualItem::SubdirContainer(child_idx) => {
                                laid_children.push((child_idx, entry.to));
                                queue.push_back(UpdateTask {
                                    dir_idx: child_idx,
                                    old_bounds: entry.from,
                                    new_bounds: entry.to,
                                    depth: next_depth,
                                    collapsing: false,
                                });
                            }
                        }
                    }
                    SyncStatus::Kept => {
                        match resolve_visual_item(
                            scene,
                            meta[&entry.key],
                            entry.to,
                            self.pixel_size,
                        ) {
                            VisualItem::File(id, color) | VisualItem::FolderTile(id, color) => {
                                // Check if a previously expanded SubdirContainer refolded into a FolderTile!
                                let refolded = matches!(entry.key, SlotKey::Subdir(c) if self.dirs.contains_key(&(c as usize)));
                                if let SlotKey::Subdir(c) = entry.key
                                    && refolded
                                {
                                    queue.push_back(UpdateTask {
                                        dir_idx: c as usize,
                                        old_bounds: entry.from,
                                        new_bounds: entry.to,
                                        depth: next_depth,
                                        collapsing: true,
                                    });
                                }

                                laid_tiles.push(LaidOutTile {
                                    key: entry.key,
                                    id,
                                    rect: entry.to,
                                    color,
                                });
                                let moved = !bounds_within_tol(entry.from, entry.to, skip_tol);
                                let tile_dirty = refolded
                                    || moved
                                    || matches!(entry.key, SlotKey::Subdir(c) if dirty_set.contains(&(c as usize)))
                                    || is_dirty;
                                if moved || refolded {
                                    let dirty_union = union_rects(entry.from, entry.to);
                                    if render::valid(dirty_union) {
                                        raster_dirty_rects.push(dirty_union);
                                    }
                                }
                                if tile_dirty
                                    && entry.to.w >= visible_min
                                    && entry.to.h >= visible_min
                                    && let Some(info) = scene.tile_info(id, entry.to)
                                {
                                    upserted_tiles.push(info);
                                }
                            }
                            VisualItem::SubdirContainer(child_idx) => {
                                // Check if a previously collapsed FolderTile unfolded into a SubdirContainer!
                                let unfolded = !self.dirs.contains_key(&child_idx);
                                if unfolded && let Some(&old_id) = old_tiles_by_key.get(&entry.key)
                                {
                                    removed_ids.push(old_id);
                                }
                                laid_children.push((child_idx, entry.to));
                                queue.push_back(UpdateTask {
                                    dir_idx: child_idx,
                                    old_bounds: entry.from,
                                    new_bounds: entry.to,
                                    depth: next_depth,
                                    collapsing: false,
                                });
                            }
                        }
                    }
                }
            }

            existing.bounds = new_bounds;
            existing.platform = platform;
            existing.depth = depth;
            existing.tiles = laid_tiles;
            existing.children = laid_children;
            self.dirs.insert(dir_idx, existing_opt.unwrap());
        }

        // Keep removed_ids strictly disjoint from upserted_tiles (e.g. refold).
        if !removed_ids.is_empty() && !upserted_tiles.is_empty() {
            let upserted_ids: HashSet<NodeId> = upserted_tiles.iter().map(|t| t.id).collect();
            removed_ids.retain(|id| !upserted_ids.contains(id));
        }

        // Merge raster dirty rects to avoid overlapping LIGHT_REACH raster work.
        let merged_raster_rects = merge_raster_dirty_rects(&raster_dirty_rects, self.bounds);

        Some(LayoutDiff {
            dirty_rects: merged_raster_rects,
            removed_ids,
            upserted_tiles,
        })
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
                    && let Some(info) = scene.tile_info(t.id, t.rect)
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

/// Sub-pixel LOD cutoff: a directory container smaller than ~1.5 physical
/// pixels on either side (or < 3.4 px² area) cannot visually resolve multiple
/// child tiles. Collapsing it into a single directory-colored tile avoids
/// recursing through hundreds of thousands of invisible nested directories and
/// automatically unfolds if the directory later grows or the viewport enlarges.
#[inline]
fn is_subpixel_container(rect: Rect, pixel_size: f32) -> bool {
    let min_side = pixel_size * 1.5;
    rect.w < min_side || rect.h < min_side || rect.w * rect.h < min_side * min_side * 1.5
}

#[inline]
fn resolve_visual_item(
    scene: &SceneTree,
    item: VisualItem,
    rect: Rect,
    pixel_size: f32,
) -> VisualItem {
    if let VisualItem::SubdirContainer(child_idx) = item
        && is_subpixel_container(rect, pixel_size)
        && let Some(c) = scene.dirs.get(child_idx).and_then(|d| d.as_ref())
    {
        return VisualItem::FolderTile(c.id, c.color);
    }
    item
}

fn collect_dir_items(scene: &SceneTree, dir_idx: usize) -> Vec<(SlotKey, VisualItem, u64)> {
    let Some(dir) = scene.dirs.get(dir_idx).and_then(|d| d.as_ref()) else {
        return Vec::new();
    };
    let mut items = Vec::with_capacity(dir.files.len() + dir.subdirs.len());
    let dir_color = dir.color;

    if dir_idx == 0 && dir.is_leaf {
        if !dir.files.is_empty() {
            items.push((
                SlotKey::SelfFiles,
                VisualItem::FolderTile(dir.id, dir_color),
                dir.direct_file_bytes.max(1),
            ));
        }
        return items;
    }

    for f in &dir.files {
        items.push((
            SlotKey::File(f.id),
            VisualItem::File(f.id, dir_color),
            f.size.max(1),
        ));
    }
    for &child in &dir.subdirs {
        if let Some(c) = scene.dirs[child].as_ref()
            && c.total_files > 0
        {
            if c.is_leaf {
                items.push((
                    SlotKey::Subdir(child as u32),
                    VisualItem::FolderTile(c.id, c.color),
                    c.direct_file_bytes.max(1),
                ));
            } else {
                items.push((
                    SlotKey::Subdir(child as u32),
                    VisualItem::SubdirContainer(child),
                    c.total_bytes.max(1),
                ));
            }
        }
    }
    items
}

fn compute_platform(
    dir_idx: usize,
    bounds: Rect,
    depth: u8,
    pixel_size: f32,
    items: &[(SlotKey, VisualItem, u64)],
) -> (Option<Platform>, Rect, u8) {
    let below_root = dir_idx != 0;
    let container_count = items
        .iter()
        .filter(|(_, it, _)| matches!(it, VisualItem::SubdirContainer(_)))
        .count();
    let tile_count = items.len() - container_count;
    let unary = container_count == 1 && tile_count == 0;
    let platform = (below_root && !unary)
        .then(|| Platform::new(bounds, platform_color(depth), depth, pixel_size))
        .flatten();
    let content = platform.map_or(bounds, |p| p.top);
    let next_depth = if below_root {
        depth.saturating_add(u8::from(platform.is_some()))
    } else {
        0
    };
    (platform, content, next_depth)
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

fn center_rect(r: Rect) -> Rect {
    Rect {
        x: r.x + r.w * 0.5,
        y: r.y + r.h * 0.5,
        w: 0.0,
        h: 0.0,
    }
}

fn union_rects(a: Rect, b: Rect) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);
    Rect {
        x: x0,
        y: y0,
        w: (x1 - x0).max(0.0),
        h: (y1 - y0).max(0.0),
    }
}

fn bounds_within_tol(a: Rect, b: Rect, tol: f32) -> bool {
    (a.x - b.x).abs() <= tol
        && (a.y - b.y).abs() <= tol
        && ((a.x + a.w) - (b.x + b.w)).abs() <= tol
        && ((a.y + a.h) - (b.y + b.h)).abs() <= tol
}

fn weighted_aspect(rects: impl Iterator<Item = Rect>) -> f64 {
    let mut sum_wa = 0.0f64;
    let mut sum_a = 0.0f64;
    for r in rects {
        let area = (r.w as f64) * (r.h as f64);
        if area > 0.0 {
            sum_wa += area * treemap::rect_aspect(r);
            sum_a += area;
        }
    }
    if sum_a > 0.0 { sum_wa / sum_a } else { 1.0 }
}

fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.w && a.x + a.w > b.x && a.y < b.y + b.h && a.y + a.h > b.y
}

fn merge_raster_dirty_rects(rects: &[Rect], full: Rect) -> Vec<Rect> {
    let mut valid_rects: Vec<Rect> = rects
        .iter()
        .copied()
        .filter(|&r| render::valid(r))
        .collect();
    if valid_rects.is_empty() {
        return Vec::new();
    }
    let full_area = (full.w * full.h).max(1.0);
    valid_rects.sort_unstable_by(|a, b| a.x.total_cmp(&b.x).then_with(|| a.y.total_cmp(&b.y)));
    let mut merged: Vec<Rect> = Vec::with_capacity(valid_rects.len().min(64));
    for mut rect in valid_rects {
        if rect.w * rect.h >= full_area * 0.6 {
            return vec![full];
        }
        let mut changed = true;
        while changed {
            changed = false;
            let mut i = 0;
            while i < merged.len() {
                let expanded = Rect {
                    x: merged[i].x - LIGHT_REACH,
                    y: merged[i].y - LIGHT_REACH,
                    w: merged[i].w + LIGHT_REACH * 2.0,
                    h: merged[i].h + LIGHT_REACH * 2.0,
                };
                if rects_intersect(expanded, rect) {
                    rect = union_rects(rect, merged.swap_remove(i));
                    if rect.w * rect.h >= full_area * 0.6 {
                        return vec![full];
                    }
                    changed = true;
                } else {
                    i += 1;
                }
            }
        }
        merged.push(rect);
    }
    let merged_area: f32 = merged.iter().map(|r| r.w * r.h).sum();
    if merged_area >= full_area * 0.6 {
        return vec![full];
    }
    merged
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
                    inode: (entries.len() as u64 + 100).into(),
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
                        inode: (i as u64 + 1).into(),
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
    fn file_deletion_updates_global_proportions_and_preserves_spatial_order() {
        let scan = make_scan(&[
            ("/disk/a/f1", 500, false),
            ("/disk/a/sub/f2", 500, true),
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
        let mut layout = LayoutTree::build(&scene, bounds, 1.0, &token, 1).unwrap();
        let before = layout.collect_visible_tiles(&scene);
        let sub_before = before
            .values()
            .find(|t| t.path == Path::new("/disk/a/sub"))
            .unwrap()
            .rect;
        let b_before = before
            .values()
            .find(|t| t.path == Path::new("/disk/b"))
            .unwrap()
            .rect;

        let dirty = scene.remove_file(Path::new("/disk/a/f1"), FileIdentity::default());
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        assert!(!diff.dirty_rects.is_empty());
        assert!(!diff.removed_ids.is_empty());

        // True proportions: /disk/a/sub (500) = 1/3, /disk/b (1000) = 2/3!
        let after = layout.collect_visible_tiles(&scene);
        let sub_after = after
            .values()
            .find(|t| t.path == Path::new("/disk/a/sub"))
            .unwrap()
            .rect;
        let b_after = after
            .values()
            .find(|t| t.path == Path::new("/disk/b"))
            .unwrap()
            .rect;
        let canvas_area = bounds.w * bounds.h;
        assert!(
            (sub_after.w * sub_after.h / canvas_area - 1.0 / 3.0).abs() < 0.02,
            "sub area should reflect 1/3 proportion, got {}",
            sub_after.w * sub_after.h / canvas_area
        );
        assert!(
            (b_after.w * b_after.h / canvas_area - 2.0 / 3.0).abs() < 0.02,
            "b area should reflect 2/3 proportion, got {}",
            b_after.w * b_after.h / canvas_area
        );
        // Spatial order between folder `a` and folder `b` must be preserved!
        let a_was_left = sub_before.x < b_before.x;
        let a_is_left = sub_after.x < b_after.x;
        assert_eq!(a_was_left, a_is_left, "folders must never swap sides");
    }

    #[test]
    fn resize_and_addition_preserve_spatial_memory_and_exact_proportions() {
        let scan = make_scan(&[
            ("/disk/a/f1", 400, true),
            ("/disk/b/g1", 300, true),
            ("/disk/c/h1", 200, true),
            ("/disk/d/k1", 100, true),
        ]);
        let mut scene = SceneTree::from_scan(scan);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 600.0,
        };
        let token = AtomicU64::new(1);
        let mut layout = LayoutTree::build(&scene, bounds, 1.0, &token, 1).unwrap();
        let before: HashMap<PathBuf, Rect> = layout
            .collect_visible_tiles(&scene)
            .into_values()
            .map(|t| (t.path, t.rect))
            .collect();

        // Grow the smallest folder `/disk/d` from 100 -> 1100 (now the largest!).
        let snap_d = DirectorySnapshot {
            identity: FileIdentity {
                device: 1,
                inode: 5,
            },
            enumeration_complete: true,
            excluded: false,
            files: vec![scan::FileEntry {
                path: PathBuf::from("/disk/d/k1"),
                parent: Arc::from(Path::new("/disk/d")),
                size: 1100,
                identity: FileIdentity {
                    device: 1,
                    inode: 103,
                },
                parent_identity: FileIdentity {
                    device: 1,
                    inode: 5,
                },
                parent_is_leaf: true,
                can_trash: true,
            }],
            subdirs: Vec::new(),
        };
        let (dirty, _) = scene.apply_dir_snapshot(Path::new("/disk/d"), Some(snap_d));
        let _diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();

        let after: HashMap<PathBuf, Rect> = layout
            .collect_visible_tiles(&scene)
            .into_values()
            .map(|t| (t.path, t.rect))
            .collect();
        let canvas_area = bounds.w * bounds.h;
        // Total is now 400 + 300 + 200 + 1100 = 2000 -> `/disk/d` must be 55%!
        let d_frac = after[Path::new("/disk/d")].w * after[Path::new("/disk/d")].h / canvas_area;
        assert!(
            (d_frac - 0.55).abs() < 0.01,
            "expected /disk/d to occupy 55%, got {d_frac}"
        );
        // Relative left/right and top/bottom ordering among all 4 folders must be preserved!
        let paths: Vec<PathBuf> = before.keys().cloned().collect();
        for p1 in &paths {
            for p2 in &paths {
                if p1 == p2 {
                    continue;
                }
                let (b1, b2) = (before[p1], before[p2]);
                let (a1, a2) = (after[p1], after[p2]);
                if b1.x + b1.w <= b2.x + 1e-2 {
                    assert!(
                        a1.x + a1.w <= a2.x + 1e-1,
                        "horizontal order flipped between {p1:?} and {p2:?}"
                    );
                }
                if b1.y + b1.h <= b2.y + 1e-2 {
                    assert!(
                        a1.y + a1.h <= a2.y + 1e-1,
                        "vertical order flipped between {p1:?} and {p2:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn tiny_mutation_skips_unaffected_sibling_subtrees() {
        let scan = make_scan(&[
            ("/disk/a/f1", 1_000, false),
            ("/disk/a/sub/f2", 5_000_000, true),
            ("/disk/b/g1", 1_000_000_000, true),
        ]);
        let mut scene = SceneTree::from_scan(scan);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        let token = AtomicU64::new(1);
        let mut layout = LayoutTree::build(&scene, bounds, 1.0, &token, 1).unwrap();

        let dirty = scene.remove_file(Path::new("/disk/a/f1"), FileIdentity::default());
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        // Folder `a` is visible (~4px wide) while sibling `b` moved by ~0.0008px
        // (< skip_tol), so dirty rect stays tiny and local!
        assert!(!diff.dirty_rects.is_empty());
        assert!(diff.dirty_rects.iter().all(|r| r.w < bounds.w * 0.25));
    }

    #[test]
    fn demo_mutations_keep_exact_proportions_and_spatial_order() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1200.0,
            h: 800.0,
        };
        let canvas_area = bounds.w * bounds.h;
        let token = AtomicU64::new(1);

        let mut scene = SceneTree::from_scan(crate::demo::virtual_scan());
        let mut layout = LayoutTree::build(&scene, bounds, 1.0, &token, 1).unwrap();

        // Record initial top-level folder rectangles from root's split tree.
        let root_rects = |l: &LayoutTree| -> HashMap<SlotKey, Rect> {
            let root_layout = &l.dirs[&0];
            root_layout
                .split_tree
                .evaluate(bounds)
                .into_iter()
                .collect()
        };
        let initial_top = root_rects(&layout);

        let actions = [
            crate::demo::DemoAction::AddFile,
            crate::demo::DemoAction::ResizeRandom,
            crate::demo::DemoAction::DeleteRandom,
            crate::demo::DemoAction::ResizeRandom,
            crate::demo::DemoAction::AddFile,
            crate::demo::DemoAction::DeleteRandom,
        ];
        for (step, action) in actions.into_iter().enumerate() {
            let dirty = scene.apply_demo_action(action, step as u64 + 1);
            let _diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();

            let now_top = root_rects(&layout);
            let total_bytes = scene.total_bytes() as f32;
            assert!(total_bytes > 0.0);

            // 1. Every top-level folder area must reflect its real byte share!
            for (&key, &rect) in &now_top {
                if let SlotKey::Subdir(idx) = key
                    && let Some(dir) = scene.dirs[idx as usize].as_ref()
                {
                    let expected_share = dir.total_bytes.max(1) as f32 / total_bytes;
                    let actual_share = (rect.w * rect.h) / canvas_area;
                    assert!(
                        (actual_share - expected_share).abs() < 1e-3,
                        "step={step}: dir {} share {actual_share} != expected {expected_share}",
                        dir.path.display()
                    );
                }
            }

            // 2. Relative spatial ordering of every surviving top-level pair must never flip!
            let surviving: Vec<SlotKey> = initial_top
                .keys()
                .copied()
                .filter(|k| now_top.contains_key(k))
                .collect();
            for &k1 in &surviving {
                for &k2 in &surviving {
                    if k1 == k2 {
                        continue;
                    }
                    let (b1, b2) = (initial_top[&k1], initial_top[&k2]);
                    let (a1, a2) = (now_top[&k1], now_top[&k2]);
                    let was_left = b1.x + b1.w <= b2.x + 1e-2;
                    let was_right = b2.x + b2.w <= b1.x + 1e-2;
                    let was_above = b1.y + b1.h <= b2.y + 1e-2;
                    let was_below = b2.y + b2.h <= b1.y + 1e-2;
                    let is_left = a1.x + a1.w <= a2.x + 1e-1;
                    let is_right = a2.x + a2.w <= a1.x + 1e-1;
                    let is_above = a1.y + a1.h <= a2.y + 1e-1;
                    let is_below = a2.y + a2.h <= a1.y + 1e-1;
                    if was_left && !was_above && !was_below {
                        assert!(is_left, "step={step}: horizontal order flipped");
                    }
                    if was_above && !was_left && !was_right {
                        assert!(is_above, "step={step}: vertical order flipped");
                    }
                    if was_left && was_above {
                        assert!(
                            (is_left || is_above) && !is_right && !is_below,
                            "step={step}: top-left/bottom-right order flipped"
                        );
                    }
                    if was_left && was_below {
                        assert!(
                            (is_left || is_below) && !is_right && !is_above,
                            "step={step}: bottom-left/top-right order flipped"
                        );
                    }
                }
            }
        }
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
        let mut layout = LayoutTree::build(&scene, bounds, 1.0, &token, 1).unwrap();

        // Delete leaf1 -> leaf2 expands cleanly across root.
        let dirty = scene.remove_directory(Path::new("/disk/leaf1"), FileIdentity::default());
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        assert!(!diff.dirty_rects.is_empty());
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
        assert!(!diff.dirty_rects.is_empty());
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
        let mut layout = LayoutTree::build(&scene, bounds, 1.0, &token, 1).unwrap();
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
            ("/disk/a/f1", 300, false),
            ("/disk/a/sub/f2", 500, true),
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
        let mut layout = LayoutTree::build(&scene, bounds, 1.0, &token, 1).unwrap();
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

    #[test]
    fn streaming_scan_stays_squarish_and_settles_without_final_scramble() {
        let root = PathBuf::from("/disk");
        let root_id = FileIdentity {
            device: 1,
            inode: 1,
        };
        let mut scene = SceneTree::empty_for_stream(root.clone(), root.clone(), root_id);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1600.0,
            h: 900.0,
        };
        let token = AtomicU64::new(1);

        // Batch 1: early discovery where dir-0 starts large and dir-2 starts tiny.
        let make_batch = |batch_idx: u64, dirs_and_sizes: &[(&str, u64, u64)]| -> scan::ScanBatch {
            let mut directories = Vec::new();
            let mut files = Vec::new();
            if batch_idx == 0 {
                directories.push(scan::DirectoryFact {
                    path: root.clone(),
                    identity: root_id,
                    enumeration_complete: true,
                    has_subdirectories: true,
                    excluded: false,
                });
            }
            for &(name, dir_ino, size) in dirs_and_sizes {
                let dpath = root.join(name);
                let did = FileIdentity {
                    device: 1,
                    inode: dir_ino.into(),
                };
                directories.push(scan::DirectoryFact {
                    path: dpath.clone(),
                    identity: did,
                    enumeration_complete: true,
                    has_subdirectories: false,
                    excluded: false,
                });
                files.push(scan::FileEntry {
                    path: dpath.join(format!("b{batch_idx}.bin")),
                    parent: Arc::from(dpath.as_path()),
                    size,
                    identity: FileIdentity {
                        device: 1,
                        inode: (batch_idx * 1000 + dir_ino).into(),
                    },
                    parent_identity: did,
                    parent_is_leaf: true,
                    can_trash: true,
                });
            }
            scan::ScanBatch {
                directories,
                files,
                errors: Vec::new(),
                skipped: 0,
                elapsed: Duration::from_millis(batch_idx * 200),
            }
        };

        let _ = scene.apply_stream_batch(make_batch(
            0,
            &[("d0", 10, 50_000), ("d1", 11, 10_000), ("d2", 12, 5_000)],
        ));
        let mut layout = LayoutTree::build(&scene, bounds, 1.0, &token, 1).unwrap();

        // Simulate 15 streaming batches where multiple dirs grow at different
        // rates (d2 and d5 become large, 15 new dirs arrive mid-scan).
        for b in 1..=15u64 {
            let mut batch_items = vec![
                ("d0", 10, 120_000),
                ("d1", 11, 80_000),
                ("d2", 12, 160_000 * b.min(8)),
            ];
            if b >= 4 {
                batch_items.push(("d5", 15, 220_000));
            }
            let dynamic_name = format!("d{}", b + 2);
            let leaked_name: &str = &dynamic_name;
            batch_items.push((leaked_name, 12 + b, 90_000 + (b * 37_000) % 240_000));
            let dirty = scene.apply_stream_batch(make_batch(b, &batch_items));
            let _ = layout
                .apply_dirty_streaming(&scene, &dirty, 1.0, &token, 1)
                .unwrap();
        }

        // Snapshot positions before final ~5% batch.
        let before_final: HashMap<PathBuf, Rect> = layout
            .collect_visible_tiles(&scene)
            .into_values()
            .map(|t| (t.path, t.rect))
            .collect();

        // Final small batch (last ~5% of scan): layout is already squarish -> zero scramble!
        let dirty = scene.apply_stream_batch(make_batch(16, &[("d2", 12, 100_000)]));
        let _ = layout
            .apply_dirty_streaming(&scene, &dirty, 1.0, &token, 1)
            .unwrap();

        let after_final: HashMap<PathBuf, Rect> = layout
            .collect_visible_tiles(&scene)
            .into_values()
            .map(|t| (t.path, t.rect))
            .collect();

        let fresh_opt = LayoutTree::build(&scene, bounds, 1.0, &token, 1)
            .unwrap()
            .collect_visible_tiles(&scene);
        let fresh_wa = weighted_aspect(fresh_opt.values().map(|t| t.rect));
        let final_wa = weighted_aspect(after_final.values().copied());
        assert!(
            final_wa < 1.55 && final_wa <= fresh_wa * 1.15,
            "expected near-optimal squarish layout after streaming (got {final_wa:.2}, fresh opt {fresh_wa:.2})"
        );
        // Verify zero horizontal/vertical order flip on the final batch!
        for (p1, &b1) in &before_final {
            for (p2, &b2) in &before_final {
                if p1 == p2 {
                    continue;
                }
                let (a1, a2) = (after_final[p1], after_final[p2]);
                let was_left = b1.x + b1.w <= b2.x + 1e-2;
                let was_above = b1.y + b1.h <= b2.y + 1e-2;
                let was_right = b2.x + b2.w <= b1.x + 1e-2;
                let was_below = b2.y + b2.h <= b1.y + 1e-2;
                if was_left && !was_above && !was_below {
                    assert!(a1.x + a1.w <= a2.x + 1e-1);
                }
                if was_above && !was_left && !was_right {
                    assert!(a1.y + a1.h <= a2.y + 1e-1);
                }
            }
        }
    }

    #[test]
    fn subpixel_lod_prunes_tiny_subtrees_and_unfolds_when_enlarged() {
        // Huge sibling (`100_000_000`) squeezes `tiny` (`100`) to < 1px, so its
        // nested subdirectories must be pruned from LayoutTree rather than expanded.
        let scan = make_scan(&[
            ("/disk/huge/blob.bin", 100_000_000, true),
            ("/disk/tiny/sub1/a.txt", 40, true),
            ("/disk/tiny/sub2/b.txt", 60, true),
        ]);
        let mut scene = SceneTree::from_scan(scan);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 600.0,
        };
        let token = AtomicU64::new(1);
        let mut layout = LayoutTree::build(&scene, bounds, 1.0, &token, 1).unwrap();
        let tiny_idx = scene.dir_by_path[Path::new("/disk/tiny")];
        assert!(
            !layout.dirs.contains_key(&tiny_idx),
            "sub-pixel directory should be LOD-collapsed instead of expanded"
        );

        // Removing `/disk/huge/blob.bin` makes `/disk/tiny` fill the whole canvas -> unfolds!
        let dirty = scene.remove_file(Path::new("/disk/huge/blob.bin"), FileIdentity::default());
        let diff = layout.apply_dirty(&scene, &dirty, 1.0, &token, 1).unwrap();
        assert!(!diff.dirty_rects.is_empty());
        assert!(
            layout.dirs.contains_key(&tiny_idx),
            "previously LOD-collapsed directory must unfold once enlarged"
        );
        let tiles = layout.collect_visible_tiles(&scene);
        assert!(
            tiles
                .values()
                .any(|t| t.path == Path::new("/disk/tiny/sub1"))
        );
        assert!(
            tiles
                .values()
                .any(|t| t.path == Path::new("/disk/tiny/sub2"))
        );
    }
}
