//! Stable geometry within one directory's existing content rectangle.
//!
//! A partition never moves its outer bounds during an ordinary update. Deleted
//! slots stay vacant, allowing a later sibling to reuse their pixels. When no
//! vacancy exists, one neighboring allocation is split. The caller owns
//! directory ancestry, platform geometry and propagation of area drift.
use crate::treemap::Rect;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

// Sorted, copy-on-write pages keep per-entry storage compact while allowing a
// transaction to clone and modify a single bounded page.  A flat Vec would
// make each insertion or deletion in a large directory move the entire tail;
// a per-entry B-tree costs considerably more memory for millions of files.
const PAGE_SIZE: usize = 256;

#[derive(Clone, Debug)]
struct Entries<K> {
    pages: Vec<Arc<Vec<(K, Placement)>>>,
}

impl<K: Ord + Clone> Entries<K> {
    fn from_unsorted(mut entries: Vec<(K, Placement)>) -> Result<Self, Error> {
        entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        if entries.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(Error::Duplicate);
        }
        let mut pages = Vec::with_capacity(entries.len().div_ceil(PAGE_SIZE));
        let mut source = entries.into_iter();
        loop {
            let page: Vec<_> = source.by_ref().take(PAGE_SIZE).collect();
            if page.is_empty() {
                break;
            }
            pages.push(Arc::new(page));
        }
        Ok(Self { pages })
    }

    fn page_index(&self, id: &K) -> usize {
        self.pages
            .partition_point(|page| page.last().is_some_and(|entry| &entry.0 < id))
    }
    fn get(&self, id: &K) -> Option<&Placement> {
        let page = self.pages.get(self.page_index(id))?;
        page.binary_search_by(|entry| entry.0.cmp(id))
            .ok()
            .map(|i| &page[i].1)
    }
    fn get_mut(&mut self, id: &K) -> Option<&mut Placement> {
        let index = self.page_index(id);
        let page = self.pages.get(index)?;
        let item = page.binary_search_by(|entry| entry.0.cmp(id)).ok()?;
        Some(&mut Arc::make_mut(&mut self.pages[index])[item].1)
    }
    fn contains_key(&self, id: &K) -> bool {
        self.get(id).is_some()
    }
    fn insert(&mut self, id: K, value: Placement) -> Option<Placement> {
        let index = self.page_index(&id);
        if index == self.pages.len() {
            if let Some(last) = self.pages.last_mut().filter(|page| page.len() < PAGE_SIZE) {
                Arc::make_mut(last).push((id, value));
            } else {
                self.pages.push(Arc::new(vec![(id, value)]));
            }
            return None;
        }
        let page = Arc::make_mut(&mut self.pages[index]);
        match page.binary_search_by(|entry| entry.0.cmp(&id)) {
            Ok(i) => Some(std::mem::replace(&mut page[i].1, value)),
            Err(i) => {
                page.insert(i, (id, value));
                if page.len() > PAGE_SIZE * 2 {
                    let tail = page.split_off(PAGE_SIZE);
                    self.pages.insert(index + 1, Arc::new(tail));
                }
                None
            }
        }
    }
    fn remove(&mut self, id: &K) -> Option<Placement> {
        let index = self.page_index(id);
        let page = self.pages.get(index)?;
        let item = page.binary_search_by(|entry| entry.0.cmp(id)).ok()?;
        let page = Arc::make_mut(&mut self.pages[index]);
        let value = page.remove(item).1;
        if page.is_empty() {
            self.pages.remove(index);
        } else if page.len() < PAGE_SIZE / 2 && self.pages.len() > 1 {
            // Avoid accumulating many sparsely populated pages after a long
            // sequence of removals. Preserve old versions through COW.
            let left = if index == 0 { 0 } else { index - 1 };
            let right = left + 1;
            let mut joined: Vec<_> = self.pages[left]
                .iter()
                .chain(self.pages[right].iter())
                .cloned()
                .collect();
            if joined.len() > PAGE_SIZE * 2 {
                let tail = joined.split_off(joined.len() / 2);
                self.pages[left] = Arc::new(joined);
                self.pages[right] = Arc::new(tail);
            } else {
                self.pages[left] = Arc::new(joined);
                self.pages.remove(right);
            }
        }
        Some(value)
    }
    fn iter(&self) -> impl Iterator<Item = (&K, &Placement)> {
        self.pages
            .iter()
            .flat_map(|page| page.iter().map(|(key, value)| (key, value)))
    }
    fn values(&self) -> impl Iterator<Item = &Placement> {
        self.iter().map(|(_, value)| value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub rect: Rect,
    pub weight: u64,
}

#[derive(Clone, Debug)]
pub struct Partition<K> {
    bounds: Rect,
    entries: Entries<K>,
    vacant: Vec<Rect>,
    total: u128,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Update<K> {
    /// The complete region that must be redrawn, including underlying platforms.
    pub dirty: Rect,
    /// Every entry whose geometry changed, paired with its new rectangle.
    pub moved: Vec<(K, Rect)>,
    /// True when all live entries were laid out again inside the fixed bounds.
    pub repacked: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidBounds,
    Duplicate,
    Missing,
    Cancelled,
}

fn area(r: Rect) -> f64 {
    r.w as f64 * r.h as f64
}
fn valid(r: Rect) -> bool {
    r.x.is_finite()
        && r.y.is_finite()
        && r.w.is_finite()
        && r.h.is_finite()
        && r.w > 0.0
        && r.h > 0.0
}
fn weight(n: u64) -> u128 {
    u128::from(n.max(1))
}

// Bitwise hash keys matching float equality, including +0 == -0.
fn float_key(value: f32) -> u32 {
    if value == 0.0 { 0 } else { value.to_bits() }
}

fn adjacent_union(a: Rect, b: Rect) -> Option<Rect> {
    if a.y == b.y && a.h == b.h && (a.x + a.w == b.x || b.x + b.w == a.x) {
        Some(Rect {
            x: a.x.min(b.x),
            y: a.y,
            w: a.w + b.w,
            h: a.h,
        })
    } else if a.x == b.x && a.w == b.w && (a.y + a.h == b.y || b.y + b.h == a.y) {
        Some(Rect {
            x: a.x,
            y: a.y.min(b.y),
            w: a.w,
            h: a.h + b.h,
        })
    } else {
        None
    }
}

impl<K: Ord + Clone> Partition<K> {
    /// `placements` must be a non-overlapping layout within `bounds` produced
    /// by the initial directory layout. Its ID is a stable directory-entry ID,
    /// not an index into a sorted snapshot.
    pub fn from_layout(
        bounds: Rect,
        placements: impl IntoIterator<Item = (K, Placement)>,
    ) -> Result<Self, Error> {
        if !valid(bounds) {
            return Err(Error::InvalidBounds);
        }
        // Collect first so Vec::IntoIter can reuse its allocation, rather than
        // retaining an equally large source buffer while a second one grows.
        let entries: Vec<_> = placements.into_iter().collect();
        let mut total = 0u128;
        for (_, placement) in &entries {
            let r = placement.rect;
            if !valid(r)
                || r.x < bounds.x
                || r.y < bounds.y
                || r.x + r.w > bounds.x + bounds.w + 0.01
                || r.y + r.h > bounds.y + bounds.h + 0.01
            {
                return Err(Error::InvalidBounds);
            }
            total += weight(placement.weight);
        }
        Ok(Self {
            bounds,
            entries: Entries::from_unsorted(entries)?,
            vacant: Vec::new(),
            total,
        })
    }

    pub fn bounds(&self) -> Rect {
        self.bounds
    }
    pub fn get(&self, id: &K) -> Option<Placement> {
        self.entries.get(id).copied()
    }
    pub fn entries(&self) -> impl Iterator<Item = (&K, &Placement)> {
        self.entries.iter()
    }
    pub fn vacancies(&self) -> &[Rect] {
        &self.vacant
    }

    /// Restore a previously rendered slot as empty during scene recovery. The
    /// caller supplies rectangles from the original non-overlapping layout.
    pub fn restore_vacancy(&mut self, rect: Rect) -> Result<(), Error> {
        if !valid(rect)
            || rect.x < self.bounds.x
            || rect.y < self.bounds.y
            || rect.x + rect.w > self.bounds.x + self.bounds.w + 0.01
            || rect.y + rect.h > self.bounds.y + self.bounds.h + 0.01
        {
            return Err(Error::InvalidBounds);
        }
        self.vacant.push(rect);
        Ok(())
    }

    /// Area error in physical pixels; callers use this to decide when to grow
    /// the re-layout region to an ancestor. Absolute error includes vacancies.
    pub fn max_area_error(&self, pixels_per_point: f32) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        let pixel_area = (pixels_per_point as f64).max(0.0).powi(2);
        self.entries
            .values()
            .map(|p| {
                let ideal = area(self.bounds) * weight(p.weight) as f64 / self.total as f64;
                (area(p.rect) - ideal).abs() * pixel_area
            })
            .fold(0.0, f64::max)
    }

    pub fn remove(&mut self, id: &K) -> Result<Update<K>, Error> {
        let old = self.entries.remove(id).ok_or(Error::Missing)?;
        self.total -= weight(old.weight);
        self.vacant.push(old.rect);
        Ok(Update {
            dirty: old.rect,
            moved: Vec::new(),
            repacked: false,
        })
    }

    /// Change an allocation's identity and weight without moving its frame or
    /// disturbing unrelated vacancies. Used when a directory tile expands
    /// into a subtree (or folds back).
    pub fn replace_key(&mut self, old: &K, new: K, value: u64) -> Result<(), Error> {
        let placement = self.entries.get(old).copied().ok_or(Error::Missing)?;
        if old != &new && self.entries.contains_key(&new) {
            return Err(Error::Duplicate);
        }
        self.entries.remove(old).ok_or(Error::Missing)?;
        self.total = self.total - weight(placement.weight) + weight(value);
        self.entries.insert(
            new,
            Placement {
                rect: placement.rect,
                weight: value,
            },
        );
        Ok(())
    }

    /// Allocate inside a prior vacancy, or split the largest existing frame.
    /// All other allocations and vacancies retain their exact geometry.
    pub fn allocate_local(
        &mut self,
        id: K,
        value: u64,
    ) -> Result<(Rect, Option<(K, Rect, Rect)>), Error> {
        if self.entries.contains_key(&id) {
            return Err(Error::Duplicate);
        }
        if let Some(rect) = self.vacant.pop() {
            self.total += weight(value);
            self.entries.insert(
                id,
                Placement {
                    rect,
                    weight: value,
                },
            );
            return Ok((rect, None));
        }
        // A retained empty partition can originate in an exported layout that
        // has no explicit vacancy. Its content bounds contain no allocation,
        // so the first item can occupy them without moving another subtree.
        if self.entries.pages.is_empty() {
            let rect = self.bounds;
            self.total += weight(value);
            self.entries.insert(
                id,
                Placement {
                    rect,
                    weight: value,
                },
            );
            return Ok((rect, None));
        }
        let (donor, old) = self
            .entries
            .iter()
            .max_by(|(_, a), (_, b)| area(a.rect).total_cmp(&area(b.rect)))
            .map(|(key, value)| (key.clone(), *value))
            .ok_or(Error::Missing)?;
        let fraction = (weight(value) as f64 / (weight(value) + weight(old.weight)) as f64)
            .clamp(0.05, 0.95) as f32;
        let (kept, new) = if old.rect.w >= old.rect.h {
            let width = old.rect.w * (1.0 - fraction);
            (
                Rect {
                    w: width,
                    ..old.rect
                },
                Rect {
                    x: old.rect.x + width,
                    w: old.rect.w - width,
                    ..old.rect
                },
            )
        } else {
            let height = old.rect.h * (1.0 - fraction);
            (
                Rect {
                    h: height,
                    ..old.rect
                },
                Rect {
                    y: old.rect.y + height,
                    h: old.rect.h - height,
                    ..old.rect
                },
            )
        };
        self.entries.get_mut(&donor).ok_or(Error::Missing)?.rect = kept;
        self.total += weight(value);
        self.entries.insert(
            id,
            Placement {
                rect: new,
                weight: value,
            },
        );
        Ok((new, Some((donor, old.rect, kept))))
    }

    /// Translate keys while preserving all live frames and vacancies. Used
    /// only for a newly laid-out subtree when local directory IDs are interned
    /// into its owning scene.
    pub fn map_keys<J: Ord + Clone>(
        &self,
        mut map: impl FnMut(&K) -> Option<J>,
    ) -> Option<Partition<J>> {
        let items: Option<Vec<_>> = self
            .entries
            .iter()
            .map(|(key, placement)| Some((map(key)?, *placement)))
            .collect();
        let mut result = Partition::from_layout(self.bounds, items?).ok()?;
        result.vacant = self.vacant.clone();
        Some(result)
    }

    /// Grow a movable neighbor into an exactly adjacent vacancy without moving
    /// any directory allocation. No geometry is committed on cancellation.
    pub fn compact_adjacent(
        &mut self,
        movable: impl Fn(&K) -> bool,
        token: &AtomicU64,
        generation: u64,
    ) -> Result<Vec<Update<K>>, Error> {
        if token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        if self.vacant.is_empty() {
            return Ok(Vec::new());
        }
        let mut staged = self.clone();
        let mut changes = Vec::new();
        // Index only the first 64 holes, then scan live entries once. An edge
        // collision keeps the earliest hole; any others remain reusable later.
        // This keeps work O(entries + holes), rather than 64 full scans.
        let vacancies: Vec<_> = staged.vacant.iter().copied().take(64).collect();
        let mut edges = HashMap::<[u32; 4], usize>::with_capacity(vacancies.len() * 4);
        for (i, b) in vacancies.iter().enumerate() {
            for key in [
                [0, float_key(b.x), float_key(b.y), float_key(b.h)],
                [1, float_key(b.x + b.w), float_key(b.y), float_key(b.h)],
                [2, float_key(b.y), float_key(b.x), float_key(b.w)],
                [3, float_key(b.y + b.h), float_key(b.x), float_key(b.w)],
            ] {
                edges.entry(key).or_insert(i);
            }
        }
        let mut candidates: Vec<Option<K>> = vec![None; vacancies.len()];
        for (i, (id, placement)) in staged.entries.iter().enumerate() {
            if i % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
                return Err(Error::Cancelled);
            }
            if !movable(id) {
                continue;
            }
            let a = placement.rect;
            for key in [
                [0, float_key(a.x + a.w), float_key(a.y), float_key(a.h)],
                [1, float_key(a.x), float_key(a.y), float_key(a.h)],
                [2, float_key(a.y + a.h), float_key(a.x), float_key(a.w)],
                [3, float_key(a.y), float_key(a.x), float_key(a.w)],
            ] {
                if let Some(&vacancy) = edges.get(&key)
                    && candidates[vacancy].is_none()
                    && adjacent_union(a, vacancies[vacancy]).is_some()
                {
                    candidates[vacancy] = Some(id.clone());
                }
            }
        }
        let mut removed = [false; 64];
        for (index, id) in candidates.into_iter().enumerate() {
            if token.load(Ordering::Relaxed) != generation {
                return Err(Error::Cancelled);
            }
            let Some(id) = id else {
                continue;
            };
            // A previously compacted hole may have changed this candidate.
            // Recheck rather than moving it twice on stale geometry.
            let Some(rect) = staged
                .entries
                .get(&id)
                .and_then(|p| adjacent_union(p.rect, vacancies[index]))
            else {
                continue;
            };
            staged.entries.get_mut(&id).expect("selected entry").rect = rect;
            removed[index] = true;
            changes.push(Update {
                dirty: rect,
                moved: vec![(id, rect)],
                repacked: false,
            });
        }
        let mut index = 0usize;
        staged.vacant.retain(|_| {
            let keep = index >= removed.len() || !removed[index];
            index += 1;
            keep
        });
        if token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        *self = staged;
        Ok(changes)
    }

    /// Reuse a vacant slot or split one donor. Other allocations remain fixed;
    /// callers may accept a temporary area error to preserve the old scene.
    pub fn insert(
        &mut self,
        id: K,
        value: u64,
        pixels_per_point: f32,
        tolerance_pixels: f64,
        token: &AtomicU64,
        generation: u64,
    ) -> Result<Update<K>, Error> {
        if self.entries.contains_key(&id) {
            return Err(Error::Duplicate);
        }
        if token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        let new_total = self.total + weight(value);
        let ideal = area(self.bounds) * weight(value) as f64 / new_total as f64;
        let scale = (pixels_per_point as f64).max(0.0).powi(2);
        let best = self
            .vacant
            .iter()
            .enumerate()
            .map(|(i, &rect)| (i, (area(rect) - ideal).abs() * scale))
            .filter(|(_, error)| *error <= tolerance_pixels.max(0.0))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((index, _)) = best {
            let rect = self.vacant.swap_remove(index);
            self.entries.insert(
                id.clone(),
                Placement {
                    rect,
                    weight: value,
                },
            );
            self.total = new_total;
            return Ok(Update {
                dirty: rect,
                moved: vec![(id, rect)],
                repacked: false,
            });
        }
        let (rect, donor) = self.allocate_local(id.clone(), value)?;
        let mut moved = Vec::with_capacity(2);
        let dirty = if let Some((donor, old, kept)) = donor {
            moved.push((donor, kept));
            old
        } else {
            rect
        };
        moved.push((id, rect));
        Ok(Update {
            dirty,
            moved,
            repacked: false,
        })
    }

    /// Update a weight while preserving the old geometry, so callers can batch
    /// drift without moving unrelated allocations.
    pub fn set_weight(&mut self, id: &K, value: u64) -> Result<(), Error> {
        let entry = self.entries.get_mut(id).ok_or(Error::Missing)?;
        self.total = self.total - weight(entry.weight) + weight(value);
        entry.weight = value;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paged_entries_merge_after_sparse_removals_without_mutating_old_version() {
        let placement = Placement {
            rect: rect(0.0, 1.0),
            weight: 1,
        };
        let mut entries =
            Entries::from_unsorted((0..4096).map(|id| (id, placement)).collect()).unwrap();
        let old = entries.clone();
        for id in 0..4096 {
            if id % 256 != 0 {
                entries.remove(&id);
            }
        }
        assert_eq!(entries.iter().count(), 16);
        assert_eq!(entries.pages.len(), 1);
        assert_eq!(old.iter().count(), 4096);
        assert!(entries.iter().map(|(id, _)| *id).eq((0..4096).step_by(256)));
    }
    #[test]
    fn paged_entries_merge_from_tail_and_preserve_shared_snapshot() {
        let placement = Placement {
            rect: rect(0.0, 1.0),
            weight: 1,
        };
        let mut entries =
            Entries::from_unsorted((0..4096).map(|id| (id, placement)).collect()).unwrap();
        let old = entries.clone();
        for id in (0..4096).rev() {
            if id % 256 != 0 {
                entries.remove(&id);
            }
        }
        assert_eq!(entries.iter().count(), 16);
        assert_eq!(entries.pages.len(), 1);
        assert_eq!(old.iter().count(), 4096);
    }
    #[test]
    fn compaction_rechecks_shared_candidate_and_cancellation_is_atomic() {
        use std::cell::Cell;
        let mut partition = Partition::from_layout(
            rect(0.0, 100.0),
            (0..4).map(|id| {
                (
                    id,
                    Placement {
                        rect: rect((id * 25) as f32, 25.0),
                        weight: 1,
                    },
                )
            }),
        )
        .unwrap();
        partition.remove(&0).unwrap();
        partition.remove(&2).unwrap();
        let snapshot = partition.clone();
        let token = AtomicU64::new(1);
        let calls = Cell::new(0);
        assert_eq!(
            partition.compact_adjacent(
                |_| {
                    calls.set(calls.get() + 1);
                    token.store(2, Ordering::Relaxed);
                    true
                },
                &token,
                1
            ),
            Err(Error::Cancelled)
        );
        assert!(calls.get() > 0);
        assert!(partition.entries().eq(snapshot.entries()));
        assert_eq!(partition.vacancies(), snapshot.vacancies());
        token.store(1, Ordering::Relaxed);
        let updates = partition.compact_adjacent(|_| true, &token, 1).unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(partition.get(&1).unwrap().rect, rect(0.0, 75.0));
        assert!(partition.vacancies().is_empty());
    }
    #[test]
    fn compaction_checks_large_partition_once_for_many_holes() {
        use std::cell::Cell;
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 10_000.0,
            h: 100.0,
        };
        let placements = (0..10_000).map(|id| {
            (
                id,
                Placement {
                    rect: Rect {
                        x: id as f32,
                        y: 0.0,
                        w: 1.0,
                        h: 1.0,
                    },
                    weight: 1,
                },
            )
        });
        let mut partition = Partition::from_layout(bounds, placements).unwrap();
        for id in 0..64 {
            partition.remove(&id).unwrap();
        }
        let calls = Cell::new(0);
        partition
            .compact_adjacent(
                |_| {
                    calls.set(calls.get() + 1);
                    true
                },
                &AtomicU64::new(1),
                1,
            )
            .unwrap();
        assert_eq!(calls.get(), 10_000 - 64);
    }
    #[test]
    fn paged_entries_keep_order_and_snapshot_isolation() {
        let placement = Placement {
            rect: rect(0.0, 1.0),
            weight: 1,
        };
        let mut entries =
            Entries::from_unsorted((0..2048).rev().map(|id| (id * 2, placement)).collect())
                .unwrap();
        let old = entries.clone();
        assert_eq!(entries.remove(&512), Some(placement));
        assert!(entries.insert(513, placement).is_none());
        entries.get_mut(&1024).unwrap().weight = 9;
        assert_eq!(old.get(&512), Some(&placement));
        assert_eq!(old.get(&513), None);
        assert_eq!(old.get(&1024), Some(&placement));
        assert_eq!(entries.get(&1024).unwrap().weight, 9);
        let keys: Vec<_> = entries.iter().map(|(key, _)| *key).collect();
        assert!(keys.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(keys.len(), 2048);
    }
    #[test]
    fn paged_entries_match_ordered_map_across_splits_and_removals() {
        let placement = Placement {
            rect: rect(0.0, 1.0),
            weight: 1,
        };
        let mut entries = Entries::from_unsorted(Vec::<(u32, Placement)>::new()).unwrap();
        let mut reference = std::collections::BTreeMap::new();
        let mut state = 1u64;
        for _ in 0..25_000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = ((state >> 17) % 4096) as u32;
            if state & 3 == 0 {
                assert_eq!(entries.remove(&key), reference.remove(&key));
            } else {
                let value = Placement {
                    weight: state,
                    ..placement
                };
                assert_eq!(entries.insert(key, value), reference.insert(key, value));
            }
        }
        assert!(entries.iter().eq(reference.iter()));
    }
    fn rect(x: f32, w: f32) -> Rect {
        Rect {
            x,
            y: 0.0,
            w,
            h: 100.0,
        }
    }
    fn partition() -> Partition<u64> {
        Partition::from_layout(
            rect(0.0, 100.0),
            [
                (
                    1,
                    Placement {
                        rect: rect(0.0, 50.0),
                        weight: 50,
                    },
                ),
                (
                    2,
                    Placement {
                        rect: rect(50.0, 50.0),
                        weight: 50,
                    },
                ),
            ],
        )
        .unwrap()
    }
    #[test]
    fn deletion_and_equivalent_insertion_preserve_neighbor_geometry() {
        let token = AtomicU64::new(1);
        let mut p = partition();
        let neighbor = p.get(&2).unwrap();
        let deleted = p.remove(&1).unwrap();
        assert_eq!(deleted.dirty, rect(0.0, 50.0));
        let inserted = p.insert(3, 50, 1.0, 0.0, &token, 1).unwrap();
        assert!(!inserted.repacked);
        assert_eq!(p.get(&2).unwrap(), neighbor);
        assert_eq!(p.get(&3).unwrap().rect, deleted.dirty);
    }
    #[test]
    fn adjacent_compaction_grows_only_an_allowed_neighbor_and_is_atomic_on_cancel() {
        let token = AtomicU64::new(1);
        let mut p = Partition::from_layout(
            rect(0.0, 100.0),
            [
                (
                    1,
                    Placement {
                        rect: rect(0.0, 25.0),
                        weight: 25,
                    },
                ),
                (
                    2,
                    Placement {
                        rect: rect(25.0, 25.0),
                        weight: 25,
                    },
                ),
                (
                    3,
                    Placement {
                        rect: rect(50.0, 50.0),
                        weight: 50,
                    },
                ),
            ],
        )
        .unwrap();
        p.remove(&1).unwrap();
        let fixed = p.get(&3);
        assert_eq!(
            p.compact_adjacent(|id| *id != 3, &token, 2).unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(p.get(&2).unwrap().rect, rect(25.0, 25.0));
        let moves = p.compact_adjacent(|id| *id != 3, &token, 1).unwrap();
        assert_eq!(moves.len(), 1);
        assert_eq!(p.get(&2).unwrap().rect, rect(0.0, 50.0));
        assert_eq!(p.get(&3), fixed);
        assert!(p.vacancies().is_empty());
    }
    #[test]
    fn unsuitable_hole_is_reused_without_rebuilding_partition() {
        let token = AtomicU64::new(1);
        let mut p = partition();
        p.remove(&1).unwrap();
        let result = p.insert(3, 150, 1.0, 1.0, &token, 1).unwrap();
        assert!(!result.repacked);
        assert_eq!(result.dirty, rect(0.0, 50.0));
        assert_eq!(p.get(&3).unwrap().rect, rect(0.0, 50.0));
        assert!(p.vacancies().is_empty());
    }
    #[test]
    fn stale_work_does_not_mutate_and_weight_error_is_reported() {
        let token = AtomicU64::new(2);
        let mut p = partition();
        p.set_weight(&1, 150).unwrap();
        assert!((p.max_area_error(1.0) - 2500.0).abs() < 0.01);
        let before = p.get(&1);
        assert_eq!(
            p.insert(3, 50, 1.0, 0.0, &token, 1).unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(p.get(&1), before);
        let inserted = p.insert(3, 50, 1.0, 0.0, &token, 2).unwrap();
        assert!(!inserted.repacked);
    }
    #[test]
    fn zero_bytes_keep_minimum_layout_weight_and_duplicate_is_rejected() {
        let token = AtomicU64::new(1);
        let mut p = partition();
        p.remove(&1).unwrap();
        assert_eq!(
            p.insert(2, 0, 1.0, 1.0, &token, 1).unwrap_err(),
            Error::Duplicate
        );
        p.insert(3, 0, 1.0, 0.0, &token, 1).unwrap();
        assert!(p.get(&3).unwrap().rect.w > 0.0);
        assert_eq!(p.get(&3).unwrap().weight, 0);
    }

    #[test]
    fn first_allocation_reuses_empty_partition_bounds_without_a_donor() {
        let mut partition =
            Partition::from_layout(rect(0.0, 100.0), std::iter::empty::<(u32, Placement)>())
                .unwrap();
        let old = partition.clone();
        let (allocated, donor) = partition.allocate_local(7, 42).unwrap();
        assert_eq!(allocated, rect(0.0, 100.0));
        assert_eq!(donor, None);
        assert_eq!(partition.get(&7).unwrap().rect, allocated);
        assert!(old.get(&7).is_none());
    }
}
