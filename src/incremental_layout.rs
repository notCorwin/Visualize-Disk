//! Stable geometry within one directory's existing content rectangle.
//!
//! A partition never moves its outer bounds during an ordinary update. Deleted
//! slots stay vacant, allowing a later sibling to reuse their pixels. When no
//! suitably sized slot exists, only this partition is rebuilt. The caller owns
//! directory ancestry, platform geometry and propagation of area drift.
use crate::treemap::{self, Rect};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub rect: Rect,
    pub weight: u64,
}

#[derive(Clone, Debug)]
pub struct Partition<K> {
    bounds: Rect,
    entries: BTreeMap<K, Placement>,
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
        let mut entries = BTreeMap::new();
        let mut total = 0u128;
        for (id, placement) in placements {
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
            if entries.insert(id, placement).is_some() {
                return Err(Error::Duplicate);
            }
        }
        Ok(Self {
            bounds,
            entries,
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

    /// Grow a movable neighbor into an exactly adjacent vacancy without moving
    /// any directory allocation. No geometry is committed on cancellation.
    pub fn compact_adjacent(
        &mut self,
        movable: impl Fn(&K) -> bool,
        token: &AtomicU64,
        generation: u64,
    ) -> Result<Vec<Update<K>>, Error> {
        let mut staged = self.clone();
        let mut changes = Vec::new();
        // Bounded work on very large deletion batches; remaining holes stay
        // valid and can be reused by later insertions.
        for vacancy in staged.vacant.clone().into_iter().take(64) {
            if token.load(Ordering::Relaxed) != generation {
                return Err(Error::Cancelled);
            }
            let mut found = None;
            for (i, (id, placement)) in staged.entries.iter().enumerate() {
                if i % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
                    return Err(Error::Cancelled);
                }
                if !movable(id) {
                    continue;
                }
                let a = placement.rect;
                let b = vacancy;
                let union = if a.y == b.y && a.h == b.h && (a.x + a.w == b.x || b.x + b.w == a.x) {
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
                };
                if let Some(union) = union {
                    found = Some((id.clone(), union));
                    break;
                }
            }
            if let Some((id, rect)) = found {
                let Some(index) = staged.vacant.iter().position(|r| *r == vacancy) else {
                    continue;
                };
                staged.vacant.swap_remove(index);
                staged.entries.get_mut(&id).expect("selected entry").rect = rect;
                changes.push(Update {
                    dirty: rect,
                    moved: vec![(id, rect)],
                    repacked: false,
                });
            }
        }
        if token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        *self = staged;
        Ok(changes)
    }

    /// Reuse a vacant slot only when its area is within the caller's pixel-area
    /// tolerance of the new item's ideal share. Other entries are left untouched.
    /// Otherwise repack this partition atomically; stale generations cannot
    /// commit partial geometry.
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
        let mut weights: Vec<_> = self
            .entries
            .iter()
            .map(|(k, p)| (k.clone(), p.weight))
            .collect();
        weights.push((id, value));
        self.rebuild(self.bounds, weights, token, generation)
    }

    /// Update a weight while preserving the old geometry, so callers can batch
    /// drift and choose the smallest appropriate ancestor for a later repack.
    pub fn set_weight(&mut self, id: &K, value: u64) -> Result<(), Error> {
        let entry = self.entries.get_mut(id).ok_or(Error::Missing)?;
        self.total = self.total - weight(entry.weight) + weight(value);
        entry.weight = value;
        Ok(())
    }

    /// Repack this partition into `bounds` after a significant change or resize.
    pub fn repack(
        &mut self,
        bounds: Rect,
        token: &AtomicU64,
        generation: u64,
    ) -> Result<Update<K>, Error> {
        let weights = self
            .entries
            .iter()
            .map(|(k, p)| (k.clone(), p.weight))
            .collect();
        self.rebuild(bounds, weights, token, generation)
    }

    fn rebuild(
        &mut self,
        bounds: Rect,
        weights: Vec<(K, u64)>,
        token: &AtomicU64,
        generation: u64,
    ) -> Result<Update<K>, Error> {
        if !valid(bounds) {
            return Err(Error::InvalidBounds);
        }
        let indexed: Vec<_> = weights
            .iter()
            .enumerate()
            .map(|(i, (_, w))| (i, *w))
            .collect();
        let geometry = treemap::layout_with_cancel(&indexed, bounds, token, generation)
            .ok_or(Error::Cancelled)?;
        if token.load(Ordering::Relaxed) != generation {
            return Err(Error::Cancelled);
        }
        let mut next = BTreeMap::new();
        let mut moved = Vec::with_capacity(geometry.len());
        for (i, rect) in geometry {
            let (id, value) = &weights[i];
            next.insert(
                id.clone(),
                Placement {
                    rect,
                    weight: *value,
                },
            );
            moved.push((id.clone(), rect));
        }
        let old = self.bounds;
        let x = old.x.min(bounds.x);
        let y = old.y.min(bounds.y);
        let dirty = Rect {
            x,
            y,
            w: (old.x + old.w).max(bounds.x + bounds.w) - x,
            h: (old.y + old.h).max(bounds.y + bounds.h) - y,
        };
        self.total = weights.iter().map(|(_, w)| weight(*w)).sum();
        self.bounds = bounds;
        self.entries = next;
        self.vacant.clear();
        Ok(Update {
            dirty,
            moved,
            repacked: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn unsuitable_hole_rebuilds_only_fixed_partition() {
        let token = AtomicU64::new(1);
        let mut p = partition();
        p.remove(&1).unwrap();
        let result = p.insert(3, 150, 1.0, 1.0, &token, 1).unwrap();
        assert!(result.repacked);
        assert_eq!(result.dirty, p.bounds());
        assert!((area(p.get(&3).unwrap().rect) - 7500.0).abs() < 0.01);
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
            p.repack(p.bounds(), &token, 1).unwrap_err(),
            Error::Cancelled
        );
        assert_eq!(p.get(&1), before);
        p.repack(p.bounds(), &token, 2).unwrap();
        assert!(p.max_area_error(1.0) < 0.01);
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
}
