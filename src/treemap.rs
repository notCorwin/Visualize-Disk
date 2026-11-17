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
pub fn layout(items: &[(usize, u64)], bounds: Rect) -> Vec<(usize, Rect)> {
    if items.is_empty()
        || !bounds.x.is_finite()
        || !bounds.y.is_finite()
        || !bounds.w.is_finite()
        || !bounds.h.is_finite()
        || bounds.w <= 0.0
        || bounds.h <= 0.0
    {
        return Vec::new();
    }
    let mut ordered = items.to_vec();
    ordered.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut result = Vec::with_capacity(items.len());
    // Iteration avoids stack overflow on pathological file-size distributions.
    let mut pending = vec![(ordered.as_slice(), bounds)];
    while let Some((slice, rect)) = pending.pop() {
        if slice.len() == 1 {
            result.push((slice[0].0, rect));
            continue;
        }
        let total = slice
            .iter()
            .map(|(_, size)| (*size).max(1) as f64)
            .sum::<f64>();
        let target = total / 2.0;
        let mut left = 0.0;
        let mut split = 1;
        for (index, (_, size)) in slice.iter().enumerate().take(slice.len() - 1) {
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
        // At extreme size ratios f32 rounds the fraction to 1, which would
        // erase every file in the remaining branch. Keep both branches
        // representable; this only distorts areas below floating-point precision.
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
        pending.push((&slice[split..], b));
        pending.push((&slice[..split], a));
    }
    result
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
}
