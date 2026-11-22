use crate::scan::FileEntry;
use eframe::egui::{self, Color32, FontId, Rect, Sense, Vec2};
use std::{collections::HashMap, sync::Arc};

// Keep height bookkeeping proportional to recently viewed rows, never the disk.
const MAX_MEASURED_ROWS: usize = 512;
const PADDING: f32 = 6.0;
const MARKER: f32 = 8.0;
const MARKER_GAP: f32 = 8.0;
const TEXT_GAP: f32 = 3.0;
const ROW_GAP: f32 = 4.0;

#[derive(Default)]
struct Heights {
    len: usize,
    base: f64,
    measured: HashMap<usize, f64>,
    deltas: HashMap<usize, f64>,
}
impl Heights {
    fn reset(&mut self, len: usize, base: f64) {
        self.len = len;
        self.base = base;
        self.measured.clear();
        self.deltas.clear();
    }
    fn prefix(&self, end: usize) -> f64 {
        let mut index = end.min(self.len);
        let mut result = index as f64 * self.base;
        while index > 0 {
            result += self.deltas.get(&index).copied().unwrap_or_default();
            index &= index - 1;
        }
        result
    }
    fn measure(&mut self, row: usize, height: f64) {
        let previous = self.measured.insert(row, height).unwrap_or(self.base);
        let delta = height - previous;
        let mut index = row + 1;
        while index <= self.len {
            *self.deltas.entry(index).or_default() += delta;
            index += index & index.wrapping_neg();
        }
    }
    fn row_at(&self, offset: f64) -> usize {
        if self.len == 0 {
            return 0;
        }
        let target = offset.max(0.0);
        let mut index = 0usize;
        let mut height = 0.0;
        let mut step = 1usize << (usize::BITS - 1 - self.len.leading_zeros());
        while step != 0 {
            let next = index + step;
            if next <= self.len {
                let node = (next & next.wrapping_neg()) as f64 * self.base
                    + self.deltas.get(&next).copied().unwrap_or_default();
                if height + node <= target {
                    index = next;
                    height += node;
                }
            }
            step >>= 1;
        }
        index.min(self.len - 1)
    }
}

#[derive(PartialEq)]
struct CacheKey {
    generation: u64,
    files: usize,
    indices: usize,
    len: usize,
    width: u32,
    pixels_per_point: u32,
    body: FontId,
    small: FontId,
    button: FontId,
    spacing: [u32; 6],
}

#[derive(Default)]
pub struct WrappedFileList {
    key: Option<CacheKey>,
    heights: Heights,
    offset: f32,
    #[cfg(test)]
    rows_last_frame: usize,
    #[cfg(test)]
    row_rects: Vec<(usize, Rect)>,
}

fn filename_layout(ui: &egui::Ui, text: String, width: f32, font: FontId) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(text, font, crate::TEXT, width.max(1.0));
    job.wrap.break_anywhere = true;
    ui.fonts_mut(|fonts| fonts.layout_job(job))
}

impl WrappedFileList {
    fn key(ui: &egui::Ui, generation: u64, len: usize, files: usize, indices: usize) -> CacheKey {
        CacheKey {
            generation,
            files,
            indices,
            len,
            width: ui.available_width().max(1.0).to_bits(),
            pixels_per_point: ui.ctx().pixels_per_point().to_bits(),
            body: egui::TextStyle::Body.resolve(ui.style()),
            small: egui::TextStyle::Small.resolve(ui.style()),
            button: egui::TextStyle::Button.resolve(ui.style()),
            spacing: [
                ui.spacing().item_spacing.x.to_bits(),
                ui.spacing().item_spacing.y.to_bits(),
                ui.spacing().button_padding.x.to_bits(),
                ui.spacing().button_padding.y.to_bits(),
                ui.spacing().interact_size.x.to_bits(),
                ui.spacing().interact_size.y.to_bits(),
            ],
        }
    }

    /// Layout only visible rows. The callback uses normal wrapped egui widgets;
    /// its measured height is cached. `generation` identifies the ordered data.
    pub fn show_rows<T>(
        &mut self,
        ui: &mut egui::Ui,
        generation: u64,
        count: usize,
        mut row: impl FnMut(&mut egui::Ui, usize) -> Option<T>,
    ) -> Option<T> {
        let key = Self::key(ui, generation, count, 0, 0);
        self.show_inner(ui, key, &mut row)
    }

    /// `generation` changes when the ordered filter result changes. Dataset,
    /// width, text fonts and display-scale changes also invalidate measurements.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        generation: u64,
        files: &[FileEntry],
        indices: &[usize],
        selected: Option<usize>,
        color: impl Fn(usize) -> Color32,
    ) -> Option<usize> {
        let key = Self::key(
            ui,
            generation,
            indices.len(),
            files.as_ptr() as usize,
            indices.as_ptr() as usize,
        );
        let body = key.body.clone();
        let small = key.small.clone();
        self.show_inner(ui, key, &mut |ui, row| {
            let id = *indices.get(row)?;
            let file = files.get(id)?;
            let width = ui.available_width().max(1.0);
            let text_width = (width - PADDING * 2.0 - MARKER - MARKER_GAP).max(1.0);
            let name = file
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let name = filename_layout(ui, name, text_width, body.clone());
            let size =
                ui.painter()
                    .layout_no_wrap(crate::bytes(file.size), small.clone(), crate::MUTED);
            let height = PADDING * 2.0 + name.size().y + TEXT_GAP + size.size().y;
            let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::WidgetType::SelectableLabel,
                    ui.is_enabled(),
                    selected == Some(id),
                    &name.job.text,
                )
            });
            if selected == Some(id) || response.hovered() {
                let fill = if selected == Some(id) {
                    ui.visuals().selection.bg_fill
                } else {
                    ui.visuals().widgets.hovered.bg_fill
                };
                ui.painter()
                    .rect_filled(rect, ui.visuals().widgets.hovered.corner_radius, fill);
            }
            let marker = Rect::from_center_size(
                egui::pos2(
                    rect.left() + PADDING + MARKER / 2.0,
                    rect.top() + PADDING + body.size / 2.0,
                ),
                Vec2::splat(MARKER),
            );
            ui.painter().rect_filled(marker, 2.0, color(id));
            let text_x = rect.left() + PADDING + MARKER + MARKER_GAP;
            ui.painter()
                .galley(egui::pos2(text_x, rect.top() + PADDING), name, crate::TEXT);
            ui.painter().galley(
                egui::pos2(text_x, rect.bottom() - PADDING - size.size().y),
                size,
                crate::MUTED,
            );
            crate::show_file_tooltip(&response, file);
            response.clicked().then_some(id)
        })
    }

    fn show_inner<T>(
        &mut self,
        ui: &mut egui::Ui,
        key: CacheKey,
        row_contents: &mut impl FnMut(&mut egui::Ui, usize) -> Option<T>,
    ) -> Option<T> {
        if ui.available_height() <= 0.0 {
            return None;
        }
        let width = f32::from_bits(key.width);
        let base = ui.fonts_mut(|fonts| fonts.row_height(&key.body) + fonts.row_height(&key.small))
            + PADDING * 2.0
            + TEXT_GAP
            + ROW_GAP;
        let data_changed = self.key.as_ref().is_none_or(|old| {
            old.generation != key.generation
                || old.files != key.files
                || old.indices != key.indices
                || old.len != key.len
        });
        let same_key = self.key.as_ref() == Some(&key);
        let generation = key.generation;
        let reset = !same_key || self.heights.measured.len() >= MAX_MEASURED_ROWS;
        let mut restore_offset = None;
        if reset {
            let row = self.heights.row_at(self.offset as f64);
            let within = self.offset as f64 - self.heights.prefix(row);
            let anchor_height = self.heights.measured.get(&row).copied();
            self.heights.reset(key.len, base as f64);
            // On eviction the current row's known height keeps its inner offset
            // valid. Resizing/fonts instead anchor the same file at its top:
            // an old long-row offset could skip several newly shorter rows.
            if same_key && row < key.len {
                if let Some(height) = anchor_height {
                    self.heights.measure(row, height);
                }
            }
            restore_offset = Some(if data_changed {
                0.0
            } else {
                (self.heights.prefix(row) + if same_key { within.max(0.0) } else { 0.0 }) as f32
            });
            self.key = Some(key);
        }
        #[cfg(test)]
        {
            self.rows_last_frame = 0;
            self.row_rects.clear();
        }
        let count = self.heights.len;
        let mut picked = None;
        // Save this screen-space origin before ScrollArea translates its content.
        // Combining hundred-million-point f32 content coordinates loses row detail.
        let origin = ui.available_rect_before_wrap().min;
        let mut area = egui::ScrollArea::vertical()
            .id_salt("wrapped-rows")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .min_scrolled_height(0.0)
            .auto_shrink([false, false]);
        if let Some(offset) = restore_offset {
            area = area.vertical_scroll_offset(offset);
        }
        let output = area.show_viewport(ui, |ui, viewport| {
            ui.set_width(width);
            if count == 0 {
                ui.set_height(0.0);
                return;
            }
            let first = self.heights.row_at(viewport.min.y as f64);
            let mut y = origin.y + (self.heights.prefix(first) - viewport.min.y as f64) as f32;
            let last_y = origin.y + viewport.height() + base;
            for row in first..count.min(first.saturating_add(MAX_MEASURED_ROWS)) {
                if y > last_y {
                    break;
                }
                let rect = Rect::from_min_size(egui::pos2(origin.x, y), Vec2::new(width, 0.0));
                // A positioned child must not advance the content cursor: doing
                // so would add the entire virtual extent below its visible rows.
                let mut row_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .id_salt(("wrapped-row", generation, row))
                        .max_rect(rect),
                );
                row_ui.set_width(width);
                let result = row_contents(&mut row_ui, row);
                let height = row_ui.min_rect().height().max(base - ROW_GAP);
                if let Some(result) = result {
                    picked = Some(result);
                }
                self.heights.measure(row, (height + ROW_GAP) as f64);
                #[cfg(test)]
                {
                    self.row_rects.push((
                        row,
                        Rect::from_min_size(egui::pos2(origin.x, y), Vec2::new(width, height)),
                    ));
                }
                y += height + ROW_GAP;
                #[cfg(test)]
                {
                    self.rows_last_frame += 1;
                }
            }
            let total = (self.heights.prefix(count) - ROW_GAP as f64).max(0.0);
            let rounded = total as f32;
            // Round the virtual extent upward: nearest-f32 rounding at millions
            // of rows must not make the final filename's last line unreachable.
            let extent = if (rounded as f64) < total {
                f32::from_bits(rounded.to_bits() + 1)
            } else {
                rounded
            };
            ui.set_height(extent);
        });
        self.offset = output.state.offset.y;
        picked
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::FileIdentity;
    use std::path::Path;

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        crate::configure(&ctx);
        ctx
    }
    fn fixture(name: &str) -> FileEntry {
        FileEntry {
            can_trash: true,
            path: Path::new("/data").join(name),
            parent: Path::new("/data").into(),
            size: 1024,
            identity: FileIdentity::default(),
        }
    }
    fn frame<T>(
        ctx: &egui::Context,
        size: Vec2,
        events: Vec<egui::Event>,
        mut render: impl FnMut(&mut egui::Ui) -> T,
    ) -> T {
        let mut value = None;
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)),
                events,
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| value = Some(render(ui)));
            },
        );
        value.unwrap()
    }
    fn wheel(delta: f32) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(egui::pos2(90.0, 150.0)),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, delta),
                modifiers: Default::default(),
            },
        ]
    }
    fn click(point: egui::Pos2, pressed: bool) -> Vec<egui::Event> {
        vec![
            egui::Event::PointerMoved(point),
            egui::Event::PointerButton {
                pos: point,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: Default::default(),
            },
        ]
    }

    #[test]
    fn sparse_prefix_heights_match_actual_variable_rows() {
        let mut heights = Heights::default();
        heights.reset(128, 40.0);
        let mut actual = vec![40.0; 128];
        for (row, height) in [(0, 100.0), (50, 160.0), (127, 80.0), (50, 120.0)] {
            actual[row] = height;
            heights.measure(row, height);
        }
        for end in 0..=128 {
            assert_eq!(heights.prefix(end), actual[..end].iter().sum::<f64>());
        }
        for row in 0..128 {
            let begin = actual[..row].iter().sum::<f64>();
            assert_eq!(heights.row_at(begin + actual[row] / 2.0), row);
        }
    }

    #[test]
    fn complete_unicode_and_unbroken_filenames_wrap_without_elision() {
        let ctx = context();
        frame(&ctx, Vec2::new(180.0, 700.0), vec![], |ui| {
            for name in [
                format!("br-{}.jsonl.brj", "9f1a2".repeat(40)),
                "中文缓存文件名称".repeat(20),
            ] {
                let galley = filename_layout(
                    ui,
                    name.clone(),
                    140.0,
                    egui::TextStyle::Body.resolve(ui.style()),
                );
                assert!(!galley.elided);
                assert_eq!(galley.text(), name);
                assert!(galley.rows.len() > 2);
                assert!(galley.size().x <= 141.0);
                assert_eq!(
                    galley
                        .rows
                        .iter()
                        .map(|row| row.glyphs.len())
                        .sum::<usize>(),
                    name.chars().count()
                );
            }
        });
    }

    #[test]
    fn wrapped_file_rows_do_not_overlap_and_filter_changes_cancel_pressed_selection() {
        let ctx = context();
        let files = vec![
            fixture("short"),
            fixture(&format!("br-{}.jsonl.brj", "9f1a2".repeat(40))),
            fixture("数据文件"),
        ];
        let mut state = WrappedFileList::default();
        let indices = vec![0, 1, 2];
        frame(&ctx, Vec2::new(220.0, 600.0), vec![], |ui| {
            state.show(ui, 1, &files, &indices, None, |_| Color32::BLUE)
        });
        assert!(state.row_rects[1].1.height() > state.row_rects[0].1.height() * 2.0);
        for pair in state.row_rects.windows(2) {
            assert!(pair[1].1.top() >= pair[0].1.bottom() + ROW_GAP - 0.01);
        }
        let point = state.row_rects[0].1.center();
        assert!(
            frame(&ctx, Vec2::new(220.0, 600.0), click(point, true), |ui| {
                state.show(ui, 1, &files, &indices, None, |_| Color32::BLUE)
            })
            .is_none()
        );
        let swapped = vec![2, 1, 0];
        assert!(
            frame(&ctx, Vec2::new(220.0, 600.0), click(point, false), |ui| {
                state.show(ui, 2, &files, &swapped, None, |_| Color32::BLUE)
            })
            .is_none(),
            "a release after a new filtered generation must not choose a different file"
        );
    }

    #[test]
    fn million_row_list_reaches_last_wrapped_row_and_selects_it() {
        let ctx = context();
        let mut state = WrappedFileList::default();
        let count = 3_500_000;
        let render = |ui: &mut egui::Ui, state: &mut WrappedFileList| {
            state.show_rows(ui, 1, count, |ui, row| {
                let height = if row == count - 1 { 133.0 } else { 47.0 };
                let (_, response) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
                response.clicked().then_some(row)
            })
        };
        frame(&ctx, Vec2::new(220.0, 320.0), vec![], |ui| {
            render(ui, &mut state)
        });
        assert!(state.rows_last_frame < 12);
        for _ in 0..12 {
            frame(
                &ctx,
                Vec2::new(220.0, 320.0),
                wheel(-1_000_000_000.0),
                |ui| render(ui, &mut state),
            );
        }
        let last = state
            .row_rects
            .iter()
            .find(|(row, _)| *row == count - 1)
            .expect("last row should be reachable")
            .1;
        assert!(
            last.top() >= -0.01 && last.bottom() <= 320.01,
            "last wrapped row must be fully visible: {last:?}"
        );
        assert!(state.rows_last_frame < 12);
        assert!(state.heights.measured.len() < MAX_MEASURED_ROWS + 12);
        let point = last.center();
        frame(&ctx, Vec2::new(220.0, 320.0), click(point, true), |ui| {
            render(ui, &mut state)
        });
        let selected = frame(&ctx, Vec2::new(220.0, 320.0), click(point, false), |ui| {
            render(ui, &mut state)
        });
        assert_eq!(selected, Some(count - 1));
    }

    #[test]
    fn width_change_preserves_anchor_file_without_reusing_oversized_inner_offset() {
        let ctx = context();
        let mut state = WrappedFileList::default();
        let draw = |ui: &mut egui::Ui, row: usize| {
            let text = format!("record-{row}-{}", "cache-file-name-".repeat(10));
            let galley = filename_layout(
                ui,
                text,
                ui.available_width(),
                egui::TextStyle::Body.resolve(ui.style()),
            );
            ui.add(egui::Label::new(galley));
            None::<usize>
        };
        frame(&ctx, Vec2::new(120.0, 320.0), vec![], |ui| {
            state.show_rows(ui, 1, 1000, draw)
        });
        for _ in 0..4 {
            frame(&ctx, Vec2::new(120.0, 320.0), wheel(-90.0), |ui| {
                state.show_rows(ui, 1, 1000, draw)
            });
        }
        let anchor = state.heights.row_at(state.offset as f64);
        frame(&ctx, Vec2::new(400.0, 320.0), vec![], |ui| {
            state.show_rows(ui, 1, 1000, draw)
        });
        assert_eq!(state.row_rects.first().unwrap().0, anchor);
        assert!(state.row_rects.first().unwrap().1.top().abs() < 0.01);
    }

    #[test]
    fn browsing_keeps_measurement_cache_bounded_without_losing_top_file() {
        let ctx = context();
        let mut state = WrappedFileList::default();
        let draw = |ui: &mut egui::Ui, row: usize| {
            ui.allocate_exact_size(
                Vec2::new(ui.available_width(), 47.0 + (row % 3) as f32 * 21.0),
                Sense::hover(),
            );
            None::<usize>
        };
        frame(&ctx, Vec2::new(220.0, 320.0), vec![], |ui| {
            state.show_rows(ui, 1, 3_500_000, draw)
        });
        let mut evicted = false;
        for _ in 0..160 {
            let count_before_wheel = state.heights.measured.len();
            frame(&ctx, Vec2::new(220.0, 320.0), wheel(-750.0), |ui| {
                state.show_rows(ui, 1, 3_500_000, draw)
            });
            evicted |= state.heights.measured.len() < count_before_wheel;
            let expected = state.heights.row_at(state.offset as f64);
            let count_before = state.heights.measured.len();
            frame(&ctx, Vec2::new(220.0, 320.0), vec![], |ui| {
                state.show_rows(ui, 1, 3_500_000, draw)
            });
            evicted |= state.heights.measured.len() < count_before;
            assert_eq!(
                state.row_rects.first().unwrap().0,
                expected,
                "height-cache eviction must retain the top file"
            );
            assert!(state.heights.measured.len() <= MAX_MEASURED_ROWS + 12);
            assert!(state.heights.deltas.len() < 14_000);
        }
        assert!(evicted, "the test must actually cross the lazy cache limit");
    }

    #[test]
    #[ignore = "release performance benchmark"]
    fn wrapped_million_row_frames_are_visible_work_only() {
        let ctx = context();
        let files = vec![
            fixture("small"),
            fixture(&format!("{}.cache", "c87b9".repeat(40))),
        ];
        let indices: Vec<_> = (0..3_500_000).map(|row| row % 2).collect();
        let mut state = WrappedFileList::default();
        let mut times = Vec::new();
        for frame_index in 0..90 {
            let now = std::time::Instant::now();
            frame(&ctx, Vec2::new(280.0, 650.0), vec![], |ui| {
                state.show(ui, 1, &files, &indices, None, |_| Color32::BLUE)
            });
            if frame_index > 10 {
                times.push(now.elapsed().as_secs_f64() * 1000.0);
            }
            assert!(state.rows_last_frame < 20);
        }
        times.sort_by(f64::total_cmp);
        eprintln!(
            "3.5m wrapped rows CPU p95={:.3}ms max={:.3}ms measured={} tree_nodes={}",
            times[times.len() * 95 / 100],
            times[times.len() - 1],
            state.heights.measured.len(),
            state.heights.deltas.len()
        );
        assert!(times[times.len() * 95 / 100] < 16.7);
    }
}
