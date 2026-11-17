use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
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
}
