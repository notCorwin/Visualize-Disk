use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitAxis {
    /// Splits width: `left` gets `[x, x + w * f]`, `right` gets `[x + w * f, x + w]`.
    Vertical,
    /// Splits height: `left` gets `[y, y + h * f]`, `right` gets `[y + h * f, y + h]`.
    Horizontal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncStatus {
    Kept,
    Added,
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyncEntry<K> {
    pub key: K,
    pub from: Rect,
    pub to: Rect,
    pub status: SyncStatus,
}

#[derive(Clone, Debug)]
struct SplitNode<K> {
    weight: u64,
    kind: SplitKind<K>,
}

#[derive(Clone, Debug)]
enum SplitKind<K> {
    Leaf(K),
    Branch {
        axis: SplitAxis,
        left: u32,
        right: u32,
    },
}

/// Persistent binary space-partitioning treemap.
///
/// Initial construction sorts by weight descending and chooses balanced 50/50
/// splits along the longer axis for clean aspect ratios. Every subsequent
/// incremental update (resize, insertion, deletion) preserves the existing split
/// topology and split axes so surviving items never reorder or flip 90 degrees,
/// while updating every branch's split fraction to exact byte proportions.
#[derive(Clone, Debug)]
pub struct SplitTree<K> {
    nodes: Vec<SplitNode<K>>,
    root: Option<u32>,
}

impl<K> Default for SplitTree<K> {
    fn default() -> Self {
        Self::empty()
    }
}

impl<K> SplitTree<K> {
    pub fn empty() -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }
}

impl<K: Copy + Eq + Hash + Send> SplitTree<K> {
    /// Build a balanced initial `SplitTree` for `items` in `bounds`.
    pub fn build_with_cancel(
        items: &[(K, u64)],
        bounds: Rect,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Self> {
        Self::build_inner(items, bounds, Some((token, generation)))
    }

    #[cfg(test)]
    pub fn build(items: &[(K, u64)], bounds: Rect) -> Self {
        Self::build_inner(items, bounds, None).unwrap_or_else(Self::empty)
    }

    fn build_inner(items: &[(K, u64)], bounds: Rect, token: Generation<'_>) -> Option<Self> {
        if cancelled(token) {
            return None;
        }
        if items.is_empty() || !rect_non_negative(bounds) {
            return Some(Self::empty());
        }
        let mut ordered: Vec<(usize, K, u64)> = items
            .iter()
            .enumerate()
            .map(|(i, &(k, w))| (i, k, w.max(1)))
            .collect();
        if ordered.len() >= 4096 {
            ordered.par_sort_unstable_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        } else {
            ordered.sort_unstable_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        }
        if cancelled(token) {
            return None;
        }
        let mut nodes = Vec::with_capacity(ordered.len().saturating_mul(2).saturating_sub(1));
        let root = build_nodes(&ordered, bounds, &mut nodes, token)?;
        Some(Self {
            nodes,
            root: Some(root),
        })
    }

    /// Evaluate every leaf rectangle in `bounds` using the stored weights.
    pub fn evaluate(&self, bounds: Rect) -> Vec<(K, Rect)> {
        let Some(root) = self.root else {
            return Vec::new();
        };
        if !rect_non_negative(bounds) {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(self.nodes.len().div_ceil(2));
        evaluate_nodes(
            &self.nodes,
            root,
            bounds,
            |i| self.nodes[i].weight,
            &mut out,
        );
        out
    }

    /// Synchronize the persistent split tree to `desired` items without
    /// reordering or flipping any surviving item, returning paired `(from, to)`
    /// rectangles for every kept, added, and removed leaf.
    ///
    /// Both `from` and `to` are evaluated from the same augmented tree using
    /// `before_weights` (added = 0) and `after_weights` (removed = 0), so
    /// linearly interpolating `(from -> to)` keeps all shared borders flush
    /// with zero overlaps and zero gaps at every frame.
    pub fn sync_and_evaluate(
        &mut self,
        desired: &[(K, u64)],
        old_bounds: Rect,
        new_bounds: Rect,
        token: &AtomicU64,
        generation: u64,
    ) -> Option<Vec<SyncEntry<K>>> {
        self.sync_inner(desired, old_bounds, new_bounds, Some((token, generation)))
    }

    #[cfg(test)]
    pub fn sync(
        &mut self,
        desired: &[(K, u64)],
        old_bounds: Rect,
        new_bounds: Rect,
    ) -> Vec<SyncEntry<K>> {
        self.sync_inner(desired, old_bounds, new_bounds, None)
            .unwrap_or_default()
    }

    fn sync_inner(
        &mut self,
        desired: &[(K, u64)],
        old_bounds: Rect,
        new_bounds: Rect,
        token: Generation<'_>,
    ) -> Option<Vec<SyncEntry<K>>> {
        if cancelled(token) {
            return None;
        }
        // Case 1: Previously empty tree.
        if self.root.is_none() {
            if desired.is_empty() {
                return Some(Vec::new());
            }
            let built = Self::build_inner(desired, new_bounds, token)?;
            let froms = built.evaluate(old_bounds);
            let tos = built.evaluate(new_bounds);
            *self = built;
            let entries = froms
                .into_iter()
                .zip(tos)
                .map(|((k, from), (_, to))| SyncEntry {
                    key: k,
                    from,
                    to,
                    status: SyncStatus::Added,
                })
                .collect();
            return Some(entries);
        }

        // Case 2: All items removed.
        if desired.is_empty() {
            let froms = self.evaluate(old_bounds);
            let tos = self.evaluate(new_bounds);
            self.nodes.clear();
            self.root = None;
            let entries = froms
                .into_iter()
                .zip(tos)
                .map(|((k, from), (_, to))| SyncEntry {
                    key: k,
                    from,
                    to,
                    status: SyncStatus::Removed,
                })
                .collect();
            return Some(entries);
        }

        let desired_map: HashMap<K, u64> = desired.iter().map(|&(k, w)| (k, w.max(1))).collect();

        // Fast path: check if topology is completely unchanged (weight-only update).
        let mut leaf_count = 0usize;
        let mut all_existing_kept = true;
        for node in &self.nodes {
            if let SplitKind::Leaf(k) = node.kind {
                leaf_count += 1;
                if !desired_map.contains_key(&k) {
                    all_existing_kept = false;
                    break;
                }
            }
        }
        if all_existing_kept && leaf_count == desired_map.len() {
            let root = self.root.unwrap();
            let mut froms = Vec::with_capacity(leaf_count);
            evaluate_nodes(
                &self.nodes,
                root,
                old_bounds,
                |i| self.nodes[i].weight,
                &mut froms,
            );
            let post = post_order(&self.nodes, root);
            for idx in post {
                match self.nodes[idx].kind {
                    SplitKind::Leaf(k) => {
                        self.nodes[idx].weight = desired_map[&k];
                    }
                    SplitKind::Branch { left, right, .. } => {
                        self.nodes[idx].weight = self.nodes[left as usize]
                            .weight
                            .saturating_add(self.nodes[right as usize].weight);
                    }
                }
            }
            let mut tos = Vec::with_capacity(leaf_count);
            evaluate_nodes(
                &self.nodes,
                root,
                new_bounds,
                |i| self.nodes[i].weight,
                &mut tos,
            );
            let entries = froms
                .into_iter()
                .zip(tos)
                .map(|((k, from), (_, to))| SyncEntry {
                    key: k,
                    from,
                    to,
                    status: SyncStatus::Kept,
                })
                .collect();
            return Some(entries);
        }

        // General path: structural additions and/or removals.
        let mut existing_keys = HashSet::with_capacity(self.nodes.len().div_ceil(2));
        for node in &self.nodes {
            if let SplitKind::Leaf(k) = node.kind {
                existing_keys.insert(k);
            }
        }
        let added: Vec<(K, u64)> = desired
            .iter()
            .filter_map(|&(k, w)| (!existing_keys.contains(&k)).then_some((k, w.max(1))))
            .collect();

        let mut before_weights: Vec<u64> = self.nodes.iter().map(|n| n.weight).collect();

        // Splice newly added items along the right-most spine at the depth that
        // gives the post-insertion rectangle the squarest aspect ratio, with
        // `before_weight = 0` so existing items keep their exact `before`
        // geometry and never reorder or flip axes.
        if !added.is_empty() {
            let added_total: u64 = added
                .iter()
                .map(|(_, w)| *w)
                .fold(0u64, u64::saturating_add);
            let mut cur = self.root.unwrap() as usize;
            let mut cur_post_rect = new_bounds;
            while let SplitKind::Branch { axis, left, right } = self.nodes[cur].kind {
                let lw = before_weights[left as usize];
                let rw = before_weights[right as usize];
                // Compare splitting HERE vs descending into `right` using the
                // exact post-insertion rects of BOTH `right` and `added`.
                let here_axis = if cur_post_rect.w >= cur_post_rect.h {
                    SplitAxis::Vertical
                } else {
                    SplitAxis::Horizontal
                };
                let (existing_if_here, new_if_here) = split_rect_by_weights(
                    cur_post_rect,
                    here_axis,
                    before_weights[cur],
                    added_total,
                );
                let (_, right_if_here) = split_rect_by_weights(existing_if_here, axis, lw, rw);
                let worst_here = rect_aspect(right_if_here).max(rect_aspect(new_if_here));

                let (_, post_right_rect) =
                    split_rect_by_weights(cur_post_rect, axis, lw, rw.saturating_add(added_total));
                let worst_deeper = candidate_split_aspect(post_right_rect, rw, added_total);
                if worst_deeper >= worst_here {
                    break;
                }
                cur = right as usize;
                cur_post_rect = post_right_rect;
            }

            let axis = if cur_post_rect.w >= cur_post_rect.h {
                SplitAxis::Vertical
            } else {
                SplitAxis::Horizontal
            };
            let (_, prospective_rect) =
                split_rect_by_weights(cur_post_rect, axis, before_weights[cur], added_total);
            let sub = Self::build_inner(&added, prospective_rect, token)?;
            let sub_root = sub.root.expect("non-empty added list must build a root");

            let moved_cur = self.nodes.len() as u32;
            let cur_clone = self.nodes[cur].clone();
            let cur_before_w = before_weights[cur];
            self.nodes.push(cur_clone);
            before_weights.push(cur_before_w);

            let base_offset = self.nodes.len() as u32;
            for node in sub.nodes {
                let shifted_kind = match node.kind {
                    SplitKind::Leaf(k) => SplitKind::Leaf(k),
                    SplitKind::Branch { axis, left, right } => SplitKind::Branch {
                        axis,
                        left: left + base_offset,
                        right: right + base_offset,
                    },
                };
                self.nodes.push(SplitNode {
                    weight: node.weight,
                    kind: shifted_kind,
                });
                before_weights.push(0);
            }
            self.nodes[cur] = SplitNode {
                weight: 0,
                kind: SplitKind::Branch {
                    axis,
                    left: moved_cur,
                    right: sub_root + base_offset,
                },
            };
            // `before_weights[cur]` stays `cur_before_w + 0 == cur_before_w`,
            // so every ancestor's `before_weight` remains exact!
        }

        if cancelled(token) {
            return None;
        }

        let root = self.root.unwrap();
        let order = post_order(&self.nodes, root);
        let mut after_weights = vec![0u64; self.nodes.len()];
        for &idx in &order {
            after_weights[idx] = match self.nodes[idx].kind {
                SplitKind::Leaf(k) => desired_map.get(&k).copied().unwrap_or(0),
                SplitKind::Branch { left, right, .. } => {
                    after_weights[left as usize].saturating_add(after_weights[right as usize])
                }
            };
        }

        let approx_leaves = self.nodes.len().div_ceil(2);
        let mut froms = Vec::with_capacity(approx_leaves);
        let mut tos = Vec::with_capacity(approx_leaves);
        evaluate_nodes(
            &self.nodes,
            root,
            old_bounds,
            |i| before_weights[i],
            &mut froms,
        );
        evaluate_nodes(
            &self.nodes,
            root,
            new_bounds,
            |i| after_weights[i],
            &mut tos,
        );

        let mut leaf_statuses = Vec::with_capacity(froms.len());
        collect_leaf_statuses(
            &self.nodes,
            root,
            &before_weights,
            &after_weights,
            &mut leaf_statuses,
        );

        let entries: Vec<SyncEntry<K>> = froms
            .into_iter()
            .zip(tos)
            .zip(leaf_statuses)
            .map(|(((k, from), (_, to)), status)| SyncEntry {
                key: k,
                from,
                to,
                status,
            })
            .collect();

        // Prune zero-after-weight nodes and compact the arena.
        let mut remapped: Vec<Option<u32>> = vec![None; self.nodes.len()];
        let mut compacted: Vec<SplitNode<K>> = Vec::with_capacity(desired.len().saturating_mul(2));
        for idx in order {
            let w = after_weights[idx];
            if w == 0 {
                continue;
            }
            match self.nodes[idx].kind {
                SplitKind::Leaf(k) => {
                    let new_id = compacted.len() as u32;
                    compacted.push(SplitNode {
                        weight: w,
                        kind: SplitKind::Leaf(k),
                    });
                    remapped[idx] = Some(new_id);
                }
                SplitKind::Branch { axis, left, right } => {
                    match (remapped[left as usize], remapped[right as usize]) {
                        (Some(nl), Some(nr)) => {
                            let new_id = compacted.len() as u32;
                            compacted.push(SplitNode {
                                weight: w,
                                kind: SplitKind::Branch {
                                    axis,
                                    left: nl,
                                    right: nr,
                                },
                            });
                            remapped[idx] = Some(new_id);
                        }
                        (Some(nl), None) => remapped[idx] = Some(nl),
                        (None, Some(nr)) => remapped[idx] = Some(nr),
                        (None, None) => {}
                    }
                }
            }
        }
        self.root = remapped[root as usize];
        self.nodes = compacted;

        Some(entries)
    }
}

fn rect_non_negative(r: Rect) -> bool {
    r.x.is_finite()
        && r.y.is_finite()
        && r.w.is_finite()
        && r.h.is_finite()
        && r.w >= 0.0
        && r.h >= 0.0
}

fn split_rect_by_weights(rect: Rect, axis: SplitAxis, left_w: u64, right_w: u64) -> (Rect, Rect) {
    let total = left_w as f64 + right_w as f64;
    let fraction = if total <= 0.0 {
        0.5
    } else if left_w == 0 {
        0.0
    } else if right_w == 0 {
        1.0
    } else {
        ((left_w as f64 / total) as f32).clamp(f32::EPSILON, 1.0 - f32::EPSILON)
    };
    match axis {
        SplitAxis::Vertical => {
            let width = rect.w * fraction;
            (
                Rect { w: width, ..rect },
                Rect {
                    x: rect.x + width,
                    w: (rect.w - width).max(0.0),
                    ..rect
                },
            )
        }
        SplitAxis::Horizontal => {
            let height = rect.h * fraction;
            (
                Rect { h: height, ..rect },
                Rect {
                    y: rect.y + height,
                    h: (rect.h - height).max(0.0),
                    ..rect
                },
            )
        }
    }
}

fn candidate_split_aspect(rect: Rect, existing_w: u64, added_w: u64) -> f64 {
    let axis = if rect.w >= rect.h {
        SplitAxis::Vertical
    } else {
        SplitAxis::Horizontal
    };
    let (existing_r, new_r) = split_rect_by_weights(rect, axis, existing_w, added_w);
    rect_aspect(existing_r).max(rect_aspect(new_r))
}

pub fn rect_aspect(r: Rect) -> f64 {
    let w = r.w as f64;
    let h = r.h as f64;
    if w <= 1e-6 || h <= 1e-6 {
        return f64::INFINITY;
    }
    (w / h).max(h / w)
}

fn strip_worst_aspect(
    row_w: f64,
    max_item: f64,
    min_item: f64,
    total_w: f64,
    short: f64,
    long: f64,
) -> f64 {
    if row_w <= 0.0 || min_item <= 0.0 || max_item <= 0.0 || short <= 0.0 || long <= 0.0 {
        return f64::INFINITY;
    }
    let s2 = row_w * row_w;
    let coeff = long / (short * total_w);
    let r_min = coeff * s2 / min_item;
    let r_max = coeff * s2 / max_item;
    r_min.max(1.0 / r_max)
}

/// Build a balanced binary subtree for a 1D squarified strip whose items are
/// stacked along `axis`, writing its root directly into `root_slot`.
fn build_strip_nodes_into<K: Copy>(
    root_slot: u32,
    slice: &[(usize, K, u64)],
    prefix_u64: &[u64],
    base_idx: usize,
    axis: SplitAxis,
    nodes: &mut Vec<SplitNode<K>>,
) {
    let mut stack = vec![(root_slot, 0usize, slice.len())];
    while let Some((slot, lo, hi)) = stack.pop() {
        let len = hi - lo;
        let total = prefix_u64[base_idx + hi].saturating_sub(prefix_u64[base_idx + lo]);
        if len == 1 {
            nodes[slot as usize] = SplitNode {
                weight: slice[lo].2,
                kind: SplitKind::Leaf(slice[lo].1),
            };
            continue;
        }
        let mid = lo + len / 2;
        let left_slot = nodes.len() as u32;
        nodes.push(SplitNode {
            weight: 0,
            kind: SplitKind::Leaf(slice[lo].1),
        });
        let right_slot = nodes.len() as u32;
        nodes.push(SplitNode {
            weight: 0,
            kind: SplitKind::Leaf(slice[mid].1),
        });
        nodes[slot as usize] = SplitNode {
            weight: total,
            kind: SplitKind::Branch {
                axis,
                left: left_slot,
                right: right_slot,
            },
        };
        stack.push((right_slot, mid, hi));
        stack.push((left_slot, lo, mid));
    }
}

/// Build a Squarified Treemap (Bruls, Huizing, van Wijk) directly as a
/// persistent binary `SplitTree` in O(n) partitioning time.
fn build_nodes<K: Copy>(
    ordered: &[(usize, K, u64)],
    bounds: Rect,
    nodes: &mut Vec<SplitNode<K>>,
    token: Generation<'_>,
) -> Option<u32> {
    let n = ordered.len();
    if n == 0 {
        return None;
    }
    if n == 1 {
        let idx = nodes.len() as u32;
        nodes.push(SplitNode {
            weight: ordered[0].2,
            kind: SplitKind::Leaf(ordered[0].1),
        });
        return Some(idx);
    }

    let mut prefix_f64 = Vec::with_capacity(n + 1);
    let mut prefix_u64 = Vec::with_capacity(n + 1);
    prefix_f64.push(0.0);
    prefix_u64.push(0);
    let mut acc_f64 = 0.0f64;
    let mut acc_u64 = 0u64;
    for (i, &(_, _, w)) in ordered.iter().enumerate() {
        if i.is_multiple_of(4096) && cancelled(token) {
            return None;
        }
        acc_f64 += w as f64;
        acc_u64 = acc_u64.saturating_add(w);
        prefix_f64.push(acc_f64);
        prefix_u64.push(acc_u64);
    }

    let root_slot = nodes.len() as u32;
    nodes.push(SplitNode {
        weight: 0,
        kind: SplitKind::Leaf(ordered[0].1),
    });

    let mut slot = root_slot;
    let mut start = 0usize;
    let mut rect = bounds;
    let mut strips = 0usize;

    while start < n {
        if strips.is_multiple_of(128) && cancelled(token) {
            return None;
        }
        strips += 1;

        let remaining = n - start;
        if remaining == 1 {
            nodes[slot as usize] = SplitNode {
                weight: ordered[start].2,
                kind: SplitKind::Leaf(ordered[start].1),
            };
            break;
        }

        let total_f64 = (prefix_f64[n] - prefix_f64[start]).max(1.0);
        let total_u64 = prefix_u64[n].saturating_sub(prefix_u64[start]);
        let (outer_axis, perp_axis, short, long) = if rect.w >= rect.h {
            (
                SplitAxis::Vertical,
                SplitAxis::Horizontal,
                (rect.h as f64).max(1e-6),
                (rect.w as f64).max(1e-6),
            )
        } else {
            (
                SplitAxis::Horizontal,
                SplitAxis::Vertical,
                (rect.w as f64).max(1e-6),
                (rect.h as f64).max(1e-6),
            )
        };

        // Greedily grow the strip along the shorter side while worst aspect improves.
        let max_item = ordered[start].2 as f64;
        let mut count = 1usize;
        let mut row_w = max_item;
        let mut best_worst = strip_worst_aspect(row_w, max_item, max_item, total_f64, short, long);

        while start + count < n {
            let next_item = ordered[start + count].2 as f64;
            let next_row_w = row_w + next_item;
            let next_worst =
                strip_worst_aspect(next_row_w, max_item, next_item, total_f64, short, long);
            if next_worst <= best_worst {
                count += 1;
                row_w = next_row_w;
                best_worst = next_worst;
            } else {
                break;
            }
        }

        if start + count == n {
            // All remaining items belong to this final 1D strip.
            build_strip_nodes_into(
                slot,
                &ordered[start..n],
                &prefix_u64,
                start,
                perp_axis,
                nodes,
            );
            break;
        }

        let strip_u64 = prefix_u64[start + count].saturating_sub(prefix_u64[start]);
        let rest_u64 = total_u64.saturating_sub(strip_u64);
        let (_, rest_rect) = split_rect_by_weights(rect, outer_axis, strip_u64, rest_u64);

        let left_slot = nodes.len() as u32;
        nodes.push(SplitNode {
            weight: 0,
            kind: SplitKind::Leaf(ordered[start].1),
        });
        build_strip_nodes_into(
            left_slot,
            &ordered[start..start + count],
            &prefix_u64,
            start,
            perp_axis,
            nodes,
        );
        let right_slot = nodes.len() as u32;
        nodes.push(SplitNode {
            weight: 0,
            kind: SplitKind::Leaf(ordered[start + count].1),
        });
        nodes[slot as usize] = SplitNode {
            weight: total_u64,
            kind: SplitKind::Branch {
                axis: outer_axis,
                left: left_slot,
                right: right_slot,
            },
        };

        slot = right_slot;
        start += count;
        rect = rest_rect;
    }

    (!cancelled(token)).then_some(root_slot)
}

fn post_order<K>(nodes: &[SplitNode<K>], root: u32) -> Vec<usize> {
    let mut order = Vec::with_capacity(nodes.len());
    let mut stack = vec![root as usize];
    while let Some(idx) = stack.pop() {
        order.push(idx);
        if let SplitKind::Branch { left, right, .. } = nodes[idx].kind {
            stack.push(left as usize);
            stack.push(right as usize);
        }
    }
    order.reverse();
    order
}

fn evaluate_nodes<K: Copy, F: Fn(usize) -> u64>(
    nodes: &[SplitNode<K>],
    root: u32,
    bounds: Rect,
    weight_of: F,
    out: &mut Vec<(K, Rect)>,
) {
    let mut stack = vec![(root as usize, bounds)];
    while let Some((idx, rect)) = stack.pop() {
        match nodes[idx].kind {
            SplitKind::Leaf(key) => out.push((key, rect)),
            SplitKind::Branch { axis, left, right } => {
                let (a, b) = split_rect_by_weights(
                    rect,
                    axis,
                    weight_of(left as usize),
                    weight_of(right as usize),
                );
                stack.push((right as usize, b));
                stack.push((left as usize, a));
            }
        }
    }
}

fn collect_leaf_statuses<K>(
    nodes: &[SplitNode<K>],
    root: u32,
    before_weights: &[u64],
    after_weights: &[u64],
    out: &mut Vec<SyncStatus>,
) {
    let mut stack = vec![root as usize];
    while let Some(idx) = stack.pop() {
        match nodes[idx].kind {
            SplitKind::Leaf(_) => {
                let bw = before_weights[idx];
                let aw = after_weights[idx];
                let status = if bw == 0 {
                    SyncStatus::Added
                } else if aw == 0 {
                    SyncStatus::Removed
                } else {
                    SyncStatus::Kept
                };
                out.push(status);
            }
            SplitKind::Branch { left, right, .. } => {
                stack.push(right as usize);
                stack.push(left as usize);
            }
        }
    }
}

/// A balanced binary treemap. Each returned rectangle identifies exactly one
/// input file; small files are never combined into an aggregate rectangle.
/// Empty files receive one unit of layout weight while their byte size remains
/// zero. Subpixel rectangles remain in the result for zoom and hit testing.
#[cfg(test)]
pub fn layout(items: &[(usize, u64)], bounds: Rect) -> Vec<(usize, Rect)> {
    layout_inner(items, bounds, None).unwrap_or_default()
}

/// Stale generations return no partial geometry. All work shares the global
/// Rayon pool; cancellation never creates another worker pool or polling thread.
#[cfg(test)]
pub fn layout_with_cancel(
    items: &[(usize, u64)],
    bounds: Rect,
    token: &AtomicU64,
    generation: u64,
) -> Option<Vec<(usize, Rect)>> {
    layout_inner(items, bounds, Some((token, generation)))
}

type Generation<'a> = Option<(&'a AtomicU64, u64)>;
fn cancelled(token: Generation<'_>) -> bool {
    token.is_some_and(|(token, generation)| token.load(Ordering::Relaxed) != generation)
}

#[cfg(test)]
fn layout_inner(
    items: &[(usize, u64)],
    bounds: Rect,
    token: Generation<'_>,
) -> Option<Vec<(usize, Rect)>> {
    if cancelled(token) {
        return None;
    }
    if items.is_empty()
        || !bounds.x.is_finite()
        || !bounds.y.is_finite()
        || !bounds.w.is_finite()
        || !bounds.h.is_finite()
        || bounds.w <= 0.0
        || bounds.h <= 0.0
    {
        return Some(Vec::new());
    }
    let mut ordered = items.to_vec();
    ordered.par_sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if cancelled(token) {
        return None;
    }
    let mut result = vec![(0, Rect::default()); items.len()];
    if fill(&ordered, bounds, &mut result, 0, token) {
        Some(result)
    } else {
        None
    }
}

#[cfg(test)]
fn fill(
    items: &[(usize, u64)],
    rect: Rect,
    output: &mut [(usize, Rect)],
    depth: u8,
    token: Generation<'_>,
) -> bool {
    if cancelled(token) {
        return false;
    }
    if items.len() < 4096 || depth >= 32 {
        return fill_serial(items, rect, output, token);
    }
    let Some((split, a, b)) = partition(items, rect, token) else {
        return false;
    };
    let (left, right) = output.split_at_mut(split);
    let (a, b) = rayon::join(
        || fill(&items[..split], a, left, depth + 1, token),
        || fill(&items[split..], b, right, depth + 1, token),
    );
    a && b
}

#[cfg(test)]
fn fill_serial(
    items: &[(usize, u64)],
    bounds: Rect,
    output: &mut [(usize, Rect)],
    token: Generation<'_>,
) -> bool {
    // Iteration avoids stack overflow even for pathological size distributions.
    let mut pending = vec![(items, bounds, 0)];
    let mut processed = 0usize;
    while let Some((slice, rect, offset)) = pending.pop() {
        if processed % 128 == 0 && cancelled(token) {
            return false;
        }
        processed += 1;
        if slice.len() == 1 {
            output[offset] = (slice[0].0, rect);
            continue;
        }
        let Some((split, a, b)) = partition(slice, rect, token) else {
            return false;
        };
        pending.push((&slice[split..], b, offset + split));
        pending.push((&slice[..split], a, offset));
    }
    !cancelled(token)
}

#[cfg(test)]
fn partition(
    slice: &[(usize, u64)],
    rect: Rect,
    token: Generation<'_>,
) -> Option<(usize, Rect, Rect)> {
    let mut total = 0.0;
    for chunk in slice.chunks(4096) {
        if slice.len() >= 4096 && cancelled(token) {
            return None;
        }
        total += chunk
            .iter()
            .map(|(_, size)| (*size).max(1) as f64)
            .sum::<f64>();
    }
    let target = total / 2.0;
    let mut left = 0.0;
    let mut split = 1;
    for (index, (_, size)) in slice.iter().enumerate().take(slice.len() - 1) {
        if index % 4096 == 0 && slice.len() >= 4096 && cancelled(token) {
            return None;
        }
        let next = left + (*size).max(1) as f64;
        if index > 0 && (left - target).abs() <= (next - target).abs() {
            split = index;
            break;
        }
        left = next;
        split = index + 1;
        if left >= target {
            break;
        }
    }
    // Keep both branches representable below f32 area precision.
    let fraction = ((left / total) as f32).clamp(f32::EPSILON, 1.0 - f32::EPSILON);
    let (a, b) = if rect.w >= rect.h {
        let width = rect.w * fraction;
        (
            Rect { w: width, ..rect },
            Rect {
                x: rect.x + width,
                w: rect.w - width,
                ..rect
            },
        )
    } else {
        let height = rect.h * fraction;
        (
            Rect { h: height, ..rect },
            Rect {
                y: rect.y + height,
                h: rect.h - height,
                ..rect
            },
        )
    };
    Some((split, a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_files_area_and_proportions_without_overlaps() {
        let input = vec![(0, 31), (1, 17), (2, 11), (3, 5), (4, 1)];
        let bounds = Rect {
            x: 20.0,
            y: 30.0,
            w: 700.0,
            h: 400.0,
        };
        let result = layout(&input, bounds);
        assert_eq!(result.len(), input.len());
        for (id, r) in &result {
            assert!(r.x >= bounds.x && r.y >= bounds.y);
            assert!(r.x + r.w <= bounds.x + bounds.w + 0.01);
            assert!(r.y + r.h <= bounds.y + bounds.h + 0.01);
            let expected = input[*id].1 as f32 / 65.0;
            assert!((r.w * r.h / (bounds.w * bounds.h) - expected).abs() < 0.00001);
        }
        for (i, (_, a)) in result.iter().enumerate() {
            for (_, b) in result.iter().skip(i + 1) {
                let overlap_w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
                let overlap_h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
                assert!(overlap_w <= 0.001 || overlap_h <= 0.001);
            }
        }
    }

    #[test]
    fn retains_zero_byte_files_as_individual_rectangles() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let result = layout(&[(2, 0), (1, 0), (0, 0)], bounds);
        assert_eq!(result.len(), 3);
        assert!(result.iter().all(|(_, rect)| rect.w > 0.0 && rect.h > 0.0));
        assert!((result.iter().map(|(_, r)| r.w * r.h).sum::<f32>() - 10000.0).abs() < 0.1);
    }

    #[test]
    fn tiny_files_are_not_aggregated() {
        let mut files = vec![(0, u64::MAX)];
        files.extend((1..10001).map(|id| (id, 1)));
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 1000.0,
        };
        let result = layout(&files, bounds);
        assert_eq!(result.len(), files.len());
        assert!(
            result
                .iter()
                .all(|(_, r)| r.w.is_finite() && r.h.is_finite() && r.w > 0.0 && r.h > 0.0)
        );
    }

    #[test]
    fn ignores_empty_input_and_invalid_bounds() {
        assert!(layout(&[], Rect::default()).is_empty());
        assert!(
            layout(
                &[(0, 1)],
                Rect {
                    w: f32::NAN,
                    h: 1.0,
                    ..Rect::default()
                }
            )
            .is_empty()
        );
    }
    #[test]
    fn parallel_layout_matches_serial_geometry_for_every_file() {
        let mut items: Vec<_> = (0..32768).map(|i| (i, (i as u64 * 7919) % 65537)).collect();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1600.0,
            h: 1000.0,
        };
        let parallel = layout(&items, bounds);
        items.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let mut serial = vec![(0, Rect::default()); items.len()];
        assert!(fill_serial(&items, bounds, &mut serial, None));
        assert_eq!(parallel, serial);
    }
    #[test]
    fn stale_generation_never_publishes_partial_layout() {
        let token = AtomicU64::new(2);
        let items: Vec<_> = (0..8192).map(|id| (id, 1)).collect();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 1000.0,
        };
        assert!(layout_with_cancel(&items, bounds, &token, 1).is_none());
        assert_eq!(
            layout_with_cancel(&items, bounds, &token, 2).unwrap().len(),
            items.len()
        );
    }

    #[test]
    fn split_tree_updates_exact_proportions_without_reordering_on_resize() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 600.0,
        };
        let initial = vec![(0u32, 500), (1, 300), (2, 150), (3, 50)];
        let mut tree = SplitTree::build(&initial, bounds);
        let before: HashMap<u32, Rect> = tree.evaluate(bounds).into_iter().collect();

        // Make the smallest item (3) become by far the largest (2000), and shrink (0) to 100!
        let resized = vec![(0u32, 100), (1, 300), (2, 150), (3, 2000)];
        let entries = tree.sync(&resized, bounds, bounds);
        assert_eq!(entries.len(), 4);
        assert!(entries.iter().all(|e| e.status == SyncStatus::Kept));

        let after: HashMap<u32, Rect> = entries.iter().map(|e| (e.key, e.to)).collect();
        let total = 2550.0f32;
        for &(k, w) in &resized {
            let r = after[&k];
            let frac = (r.w * r.h) / (bounds.w * bounds.h);
            assert!(
                (frac - (w as f32 / total)).abs() < 1e-4,
                "key {k} proportion mismatch: got {frac}, expected {}",
                w as f32 / total
            );
        }

        // Spatial order MUST be preserved: if A was strictly left of B before,
        // A is still strictly left of B after; same for strictly above.
        for &a in &[0u32, 1, 2, 3] {
            for &b in &[0u32, 1, 2, 3] {
                if a == b {
                    continue;
                }
                let (ba, bb) = (before[&a], before[&b]);
                let (aa, ab) = (after[&a], after[&b]);
                if ba.x + ba.w <= bb.x + 1e-3 {
                    assert!(
                        aa.x + aa.w <= ab.x + 1e-2,
                        "horizontal order flipped between {a} and {b}"
                    );
                }
                if ba.y + ba.h <= bb.y + 1e-3 {
                    assert!(
                        aa.y + aa.h <= ab.y + 1e-2,
                        "vertical order flipped between {a} and {b}"
                    );
                }
            }
        }
    }

    #[test]
    fn split_tree_delete_and_insert_stay_flush_without_overlaps_at_midpoint() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 800.0,
            h: 600.0,
        };
        let mut tree = SplitTree::build(&[(0u32, 400), (1, 300), (2, 200), (3, 100)], bounds);

        // Delete item 0 and add item 4 simultaneously.
        let entries = tree.sync(&[(1, 300), (2, 200), (3, 100), (4, 400)], bounds, bounds);
        let removed = entries.iter().find(|e| e.key == 0).unwrap();
        let added = entries.iter().find(|e| e.key == 4).unwrap();
        assert_eq!(removed.status, SyncStatus::Removed);
        assert_eq!(added.status, SyncStatus::Added);
        assert!(removed.to.w * removed.to.h == 0.0);
        assert!(added.from.w * added.from.h == 0.0);

        // At every animation instant t in [0, 1], interpolated rects must tile
        // `bounds` completely with zero overlap and exact total area!
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            let rects: Vec<Rect> = entries
                .iter()
                .map(|e| Rect {
                    x: e.from.x + (e.to.x - e.from.x) * t,
                    y: e.from.y + (e.to.y - e.from.y) * t,
                    w: e.from.w + (e.to.w - e.from.w) * t,
                    h: e.from.h + (e.to.h - e.from.h) * t,
                })
                .collect();
            let area_sum: f32 = rects.iter().map(|r| r.w * r.h).sum();
            assert!(
                (area_sum - bounds.w * bounds.h).abs() < 1.0,
                "t={t}: area sum {area_sum} != {}",
                bounds.w * bounds.h
            );
            for (i, a) in rects.iter().enumerate() {
                for b in rects.iter().skip(i + 1) {
                    let ow = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
                    let oh = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
                    assert!(
                        ow <= 0.05 || oh <= 0.05,
                        "t={t}: overlap ({ow}, {oh}) between {a:?} and {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn split_tree_repeated_additions_maintain_valid_geometry_and_order() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 600.0,
        };
        let mut items: Vec<(u32, u64)> = (0..10).map(|i| (i, 100)).collect();
        let mut tree = SplitTree::build(&items, bounds);
        for next_id in 10..60u32 {
            items.push((next_id, 50 + (next_id as u64 * 17) % 100));
            let entries = tree.sync(&items, bounds, bounds);
            assert_eq!(entries.len(), items.len());
        }
        let final_rects = tree.evaluate(bounds);
        assert_eq!(final_rects.len(), items.len());
        let total_w: u64 = items.iter().map(|(_, w)| *w).sum();
        let weight_map: HashMap<u32, u64> = items.into_iter().collect();
        for (k, r) in &final_rects {
            assert!(r.w > 0.0 && r.h > 0.0);
            let frac = (r.w * r.h) / (bounds.w * bounds.h);
            let exp = weight_map[k] as f32 / total_w as f32;
            assert!((frac - exp).abs() < 1e-4);
        }
    }

    #[test]
    fn squarified_split_tree_produces_near_square_rectangles() {
        // Classic Bruls-Huizing-van Wijk paper example: [6, 6, 4, 3, 2, 2, 1] in 600x400.
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 400.0,
        };
        let items = vec![(0u32, 6), (1, 6), (2, 4), (3, 3), (4, 2), (5, 2), (6, 1)];
        let tree = SplitTree::build(&items, bounds);
        let rects = tree.evaluate(bounds);
        assert_eq!(rects.len(), items.len());
        let max_aspect = rects
            .iter()
            .map(|(_, r)| rect_aspect(*r))
            .fold(0.0f64, f64::max);
        assert!(
            max_aspect <= 3.05,
            "expected squarified max aspect <= 3.05, got {max_aspect}"
        );
        // Verify average aspect ratio is very close to 1.
        let avg_aspect =
            rects.iter().map(|(_, r)| rect_aspect(*r)).sum::<f64>() / rects.len() as f64;
        assert!(
            avg_aspect <= 1.85,
            "expected squarified avg aspect <= 1.85, got {avg_aspect}"
        );
    }

    #[test]
    fn post_insertion_spine_descent_rescues_small_siblings_without_reordering() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 1000.0,
        };
        // Initial layout: one huge left item (100) and one tiny right item (5).
        let mut tree = SplitTree::build(&[(0u32, 100), (1, 5)], bounds);
        let before: HashMap<u32, Rect> = tree.evaluate(bounds).into_iter().collect();
        // Item 1 starts skinny (~21:1). Now a 50-unit item 2 arrives!
        let entries = tree.sync(&[(0, 100), (1, 5), (2, 50)], bounds, bounds);
        let after: HashMap<u32, Rect> = entries.iter().map(|e| (e.key, e.to)).collect();

        // Item 0 MUST remain strictly left of item 1 (strict order preservation!).
        assert!(before[&0].x + before[&0].w <= before[&1].x + 1e-2);
        assert!(after[&0].x + after[&0].w <= after[&1].x + 1e-2);
        // Descending into the expanded right column stacks item 2 perpendicularly
        // with item 1, rescuing item 1 from ~31:1 down to ~4:1!
        assert!(
            rect_aspect(after[&1]) < 5.0,
            "expected item 1 to be rescued below 5:1, got {}",
            rect_aspect(after[&1])
        );
        assert!(
            rect_aspect(after[&2]) < 3.0,
            "expected item 2 to be squarish (<3:1), got {}",
            rect_aspect(after[&2])
        );
    }
}
