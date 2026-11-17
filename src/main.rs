#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod file_actions;
mod file_list;
mod permission;
mod platform;
mod render;
mod scan;
mod treemap;

use eframe::egui::{self, Color32, FontId, Rect, RichText, Sense, Vec2};
use rayon::prelude::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

const BG: Color32 = Color32::from_rgb(19, 23, 29);
const PANEL: Color32 = Color32::from_rgb(26, 31, 39);
const TEXT: Color32 = Color32::from_rgb(226, 232, 239);
const MUTED: Color32 = Color32::from_rgb(143, 158, 176);
const ACCENT: Color32 = Color32::from_rgb(104, 196, 190);
const PALETTE: [Color32; 12] = [
    Color32::from_rgb(65, 143, 151),
    Color32::from_rgb(90, 126, 178),
    Color32::from_rgb(157, 117, 169),
    Color32::from_rgb(177, 133, 80),
    Color32::from_rgb(112, 151, 105),
    Color32::from_rgb(173, 107, 111),
    Color32::from_rgb(89, 153, 177),
    Color32::from_rgb(131, 127, 183),
    Color32::from_rgb(166, 151, 94),
    Color32::from_rgb(85, 159, 135),
    Color32::from_rgb(174, 121, 146),
    Color32::from_rgb(124, 147, 178),
];

fn bytes(n: u64) -> String {
    let mut value = n as f64;
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB", "PiB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if unit == "B" {
        format!("{n} B")
    } else {
        format!("{value:.1} {unit}")
    }
}
fn color(path: &std::path::Path) -> Color32 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    PALETTE[(h.finish() as usize) % PALETTE.len()]
}
fn configure(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for path in platform::font_paths() {
        if let Ok(data) = std::fs::read(path) {
            fonts
                .font_data
                .insert("chinese".into(), egui::FontData::from_owned(data).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .push("chinese".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("chinese".into());
            break;
        }
    }
    ctx.set_fonts(fonts);
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = PANEL;
    style.visuals.window_fill = PANEL;
    style.visuals.extreme_bg_color = BG;
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    style.visuals.widgets.active.bg_fill = ACCENT.gamma_multiply(0.5);
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style
        .text_styles
        .insert(egui::TextStyle::Body, FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, FontId::proportional(14.0));
    ctx.set_style(style);
}

fn is_trash_path(path: &std::path::Path) -> bool {
    file_actions::already_in_trash(path)
}

fn wrapped_text(ui: &mut egui::Ui, text: String, size: f32, color: Color32) -> egui::Response {
    let mut job = egui::text::LayoutJob::simple(
        text,
        FontId::proportional(size),
        color,
        ui.available_width(),
    );
    job.wrap.break_anywhere = true;
    ui.add(egui::Label::new(job).wrap())
}

fn path_text(ui: &mut egui::Ui, path: &std::path::Path) -> egui::Response {
    let text = path.to_string_lossy();
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let file_start = text.len().saturating_sub(name.len());
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = ui.available_width();
    job.wrap.break_anywhere = true;
    job.append(
        &text[..file_start],
        0.0,
        egui::TextFormat {
            font_id: FontId::proportional(15.0),
            color: MUTED,
            ..Default::default()
        },
    );
    job.append(
        &text[file_start..],
        0.0,
        egui::TextFormat {
            font_id: FontId::proportional(18.0),
            color: TEXT,
            ..Default::default()
        },
    );
    ui.add(egui::Label::new(job).wrap())
}

fn selected_path(ui: &mut egui::Ui, path: &std::path::Path) -> egui::Response {
    let label = path_text(ui, path);
    ui.interact(
        label.rect,
        egui::Id::new("copy-selected-path"),
        Sense::click(),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
    .on_hover_text("点击复制完整路径")
}

fn show_file_tooltip(
    response: &egui::Response,
    file: &scan::FileEntry,
) -> Option<egui::InnerResponse<()>> {
    // List rows share this helper with the treemap, so gate the always-open
    // tooltip here rather than relying on the caller's treemap hit test.
    if !response.enabled() || !response.hovered() {
        return None;
    }
    let width = (response.ctx.content_rect().width() - 32.0).clamp(1.0, 420.0);
    // Skip the standard tooltip delay so moving across files updates immediately.
    egui::Tooltip::for_widget(response)
        .at_pointer()
        .width(width)
        .show(|ui| file_tooltip(ui, file))
}

fn file_tooltip(ui: &mut egui::Ui, file: &scan::FileEntry) {
    let available = (ui.ctx().content_rect().width() - 32.0).max(1.0);
    ui.set_width(available.min(420.0));
    wrapped_text(
        ui,
        file.path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        15.0,
        TEXT,
    );
    ui.label(bytes(file.size));
    wrapped_text(ui, file.path.to_string_lossy().into_owned(), 14.0, MUTED);
}

fn defer_drop<T: Send + 'static>(value: T) {
    rayon::spawn(move || drop(value));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NoticeKind {
    Success,
    Info,
    Error,
}
struct Notice {
    text: String,
    kind: NoticeKind,
    expires: Option<Instant>,
}
impl Notice {
    fn new(text: String, kind: NoticeKind) -> Self {
        Self {
            text,
            kind,
            expires: (kind != NoticeKind::Error).then(|| Instant::now() + Duration::from_secs(5)),
        }
    }
    fn expired(&self) -> bool {
        self.expires
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

struct TrashOutput {
    source: Arc<scan::ScanResult>,
    id: usize,
    result: Result<(), String>,
}

struct RestoreOutput {
    source: Arc<scan::ScanResult>,
    result: Result<Option<scan::ScanResult>, String>,
}

fn relocate_snapshot(
    source: &scan::ScanResult,
    old_path: &std::path::Path,
    identity: scan::FileIdentity,
    moved: Option<scan::FileEntry>,
) -> Option<scan::ScanResult> {
    let moved = moved?;
    let matches: Vec<_> = source
        .files
        .par_iter()
        .enumerate()
        .filter_map(|(i, file)| (file.path == old_path && file.identity == identity).then_some(i))
        .collect();
    if matches.len() != 1 {
        return None;
    }
    let id = matches[0];
    let mut files: Vec<_> = source
        .files
        .par_iter()
        .enumerate()
        .map(|(i, file)| if i == id { moved.clone() } else { file.clone() })
        .collect();
    files.par_sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    Some(scan::ScanResult {
        root: source.root.clone(),
        files,
        skipped: source.skipped,
        errors: source.errors.clone(),
        elapsed: source.elapsed,
    })
}

struct FilterOutput {
    generation: u64,
    indices: Vec<usize>,
    bytes: u64,
}
#[derive(Clone, Copy, Debug, PartialEq)]
struct LayoutKey {
    logical_size: Vec2,
    physical_size: [usize; 2],
}
impl LayoutKey {
    fn new(size: Vec2, pixels_per_point: f32) -> Self {
        let logical_size = size.max(Vec2::splat(1.0)).ceil();
        let scale = pixels_per_point.max(0.1);
        Self {
            logical_size,
            physical_size: [
                (logical_size.x * scale).ceil() as usize,
                (logical_size.y * scale).ceil() as usize,
            ],
        }
    }
}
struct LayoutOutput {
    generation: u64,
    key: LayoutKey,
    map: render::RenderMap,
}
// One generation owns geometry, pixels and hit testing together. A resize may
// stretch this entire cache while its replacement is prepared off-thread.
struct MapCache {
    generation: u64,
    key: LayoutKey,
    map: render::RenderMap,
    texture: egui::TextureHandle,
    // Pixel-aligned holes reuse the existing texture without a full upload.
    holes: Vec<treemap::Rect>,
}
impl MapCache {
    fn hide(&mut self, id: usize) {
        self.holes.extend(self.map.erase(id));
    }
}

fn filter_snapshot(
    data: &scan::ScanResult,
    query: &str,
    known_bytes: Option<u64>,
    hidden: &BTreeSet<usize>,
    token: &AtomicU64,
    generation: u64,
) -> Option<FilterOutput> {
    let query = query.to_lowercase();
    let parts: Result<Vec<Vec<usize>>, ()> = data
        .files
        .par_chunks(4096)
        .enumerate()
        .map(|(chunk, files)| {
            if token.load(Ordering::Relaxed) != generation {
                return Err(());
            }
            Ok(files
                .iter()
                .enumerate()
                .filter_map(|(offset, file)| {
                    (!hidden.contains(&(chunk * 4096 + offset))
                        && (query.is_empty()
                            || file.path.to_string_lossy().to_lowercase().contains(&query)))
                    .then_some(chunk * 4096 + offset)
                })
                .collect())
        })
        .collect();
    let indices = parts.ok()?.into_iter().flatten().collect();
    let bytes = known_bytes.unwrap_or_else(|| {
        data.files
            .par_iter()
            .map(|file| file.size)
            .reduce(|| 0, u64::saturating_add)
    });
    (token.load(Ordering::Relaxed) == generation).then_some(FilterOutput {
        generation,
        indices,
        bytes,
    })
}

fn layout_snapshot(
    data: &scan::ScanResult,
    key: LayoutKey,
    token: &AtomicU64,
    generation: u64,
) -> Option<render::RenderMap> {
    let grouped: Result<BTreeMap<&std::path::Path, Vec<(usize, u64)>>, ()> = data
        .files
        .par_iter()
        .enumerate()
        .try_fold(BTreeMap::new, |mut groups, (i, file)| {
            if i % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
                return Err(());
            }
            groups
                .entry(file.parent.as_path())
                .or_insert_with(Vec::new)
                .push((i, file.size.max(1)));
            Ok(groups)
        })
        .try_reduce(BTreeMap::new, |mut a, b| {
            for (path, mut files) in b {
                a.entry(path).or_insert_with(Vec::new).append(&mut files);
            }
            Ok(a)
        });
    let groups: Vec<_> = grouped.ok()?.into_iter().collect();
    if token.load(Ordering::Relaxed) != generation {
        return None;
    }
    let weights: Vec<_> = groups
        .par_iter()
        .enumerate()
        .map(|(i, (_, files))| (i, files.iter().fold(0u64, |n, (_, s)| n.saturating_add(*s))))
        .collect();
    let bounds = treemap::Rect {
        x: 0.0,
        y: 0.0,
        w: key.logical_size.x,
        h: key.logical_size.y,
    };
    let group_bounds = treemap::layout_with_cancel(&weights, bounds, token, generation)?;
    let pieces: Result<Vec<Vec<render::RenderEntry>>, ()> = group_bounds
        .into_par_iter()
        .map(|(i, bounds)| {
            if token.load(Ordering::Relaxed) != generation {
                return Err(());
            }
            let (path, files) = &groups[i];
            let c = color(path);
            Ok(
                treemap::layout_with_cancel(files, bounds, token, generation)
                    .ok_or(())?
                    .into_iter()
                    .map(|(id, rect)| render::RenderEntry { id, rect, color: c })
                    .collect(),
            )
        })
        .collect();
    let entries = pieces.ok()?.into_iter().flatten().collect();
    if token.load(Ordering::Relaxed) != generation {
        return None;
    }
    render::RenderMap::build_with_size(entries, bounds, key.physical_size, token, generation)
}

struct Job {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<scan::ScanProgress>>,
    rx: mpsc::Receiver<Result<scan::ScanResult, String>>,
}
struct App {
    access: permission::Access,
    permission_rx: Option<mpsc::Receiver<permission::Access>>,
    pending_scan: bool,
    data: Option<Arc<scan::ScanResult>>,
    hidden: Arc<BTreeSet<usize>>,
    job: Option<Job>,
    reveal_rx: Option<mpsc::Receiver<Result<(), String>>>,
    trash_rx: Option<mpsc::Receiver<TrashOutput>>,
    query: String,
    selected: Option<usize>,
    message: Option<String>,
    action_notice: Option<String>,
    notice: Option<Notice>,
    show_errors: bool,
    show_recoveries: bool,
    recovery_rx: Option<mpsc::Receiver<Result<Vec<file_actions::RecoveryRecord>, String>>>,
    recovery_records: Vec<file_actions::RecoveryRecord>,
    recovery_list: file_list::WrappedFileList,
    recovery_generation: u64,
    recoveries_loaded: bool,
    recoveries_dirty: bool,
    restore_rx: Option<mpsc::Receiver<RestoreOutput>>,
    canvas_rect: Option<Rect>,
    filtered: Vec<usize>,
    file_list: file_list::WrappedFileList,
    total_bytes: u64,
    dirty: bool,
    filter_rx: Option<mpsc::Receiver<FilterOutput>>,
    filter_generation: Arc<AtomicU64>,
    filter_applied: u64,
    totals_ready: bool,
    layout_rx: Option<mpsc::Receiver<LayoutOutput>>,
    layout_generation: Arc<AtomicU64>,
    layout_key: Option<LayoutKey>,
    map_cache: Option<MapCache>,
}
impl Default for App {
    fn default() -> Self {
        Self {
            access: permission::Access::Unverified,
            permission_rx: None,
            pending_scan: false,
            data: None,
            hidden: Arc::default(),
            job: None,
            reveal_rx: None,
            trash_rx: None,
            query: String::new(),
            selected: None,
            message: None,
            action_notice: None,
            notice: None,
            show_errors: false,
            show_recoveries: false,
            recovery_rx: None,
            recovery_records: Vec::new(),
            recovery_list: file_list::WrappedFileList::default(),
            recovery_generation: 0,
            recoveries_loaded: false,
            recoveries_dirty: false,
            restore_rx: None,
            canvas_rect: None,
            filtered: Vec::new(),
            file_list: file_list::WrappedFileList::default(),
            total_bytes: 0,
            dirty: true,
            filter_rx: None,
            filter_generation: Arc::new(AtomicU64::new(0)),
            filter_applied: 0,
            totals_ready: false,
            layout_rx: None,
            layout_generation: Arc::new(AtomicU64::new(0)),
            layout_key: None,
            map_cache: None,
        }
    }
}
impl App {
    fn start(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.queue_permission_check(rx);
        std::thread::spawn(move || {
            let _ = tx.send(permission::check());
        });
    }
    fn queue_permission_check(&mut self, rx: mpsc::Receiver<permission::Access>) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            defer_drop(job);
        }
        self.pending_scan = true;
        self.access = permission::Access::Unverified;
        self.permission_rx = Some(rx);
        self.message = None;
    }
    fn permission_result(&mut self, access: permission::Access) -> bool {
        self.access = access;
        self.permission_rx = None;
        access == permission::Access::Granted && std::mem::take(&mut self.pending_scan)
    }
    fn launch_scan(&mut self) {
        if self.access != permission::Access::Granted {
            return;
        }
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            defer_drop(job);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(scan::ScanProgress {
            files: 0,
            bytes: 0,
            skipped: 0,
        }));
        let (tx, rx) = mpsc::channel();
        let (c, stats) = (cancel.clone(), progress.clone());
        std::thread::spawn(move || {
            let _ = tx.send(platform::scan_root().and_then(|root| scan::scan(root, c, stats)));
        });
        self.job = Some(Job {
            cancel,
            progress,
            rx,
        });
        self.message = None;
    }
    fn replace_snapshot(&mut self, data: scan::ScanResult) {
        defer_drop(std::mem::take(&mut self.hidden));
        if let Some(old) = self.data.replace(Arc::new(data)) {
            defer_drop(old);
        }
        self.filter_generation.fetch_add(1, Ordering::Relaxed);
        self.layout_generation.fetch_add(1, Ordering::Relaxed);
        if let Some(old) = self.filter_rx.take() {
            defer_drop(old);
        }
        if let Some(old) = self.layout_rx.take() {
            defer_drop(old);
        }
        if let Some(old) = self.map_cache.take() {
            defer_drop(old);
        }
        self.filtered.clear();
        self.total_bytes = 0;
        self.totals_ready = false;
        self.selected = None;
        self.query.clear();
        self.dirty = true;
        self.layout_key = None;
    }
    fn trash(&mut self, id: usize) {
        if self.access != permission::Access::Granted
            || self.trash_rx.is_some()
            || self.restore_rx.is_some()
            || self.hidden.contains(&id)
        {
            return;
        }
        let Some(source) = self.data.clone() else {
            return;
        };
        let Some(file) = source.files.get(id) else {
            return;
        };
        let path = file.path.clone();
        let identity = file.identity;
        let (tx, rx) = mpsc::channel();
        self.trash_rx = Some(rx);
        self.action_notice = None;
        self.set_notice(Notice {
            text: format!("正在移到{}…", platform::TRASH_NAME),
            kind: NoticeKind::Info,
            expires: None,
        });
        rayon::spawn(move || {
            let result = file_actions::move_to_trash(&path, identity).map(|_| ());
            let _ = tx.send(TrashOutput { source, id, result });
        });
    }
    fn load_recoveries(&mut self) {
        self.recoveries_dirty = true;
        self.pump_recoveries();
    }
    fn pump_recoveries(&mut self) {
        #[cfg(test)]
        {
            self.recoveries_loaded = true;
            self.recoveries_dirty = false;
        }
        #[cfg(not(test))]
        {
            if self.recovery_rx.is_some() || !self.recoveries_dirty {
                return;
            }
            self.recoveries_dirty = false;
            let (tx, rx) = mpsc::channel();
            self.recovery_rx = Some(rx);
            rayon::spawn(move || {
                let _ = tx.send(file_actions::recoveries());
            });
        }
    }
    fn restore(&mut self, record_id: &str) {
        if self.access != permission::Access::Granted
            || self.restore_rx.is_some()
            || self.trash_rx.is_some()
        {
            return;
        }
        let Some(record) = self
            .recovery_records
            .iter()
            .find(|record| record.id == record_id)
        else {
            return;
        };
        let id = record.id.clone();
        let old_path = record.trash_path.clone();
        let identity = record.identity;
        let source = self.data.clone();
        let (tx, rx) = mpsc::channel();
        self.restore_rx = Some(rx);
        self.set_notice(Notice {
            text: "正在放回原处…".into(),
            kind: NoticeKind::Info,
            expires: None,
        });
        rayon::spawn(move || {
            let result = file_actions::restore(&id).map(|moved| match (&source, &old_path) {
                (Some(source), Some(old_path)) => {
                    relocate_snapshot(source, old_path, identity, moved)
                }
                _ => None,
            });
            // An empty placeholder can never match a current scan snapshot.
            let source = source.unwrap_or_else(|| {
                Arc::new(scan::ScanResult {
                    root: "/".into(),
                    files: Vec::new(),
                    skipped: 0,
                    errors: Vec::new(),
                    elapsed: Duration::ZERO,
                })
            });
            let _ = tx.send(RestoreOutput { source, result });
        });
    }
    fn set_notice(&mut self, notice: Notice) {
        if notice.kind != NoticeKind::Error
            && self
                .notice
                .as_ref()
                .is_some_and(|current| current.kind == NoticeKind::Error)
        {
            return;
        }
        self.notice = Some(notice);
    }
    fn collect_notices(&mut self) {
        if self.notice.as_ref().is_some_and(Notice::expired) {
            self.notice = None;
        }
        let success = self.action_notice.take();
        if let Some(error) = self.message.take() {
            self.set_notice(Notice::new(error, NoticeKind::Error));
        } else if let Some(success) = success {
            self.set_notice(Notice::new(success, NoticeKind::Success));
        }
    }
    fn overlays(&mut self, ctx: &egui::Context) {
        self.collect_notices();
        let mut dismiss = false;
        if let Some(notice) = &self.notice {
            if let Some(deadline) = notice.expires {
                ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
            }
            egui::Area::new("notification".into())
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_BOTTOM, [0.0, -44.0])
                .show(ctx, |ui| {
                    ui.set_max_width((ctx.content_rect().width() - 40.0).clamp(200.0, 560.0));
                    egui::Frame::new()
                        .fill(PANEL)
                        .inner_margin(14.0)
                        .corner_radius(8.0)
                        .stroke(egui::Stroke::new(1.0, MUTED.gamma_multiply(0.3)))
                        .show(ui, |ui| {
                            ui.horizontal_top(|ui| {
                                let color = match notice.kind {
                                    NoticeKind::Success => ACCENT,
                                    NoticeKind::Info => TEXT,
                                    NoticeKind::Error => Color32::from_rgb(235, 180, 116),
                                };
                                ui.add(
                                    egui::Label::new(RichText::new(&notice.text).color(color))
                                        .wrap(),
                                );
                                if ui.small_button("×").on_hover_text("关闭提示").clicked() {
                                    dismiss = true;
                                }
                            });
                        });
                });
        }
        if dismiss {
            self.notice = None;
        }
        if self.show_errors && self.access == permission::Access::Granted {
            let mut open = true;
            egui::Window::new("无法读取的路径")
                .open(&mut open)
                .default_width(540.0)
                .default_height(280.0)
                .show(ctx, |ui| {
                    if let Some(data) = &self.data {
                        egui::ScrollArea::vertical()
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            )
                            .max_height(360.0)
                            .show(ui, |ui| {
                                for error in &data.errors {
                                    ui.add(egui::Label::new(error).wrap());
                                }
                            });
                    }
                });
            self.show_errors = open;
        }
        if self.show_recoveries && self.access == permission::Access::Granted {
            let mut open = true;
            let mut restore = None;
            egui::Window::new("恢复文件")
                .open(&mut open)
                .default_width(580.0)
                .default_height(300.0)
                .show(ctx, |ui| {
                    ui.label(RichText::new("仅显示本应用记录的文件").small().color(MUTED));
                    if self.recovery_rx.is_some() {
                        ui.label("正在读取恢复记录…");
                    }
                    if self.recovery_records.is_empty() && self.recovery_rx.is_none() {
                        ui.label("没有可用的恢复记录。");
                    }
                    let records = &self.recovery_records;
                    let busy = self.restore_rx.is_some() || self.trash_rx.is_some();
                    restore = self.recovery_list.show_rows(
                        ui,
                        self.recovery_generation,
                        records.len(),
                        |ui, index| {
                            let record = &records[index];
                            let mut restore = None;
                            ui.horizontal_top(|ui| {
                                let width = (ui.available_width() - 110.0).max(100.0);
                                ui.allocate_ui_with_layout(
                                    Vec2::new(width, 0.0),
                                    egui::Layout::top_down(egui::Align::LEFT),
                                    |ui| {
                                        path_text(ui, &record.original_path);
                                    },
                                );
                                if ui
                                    .add_enabled(!busy, egui::Button::new("放回原处"))
                                    .clicked()
                                {
                                    restore = Some(record.id.clone());
                                }
                            });
                            ui.separator();
                            restore
                        },
                    );
                });
            self.show_recoveries = open;
            if let Some(record) = restore {
                self.restore(&record);
            }
        }
    }
    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.permission_rx {
            match rx.try_recv() {
                Ok(access) => {
                    if self.permission_result(access) {
                        self.launch_scan();
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.permission_result(permission::Access::Unverified);
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.reveal_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.reveal_rx = None;
                    if let Err(error) = result {
                        self.message = Some(error);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.reveal_rx = None;
                    self.message = Some("文件管理器操作未完成，请重试。".into());
                }
                _ => {}
            }
        }
        if let Some(job) = &self.job {
            match job.rx.try_recv() {
                Ok(result) => {
                    let cancelled = job.cancel.load(Ordering::Relaxed);
                    self.job = None;
                    if cancelled {
                        defer_drop(result);
                        self.set_notice(Notice::new(
                            "已取消扫描，保留上次扫描结果。".into(),
                            NoticeKind::Info,
                        ));
                    } else {
                        match result {
                            Ok(data) => self.replace_snapshot(data),
                            Err(e) => self.message = Some(e),
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.job = None;
                    self.message = Some("扫描任务意外结束，请重新扫描。".into());
                }
                _ => {}
            }
        }
        if let Some(rx) = &self.trash_rx {
            match rx.try_recv() {
                Ok(output) => {
                    self.trash_rx = None;
                    self.message = None;
                    match output.result {
                        Ok(()) => {
                            if self
                                .data
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &output.source))
                            {
                                self.hide_file(output.id);
                                // A running scan may have read the old path already.
                                if self.job.is_some() {
                                    self.launch_scan();
                                }
                            } else {
                                // Never apply an old slot id to a newer scan.
                                self.launch_scan();
                            }
                            self.action_notice = Some(format!("已移到{}。", platform::TRASH_NAME));
                        }
                        Err(error) => self.message = Some(error),
                    }
                    defer_drop(output.source);
                    if !file_actions::SYSTEM_RECOVERY {
                        self.load_recoveries();
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.trash_rx = None;
                    self.message = Some(format!(
                        "{}操作未完成，请重新扫描后重试。",
                        platform::TRASH_NAME
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.restore_rx {
            match rx.try_recv() {
                Ok(output) => {
                    self.restore_rx = None;
                    match output.result {
                        Ok(Some(data))
                            if self
                                .data
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &output.source)) =>
                        {
                            let refresh = self.job.is_some();
                            self.replace_snapshot(data);
                            if refresh {
                                self.start();
                            }
                            self.action_notice = Some("已放回原处。".into());
                        }
                        Ok(data) => {
                            defer_drop(data);
                            self.start();
                            self.action_notice = Some("已放回原处。".into());
                        }
                        Err(error) => self.message = Some(error),
                    }
                    defer_drop(output.source);
                    self.load_recoveries();
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.restore_rx = None;
                    self.message = Some("恢复操作未完成，请刷新恢复记录后重试。".into());
                    self.load_recoveries();
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.recovery_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.recovery_rx = None;
                    self.recoveries_loaded = true;
                    match result {
                        Ok(records) => {
                            defer_drop(std::mem::replace(&mut self.recovery_records, records));
                            self.recovery_generation = self.recovery_generation.wrapping_add(1);
                        }
                        Err(error) => self.message = Some(error),
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.recovery_rx = None;
                    self.recoveries_loaded = true;
                    self.message = Some("无法读取恢复记录，请重新打开恢复文件窗口。".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.pump_recoveries();
        if let Some(rx) = &self.filter_rx {
            match rx.try_recv() {
                Ok(output) => {
                    self.filter_rx = None;
                    if output.generation == self.filter_generation.load(Ordering::Relaxed) {
                        self.filter_applied = output.generation;
                        defer_drop(std::mem::replace(&mut self.filtered, output.indices));
                        self.total_bytes = output.bytes;
                        self.totals_ready = true;
                    } else {
                        defer_drop(output);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.filter_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.layout_rx {
            match rx.try_recv() {
                Ok(mut output) => {
                    self.layout_rx = None;
                    if output.generation == self.layout_generation.load(Ordering::Relaxed)
                        && self.layout_key == Some(output.key)
                    {
                        if let Some(image) = output.map.image.take() {
                            let mut cache = MapCache {
                                generation: output.generation,
                                key: output.key,
                                map: output.map,
                                texture: ctx.load_texture(
                                    "disk-map",
                                    image,
                                    egui::TextureOptions::NEAREST,
                                ),
                                holes: Vec::new(),
                            };
                            // Resize work can finish after a deletion. Apply the
                            // current holes before displaying its replacement.
                            for &id in self.hidden.iter() {
                                cache.hide(id);
                            }
                            if let Some(old) = self.map_cache.replace(cache) {
                                defer_drop(old);
                            }
                        } else {
                            defer_drop(output);
                        }
                    } else {
                        defer_drop(output);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.layout_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.job.is_some()
            || self.reveal_rx.is_some()
            || self.trash_rx.is_some()
            || self.restore_rx.is_some()
            || self.recovery_rx.is_some()
            || self.permission_rx.is_some()
            || self.filter_rx.is_some()
            || self.layout_rx.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
    fn hide_file(&mut self, id: usize) {
        if self.data.as_ref().is_none_or(|data| id >= data.files.len())
            || !Arc::make_mut(&mut self.hidden).insert(id)
        {
            return;
        }
        let current = self.filter_rx.is_none()
            && !self.dirty
            && self.filter_applied == self.filter_generation.load(Ordering::Relaxed);
        let generation = self.filter_generation.fetch_add(1, Ordering::Relaxed) + 1;
        if let Some(old) = self.filter_rx.take() {
            defer_drop(old);
        }
        // Reuse an applied query; an in-flight query is restarted with hidden ids.
        self.filtered.retain(|&file| file != id);
        if current {
            self.filter_applied = generation;
        }
        if self.selected == Some(id) {
            self.selected = None;
        }
        if let Some(cache) = &mut self.map_cache {
            cache.hide(id);
        }
    }
    fn filter(&mut self) {
        if self.dirty {
            self.dirty = false;
            self.filter_generation.fetch_add(1, Ordering::Relaxed);
        }
        let generation = self.filter_generation.load(Ordering::Relaxed);
        if self.filter_rx.is_some() || self.filter_applied == generation {
            return;
        }
        let Some(data) = self.data.clone() else {
            return;
        };
        let query = self.query.clone();
        let known = self.totals_ready.then_some(self.total_bytes);
        let hidden = self.hidden.clone();
        let token = self.filter_generation.clone();
        let (tx, rx) = mpsc::channel();
        self.filter_rx = Some(rx);
        rayon::spawn(move || {
            if let Some(output) = filter_snapshot(&data, &query, known, &hidden, &token, generation)
            {
                let _ = tx.send(output);
            }
        });
    }
    fn layout(&mut self, size: Vec2, pixels_per_point: f32) {
        let key = LayoutKey::new(size, pixels_per_point);
        if self.layout_key != Some(key) {
            self.layout_key = Some(key);
            self.layout_generation.fetch_add(1, Ordering::Relaxed);
        }
        let generation = self.layout_generation.load(Ordering::Relaxed);
        if self.layout_rx.is_some()
            || self
                .map_cache
                .as_ref()
                .is_some_and(|cache| cache.generation == generation && cache.key == key)
        {
            return;
        }
        let Some(data) = self.data.clone() else {
            return;
        };
        let token = self.layout_generation.clone();
        let (tx, rx) = mpsc::channel();
        self.layout_rx = Some(rx);
        rayon::spawn(move || {
            if let Some(map) = layout_snapshot(&data, key, &token, generation) {
                let _ = tx.send(LayoutOutput {
                    generation,
                    key,
                    map,
                });
            }
        });
    }
    fn reveal(&mut self, i: usize) {
        if self.reveal_rx.is_some() {
            return;
        }
        let Some(data) = &self.data else {
            return;
        };
        let path = data.files[i].path.clone();
        let (tx, rx) = mpsc::channel();
        self.reveal_rx = Some(rx);
        std::thread::spawn(move || {
            let result = platform::reveal(&path);
            let _ = tx.send(result);
        });
    }
    #[cfg(windows)]
    fn open_recycle_bin(&mut self) {
        if self.reveal_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.reveal_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(file_actions::open_recycle_bin());
        });
    }
    fn open_permissions(&mut self) {
        if self.reveal_rx.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.reveal_rx = Some(rx);
        std::thread::spawn(move || {
            let result = std::process::Command::new("/usr/bin/open")
                .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
                .output();
            let result = match result {
                Ok(output) if output.status.success() => Ok(()),
                _ => Err("无法打开系统设置，请手动前往隐私与安全性 > 完全磁盘访问权限。".into()),
            };
            let _ = tx.send(result);
        });
    }
    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status")
            .exact_height(36.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(8.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let status = if let Some(job) = &self.job {
                        let p = *job.progress.lock().unwrap_or_else(|e| e.into_inner());
                        format!(
                            "扫描中 · {} 个文件 · {} · 跳过 {} 项{}",
                            p.files,
                            bytes(p.bytes),
                            p.skipped,
                            if self.data.is_some() {
                                " · 当前为上次扫描结果"
                            } else {
                                ""
                            }
                        )
                    } else if let Some(data) = &self.data {
                        format!(
                            "启动磁盘 · {:.1} 秒 · 跳过 {} 项",
                            data.elapsed.as_secs_f64(),
                            data.skipped
                        )
                    } else {
                        "启动磁盘 · 扫描已停止".into()
                    };
                    ui.add(egui::Label::new(RichText::new(status).small().color(MUTED)).truncate());
                    if self.layout_rx.is_some() {
                        ui.spinner();
                    }
                    if let Some(data) = &self.data {
                        let count = data.errors.len();
                        if count > 0
                            && ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(format!("无法读取的路径（{count}）"))
                                            .small()
                                            .color(MUTED),
                                    )
                                    .small()
                                    .frame_when_inactive(false),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand)
                                .clicked()
                        {
                            self.show_errors = true;
                        }
                    }
                });
            });
    }
    fn ui(&mut self, ctx: &egui::Context) {
        self.canvas_rect = None;
        self.poll(ctx);
        if self.access != permission::Access::Granted {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BG))
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((ui.available_height() * 0.28).max(24.0));
                        ui.label(
                            RichText::new(if cfg!(windows) {
                                "正在准备磁盘扫描"
                            } else {
                                "需要完全磁盘访问权限"
                            })
                            .size(28.0)
                            .strong(),
                        );
                        ui.add_space(12.0);
                        if self.permission_rx.is_some() {
                            ui.spinner();
                            ui.label("正在检查权限…");
                        } else {
                            let reason = if cfg!(windows) {
                                "Windows 启动检查未完成。"
                            } else if self.access == permission::Access::Denied {
                                "macOS 尚未授权。"
                            } else {
                                "无法确认权限。"
                            };
                            ui.label(RichText::new(reason).color(MUTED));
                            ui.add_space(12.0);
                            if cfg!(windows) {
                                ui.label("无法完成启动检查，请重新检查后重试。");
                            } else {
                                ui.label(
                                    "在系统设置中添加并开启 /Applications/Visualize SSD.app。",
                                );
                                ui.label("授权后点击重新检查；若系统要求，请退出并重新打开应用。");
                            }
                            ui.add_space(20.0);
                            ui.horizontal(|ui| {
                                ui.add_space((ui.available_width() * 0.5 - 150.0).max(0.0));
                                if cfg!(target_os = "macos")
                                    && ui.button("打开完全磁盘访问权限设置").clicked()
                                {
                                    self.open_permissions();
                                }
                                if ui.button("重新检查").clicked() {
                                    self.start();
                                }
                            });
                        }
                    });
                });
            self.overlays(ctx);
            return;
        }
        if !file_actions::SYSTEM_RECOVERY && !self.recoveries_loaded && self.recovery_rx.is_none() {
            self.load_recoveries();
        }
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(16.0))
            .show(ctx, |ui| {
                let padding = ui.spacing().button_padding;
                let control_height =
                    ui.text_style_height(&egui::TextStyle::Button) + 2.0 * padding.y;
                ui.spacing_mut().interact_size.y = control_height;
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(self.job.is_none(), egui::Button::new("重新扫描"))
                        .clicked()
                    {
                        self.start();
                    }
                    if ui
                        .add_enabled(self.job.is_some(), egui::Button::new("取消扫描"))
                        .clicked()
                    {
                        if let Some(job) = &self.job {
                            job.cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    if ui
                        .button(if file_actions::SYSTEM_RECOVERY {
                            "打开回收站"
                        } else {
                            "恢复文件"
                        })
                        .clicked()
                    {
                        #[cfg(windows)]
                        self.open_recycle_bin();
                        #[cfg(not(windows))]
                        {
                            self.show_recoveries = true;
                            self.load_recoveries();
                        }
                    }
                    if let Some(data) = &self.data {
                        let count = format!("{} 个文件", data.files.len());
                        let total = if self.totals_ready {
                            bytes(self.total_bytes)
                        } else {
                            "统计中…".into()
                        };
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let text_width = |text: &str| {
                            ui.painter()
                                .layout_no_wrap(text.into(), font.clone(), TEXT)
                                .size()
                                .x
                        };
                        let gap = ui.spacing().item_spacing.x;
                        let summary_width = text_width(&count) + gap + text_width(&total);
                        let clear_width = text_width("清除") + 2.0 * padding.x;
                        let search_width =
                            (ui.available_width() - summary_width - clear_width - 2.0 * gap)
                                .clamp(80.0, 360.0);
                        let response = ui.add_sized(
                            [search_width, control_height],
                            egui::TextEdit::singleline(&mut self.query)
                                .hint_text("搜索文件列表")
                                .margin(padding)
                                .desired_width(search_width),
                        );
                        if response.changed() {
                            self.dirty = true;
                        }
                        if ui
                            .add_enabled(!self.query.is_empty(), egui::Button::new("清除"))
                            .clicked()
                        {
                            self.query.clear();
                            self.dirty = true;
                        }
                        let spare = ui.available_width() - summary_width;
                        if spare > gap {
                            ui.add_space(spare);
                        }
                        ui.label(count);
                        ui.label(RichText::new(total).strong().color(ACCENT))
                            .on_hover_text("文件逻辑大小");
                    }
                });
            });
        self.filter();
        if self.data.is_none() {
            self.status_bar(ctx);
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BG))
                .show(ctx, |ui| {
                    self.canvas_rect = Some(ui.max_rect());
                    ui.vertical_centered(|ui| {
                        ui.add_space((ui.available_height() * 0.32).max(32.0));
                        let heading = if self.job.is_some() {
                            "扫描中…"
                        } else {
                            "扫描已停止"
                        };
                        ui.label(RichText::new(heading).size(30.0).strong());
                        ui.add_space(20.0);
                        if self.job.is_none() && ui.button("重新扫描").clicked() {
                            self.start();
                        }
                    });
                });
            self.overlays(ctx);
            return;
        }
        let mut reveal = None;
        let mut trash = None;
        let mut restore = None;
        egui::SidePanel::right("files")
            .default_width(280.0)
            .min_width(230.0)
            .max_width(450.0)
            .resizable(true)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(16.0))
            .show(ctx, |ui| {
                if let Some(i) = self.selected {
                    let f = &self.data.as_ref().unwrap().files[i];
                    ui.label(RichText::new(bytes(f.size)).size(24.0).color(ACCENT));
                    ui.horizontal_wrapped(|ui| {
                        if ui.button(platform::REVEAL_LABEL).clicked() {
                            reveal = Some(i);
                        }
                        if ui
                            .add_enabled(
                                self.trash_rx.is_none()
                                    && self.restore_rx.is_none()
                                    && !is_trash_path(&f.path),
                                egui::Button::new(format!("移到{}", platform::TRASH_NAME)),
                            )
                            .clicked()
                        {
                            trash = Some(i);
                        }
                    });
                    if let Some(record) = self.recovery_records.iter().find(|record| {
                        record.trash_path.as_ref() == Some(&f.path) && record.identity == f.identity
                    }) {
                        let id = record.id.clone();
                        if ui
                            .add_enabled(
                                self.restore_rx.is_none() && self.trash_rx.is_none(),
                                egui::Button::new("放回原处"),
                            )
                            .clicked()
                        {
                            restore = Some(id);
                        }
                    }
                    if selected_path(ui, &f.path).clicked() {
                        if let Some(exact) = f.path.to_str() {
                            ctx.copy_text(exact.to_owned());
                            self.action_notice = Some("路径已复制。".into());
                        } else {
                            self.message =
                                Some("路径包含无法复制为文本的字符，请在文件管理器中查看。".into());
                        }
                    }
                    ui.separator();
                }
                let status = if self.filter_rx.is_some() {
                    Some("更新中…")
                } else if self.filtered.is_empty() {
                    Some("没有匹配的文件")
                } else {
                    None
                };
                if let Some(status) = status {
                    ui.add(egui::Label::new(RichText::new(status).color(MUTED)).truncate());
                }
                let data = self.data.as_ref().unwrap();
                if let Some(selected) = self.file_list.show(
                    ui,
                    self.filter_applied,
                    &data.files,
                    &self.filtered,
                    self.selected,
                    color,
                ) {
                    self.selected = Some(selected);
                }
            });
        self.status_bar(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(12.0))
            .show(ctx, |ui| {
                if self.data.as_ref().unwrap().files.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("没有可显示的文件");
                    });
                    return;
                }
                let viewport = ui.available_size().max(Vec2::splat(1.0));
                self.layout(viewport, ctx.pixels_per_point());
                let (canvas, response) = ui.allocate_exact_size(viewport, Sense::click());
                self.canvas_rect = Some(canvas);
                let painter = ui.painter_at(canvas);
                let pointer = response.hover_pos();
                let mut hovered = None;
                let data = self.data.as_ref().unwrap();
                if let Some(cache) = &self.map_cache {
                    let map = &cache.map;
                    painter.image(
                        cache.texture.id(),
                        canvas,
                        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    let scale = Vec2::new(
                        canvas.width() / map.bounds.w,
                        canvas.height() / map.bounds.h,
                    );
                    let mut holes = egui::Mesh::default();
                    for hole in &cache.holes {
                        // Solid quads have no antialiased fringe over neighbors.
                        holes.add_colored_rect(
                            Rect::from_min_size(
                                canvas.min + Vec2::new(hole.x * scale.x, hole.y * scale.y),
                                Vec2::new(hole.w * scale.x, hole.h * scale.y),
                            ),
                            BG,
                        );
                    }
                    if !holes.is_empty() {
                        painter.add(holes);
                    }
                    if let Some(p) = pointer {
                        hovered = map.hit_test(
                            (p.x - canvas.min.x) / scale.x,
                            (p.y - canvas.min.y) / scale.y,
                        );
                    }
                    for (id, width) in [(self.selected, 2.0), (hovered, 1.5)] {
                        if let Some(r) = id.and_then(|i| map.rect(i)) {
                            let rect = Rect::from_min_size(
                                canvas.min + Vec2::new(r.x * scale.x, r.y * scale.y),
                                Vec2::new(r.w * scale.x, r.h * scale.y),
                            );
                            if rect.width() > 1.0 && rect.height() > 1.0 {
                                painter.rect_stroke(
                                    rect.shrink(0.5),
                                    0.0,
                                    egui::Stroke::new(width, Color32::WHITE),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    }
                } else {
                    painter.text(
                        canvas.center(),
                        egui::Align2::CENTER_CENTER,
                        "正在生成视图…",
                        FontId::proportional(16.0),
                        MUTED,
                    );
                }
                if let Some(i) = hovered {
                    let f = &data.files[i];
                    if response.clicked() {
                        self.selected = Some(i);
                    }
                    show_file_tooltip(&response, f);
                }
            });
        if let Some(i) = reveal {
            self.reveal(i);
        }
        if let Some(i) = trash {
            self.trash(i);
        }
        if let Some(id) = restore {
            self.restore(&id);
        }
        self.overlays(ctx);
    }
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }
}
impl Drop for App {
    fn drop(&mut self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.filter_generation.fetch_add(1, Ordering::Relaxed);
        self.layout_generation.fetch_add(1, Ordering::Relaxed);
        if let Some(data) = self.data.take() {
            defer_drop(data);
        }
        if let Some(map) = self.map_cache.take() {
            defer_drop(map);
        }
    }
}
fn native_options() -> eframe::Result<eframe::NativeOptions> {
    // eframe otherwise replaces the bundle's Dock icon with its own default.
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/AppIcon.png"))
        .map_err(|error| eframe::Error::AppCreation(Box::new(error)))?;
    Ok(eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_icon(icon)
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([860.0, 560.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    })
}

fn main() -> eframe::Result {
    eframe::run_native(
        "Visualize SSD",
        native_options()?,
        Box::new(move |cc| {
            configure(&cc.egui_ctx);
            let mut app = App::default();
            app.start();
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[cfg(windows)]
    #[test]
    fn confirmed_recycle_keeps_disk_usage_and_reuses_reported_destination() {
        let source = snapshot();
        let file = source.files[1].clone();
        let mut destination = file.clone();
        destination.path = r"C:\$Recycle.Bin\test\$Rfile".into();
        destination.parent = destination.path.parent().unwrap().into();
        let updated = relocate_snapshot(
            &source,
            &file.path,
            file.identity,
            Some(destination.clone()),
        )
        .unwrap();
        assert_eq!(updated.files.len(), source.files.len());
        assert_eq!(
            updated.files.iter().map(|entry| entry.size).sum::<u64>(),
            source.files.iter().map(|entry| entry.size).sum::<u64>()
        );
        assert!(
            updated
                .files
                .iter()
                .any(|entry| entry.path == destination.path)
        );
        assert!(!updated.files.iter().any(|entry| entry.path == file.path));
        assert!(relocate_snapshot(&source, &file.path, file.identity, None).is_none());
        let replacement = scan::FileIdentity {
            device: 123,
            inode: 456,
        };
        assert!(relocate_snapshot(&source, &file.path, replacement, Some(destination)).is_none());
    }

    #[test]
    fn startup_uses_selected_transparent_artwork_as_runtime_icon() {
        let options = native_options().unwrap();
        let icon = options
            .viewport
            .icon
            .expect("custom runtime icon must be set");
        let selected =
            eframe::icon_data::from_png_bytes(include_bytes!("../assets/AppIcon.png")).unwrap();
        assert_eq!(*icon, selected);
        assert_ne!(*icon, egui::IconData::default());
        assert_eq!(icon.width, icon.height);
        assert_eq!(icon.rgba.len(), (icon.width * icon.height * 4) as usize);
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] == 0));
        assert!(icon.rgba.chunks_exact(4).any(|pixel| pixel[3] == 255));
    }
    fn snapshot() -> scan::ScanResult {
        scan::ScanResult {
            root: "/".into(),
            files: vec![
                scan::FileEntry {
                    path: "/a/empty".into(),
                    parent: "/a".into(),
                    size: 0,
                    identity: scan::FileIdentity::default(),
                },
                scan::FileEntry {
                    path: "/a/large".into(),
                    parent: "/a".into(),
                    size: 1_000_000,
                    identity: scan::FileIdentity::default(),
                },
                scan::FileEntry {
                    path: "/b/other".into(),
                    parent: "/b".into(),
                    size: 42,
                    identity: scan::FileIdentity::default(),
                },
            ],
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        }
    }
    fn frame(app: &mut App, ctx: &egui::Context) {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(1180.0, 780.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(input, |ctx| app.ui(ctx));
        let _ = ctx.tessellate(output.shapes, output.pixels_per_point);
    }
    fn settle(app: &mut App, ctx: &egui::Context) {
        app.recoveries_loaded = true;
        let start = std::time::Instant::now();
        loop {
            frame(app, ctx);
            if app.filter_rx.is_none()
                && app.layout_rx.is_none()
                && app.filter_applied == app.filter_generation.load(Ordering::Relaxed)
                && app.map_cache.as_ref().is_some_and(|cache| {
                    cache.generation == app.layout_generation.load(Ordering::Relaxed)
                })
            {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn bytes_are_binary_and_zero_is_exact() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1024), "1.0 KiB");
    }
    #[test]
    fn gate_and_stale_permission_probes_cannot_start_scan() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.launch_scan();
        assert!(app.job.is_none());
        let (old_tx, old_rx) = mpsc::channel();
        app.queue_permission_check(old_rx);
        let (new_tx, new_rx) = mpsc::channel();
        app.queue_permission_check(new_rx);
        assert!(old_tx.send(permission::Access::Granted).is_err());
        drop(new_tx);
        app.poll(&ctx);
        assert_eq!(app.access, permission::Access::Unverified);
        assert!(app.job.is_none());
        assert!(app.pending_scan);
        assert!(!app.permission_result(permission::Access::Denied));
        assert!(app.permission_result(permission::Access::Granted));
        assert!(!app.permission_result(permission::Access::Granted));
    }
    #[test]
    fn workers_preserve_all_files_and_cancel_stale_generations() {
        let data = snapshot();
        let token = AtomicU64::new(1);
        let list = filter_snapshot(&data, "empty", None, &BTreeSet::new(), &token, 1).unwrap();
        assert_eq!(list.indices, vec![0]);
        assert_eq!(list.bytes, 1_000_042);
        let map = layout_snapshot(
            &data,
            LayoutKey::new(Vec2::new(800.0, 600.0), 1.0),
            &token,
            1,
        )
        .unwrap();
        for id in 0..3 {
            assert!(map.rect(id).is_some());
        }
        assert!(filter_snapshot(&data, "", None, &BTreeSet::new(), &token, 0).is_none());
        assert!(
            layout_snapshot(
                &data,
                LayoutKey::new(Vec2::new(800.0, 600.0), 1.0),
                &token,
                0
            )
            .is_none()
        );
    }
    #[test]
    fn completed_scan_shows_all_files_and_search_only_changes_list() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.query = "old".into();
        let (tx, rx) = mpsc::channel();
        app.job = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::default(),
            rx,
        });
        tx.send(Ok(snapshot())).unwrap();
        app.poll(&ctx);
        assert!(app.query.is_empty());
        settle(&mut app, &ctx);
        settle(&mut app, &ctx);
        settle(&mut app, &ctx);
        assert_eq!(app.filtered.len(), 3);
        assert_eq!(app.total_bytes, 1_000_042);
        let applied_generation = app.map_cache.as_ref().unwrap().generation;
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        app.query = "no-match".into();
        app.dirty = true;
        settle(&mut app, &ctx);
        assert!(app.filtered.is_empty());
        assert_eq!(
            app.map_cache.as_ref().unwrap().generation,
            applied_generation
        );
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        for id in 0..3 {
            assert!(app.map_cache.as_ref().unwrap().map.rect(id).is_some());
        }
    }
    #[test]
    fn stale_layout_result_never_replaces_current_snapshot() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        let token = AtomicU64::new(0);
        let map = layout_snapshot(
            &snapshot(),
            LayoutKey::new(Vec2::new(30.0, 30.0), 1.0),
            &token,
            0,
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        app.layout_rx = Some(rx);
        app.layout_generation.store(1, Ordering::Relaxed);
        tx.send(LayoutOutput {
            generation: 0,
            key: LayoutKey::new(Vec2::new(30.0, 30.0), 1.0),
            map,
        })
        .unwrap();
        app.poll(&ctx);
        assert!(app.map_cache.is_none());
    }
    #[test]
    fn blocked_and_stopped_screens_render_without_native_window() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        for access in [permission::Access::Denied, permission::Access::Granted] {
            app.access = access;
            frame(&mut app, &ctx);
        }
    }
    fn apply_test_map(app: &mut App, ctx: &egui::Context, key: LayoutKey, generation: u64) {
        app.layout_key = Some(key);
        app.layout_generation.store(generation, Ordering::Relaxed);
        let map = layout_snapshot(
            app.data.as_ref().unwrap(),
            key,
            &app.layout_generation,
            generation,
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        app.layout_rx = Some(rx);
        tx.send(LayoutOutput {
            generation,
            key,
            map,
        })
        .unwrap();
        app.poll(ctx);
    }
    #[test]
    fn resize_keeps_one_coherent_cache_and_discards_obsolete_completion() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.data = Some(Arc::new(snapshot()));
        let initial = LayoutKey::new(Vec2::new(800.0, 400.0), 1.0);
        apply_test_map(&mut app, &ctx, initial, 1);
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let old_map = layout_snapshot(
            app.data.as_ref().unwrap(),
            initial,
            &app.layout_generation,
            1,
        )
        .unwrap();
        let (tx, rx) = mpsc::channel();
        app.layout_rx = Some(rx);
        app.layout(Vec2::new(400.0, 900.0), 2.0);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        assert_eq!(app.map_cache.as_ref().unwrap().key, initial);
        tx.send(LayoutOutput {
            generation: 1,
            key: initial,
            map: old_map,
        })
        .unwrap();
        app.poll(&ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        assert_ne!(app.layout_key, Some(initial));
        let final_key = LayoutKey::new(Vec2::new(400.0, 900.0), 2.0);
        let generation = app.layout_generation.load(Ordering::Relaxed);
        apply_test_map(&mut app, &ctx, final_key, generation);
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.key, final_key);
        assert_eq!(cache.map.bounds.w, 400.0);
        assert_eq!(cache.map.bounds.h, 900.0);
        for id in 0..3 {
            let r = cache.map.rect(id).unwrap();
            if r.w > 2.0 && r.h > 2.0 {
                assert_eq!(
                    cache.map.hit_test(r.x + r.w * 0.5, r.y + r.h * 0.5),
                    Some(id)
                );
            }
        }
    }
    #[test]
    fn dpi_change_rebuilds_even_when_logical_window_size_is_unchanged() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.data = Some(Arc::new(snapshot()));
        let size = Vec2::new(800.0, 600.0);
        apply_test_map(&mut app, &ctx, LayoutKey::new(size, 1.0), 1);
        let (_tx, rx) = mpsc::channel();
        app.layout_rx = Some(rx);
        app.layout(size, 2.0);
        assert_eq!(app.layout_generation.load(Ordering::Relaxed), 2);
        assert_eq!(app.layout_key.unwrap().physical_size, [1600, 1200]);
        assert_eq!(app.map_cache.as_ref().unwrap().generation, 1);
    }
    #[test]
    fn trash_success_leaves_a_hole_without_rebuilding_the_snapshot_or_texture() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        app.query = "large".into();
        settle(&mut app, &ctx);
        let source = app.data.clone().unwrap();
        let cache = app.map_cache.as_ref().unwrap();
        let texture = cache.texture.id();
        let generation = cache.generation;
        let key = cache.key;
        let neighbor = cache.map.rect(2);
        let removed = cache.map.rect(1).unwrap();
        app.selected = Some(1);
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        tx.send(TrashOutput {
            source: source.clone(),
            id: 1,
            result: Err("无法移到废纸篓。".into()),
        })
        .unwrap();
        app.poll(&ctx);
        assert!(app.hidden.is_empty());
        assert!(app.map_cache.as_ref().unwrap().holes.is_empty());
        assert_eq!(app.selected, Some(1));
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        tx.send(TrashOutput {
            source: source.clone(),
            id: 1,
            result: Ok(()),
        })
        .unwrap();
        app.poll(&ctx);
        app.filter();
        assert!(Arc::ptr_eq(app.data.as_ref().unwrap(), &source));
        assert_eq!(app.data.as_ref().unwrap().files.len(), 3);
        assert_eq!(app.total_bytes, 1_000_042); // Trash still occupies disk space.
        assert_eq!(app.query, "large");
        assert!(app.filtered.is_empty());
        assert_eq!(app.selected, None);
        assert!(app.filter_rx.is_none());
        assert!(app.layout_rx.is_none());
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.texture.id(), texture);
        assert_eq!(cache.generation, generation);
        assert_eq!(cache.key, key);
        assert_eq!(cache.map.rect(1), None);
        assert_eq!(cache.map.rect(2), neighbor);
        assert_eq!(
            cache
                .map
                .hit_test(removed.x + removed.w * 0.5, removed.y + removed.h * 0.5),
            None
        );
        assert_eq!(cache.holes.len(), 1);
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1180.0, 780.0),
                )),
                ..Default::default()
            },
            |ctx| app.ui(ctx),
        );
        assert!(
            !output
                .textures_delta
                .set
                .iter()
                .any(|(id, _)| *id == texture)
        );
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        assert_eq!(app.layout_generation.load(Ordering::Relaxed), generation);
        assert!(app.layout_rx.is_none());
        assert!(app.filter_rx.is_none());
    }
    #[test]
    fn deletions_invalidate_inflight_filters_and_mask_inflight_resizes() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let stale_generation = app.filter_generation.load(Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        app.filter_rx = Some(rx);
        tx.send(FilterOutput {
            generation: stale_generation,
            indices: vec![0, 1, 2],
            bytes: 1_000_042,
        })
        .unwrap();
        app.hide_file(1);
        settle(&mut app, &ctx);
        assert!(!app.filtered.contains(&1));
        app.query = "large".into();
        app.dirty = true;
        settle(&mut app, &ctx);
        assert!(app.filtered.is_empty());
        let key = LayoutKey::new(Vec2::new(700.0, 400.0), 2.0);
        let generation = app.layout_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let map = layout_snapshot(
            app.data.as_ref().unwrap(),
            key,
            &app.layout_generation,
            generation,
        )
        .unwrap();
        app.layout_key = Some(key);
        let (tx, rx) = mpsc::channel();
        app.layout_rx = Some(rx);
        tx.send(LayoutOutput {
            generation,
            key,
            map,
        })
        .unwrap();
        app.poll(&ctx);
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.key, key);
        assert_eq!(cache.map.rect(1), None);
        assert!(!cache.holes.is_empty());
        app.hide_file(1);
        assert_eq!(app.map_cache.as_ref().unwrap().holes.len(), 1);
        // A fresh scan (also used after restore) resets holes and slot identities.
        app.replace_snapshot(snapshot());
        assert!(app.hidden.is_empty());
        settle(&mut app, &ctx);
        assert_eq!(app.filtered.len(), 3);
        assert!(app.map_cache.as_ref().unwrap().map.rect(1).is_some());
    }
    #[test]
    fn trash_completion_from_older_scan_never_removes_new_snapshot_file() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        let source = Arc::new(snapshot());
        let current = Arc::new(snapshot());
        app.data = Some(current.clone());
        let (_scan_tx, scan_rx) = mpsc::channel();
        app.job = Some(Job {
            cancel: Arc::default(),
            progress: Arc::default(),
            rx: scan_rx,
        });
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        tx.send(TrashOutput {
            source,
            id: 0,
            result: Ok(()),
        })
        .unwrap();
        app.poll(&ctx);
        assert!(Arc::ptr_eq(app.data.as_ref().unwrap(), &current));
        assert_eq!(app.data.as_ref().unwrap().files.len(), 3);
        assert!(app.hidden.is_empty());
    }
    #[test]
    fn restore_result_relocates_same_snapshot_without_changing_disk_usage() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        let mut trashed = snapshot();
        trashed.files[1].path = "/Users/test/.Trash/large".into();
        trashed.files[1].parent = "/Users/test/.Trash".into();
        app.data = Some(Arc::new(trashed));
        let source = app.data.clone().unwrap();
        let (tx, rx) = mpsc::channel();
        app.restore_rx = Some(rx);
        tx.send(RestoreOutput {
            source: source.clone(),
            result: Err("原位置存在同名文件，未覆盖。".into()),
        })
        .unwrap();
        app.poll(&ctx);
        assert!(Arc::ptr_eq(app.data.as_ref().unwrap(), &source));
        let (tx, rx) = mpsc::channel();
        app.restore_rx = Some(rx);
        tx.send(RestoreOutput {
            source,
            result: Ok(Some(snapshot())),
        })
        .unwrap();
        app.poll(&ctx);
        assert_eq!(app.data.as_ref().unwrap().files.len(), 3);
        assert_eq!(
            app.data
                .as_ref()
                .unwrap()
                .files
                .iter()
                .map(|f| f.size)
                .sum::<u64>(),
            1_000_042
        );
        assert_eq!(
            app.data.as_ref().unwrap().files[1].path,
            PathBuf::from("/a/large")
        );
        assert!(app.action_notice.as_ref().unwrap().contains("放回原处"));
        assert!(app.selected.is_none());
    }
    #[test]
    fn clicking_wrapped_path_copies_full_unicode_path_without_layout_change() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut data = snapshot();
        data.files[0].path = "/用户/文档/报告.txt".into();
        data.files[0].parent = "/用户/文档".into();
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.recoveries_loaded = true;
        app.data = Some(Arc::new(data));
        settle(&mut app, &ctx);
        app.selected = Some(0);
        frame(&mut app, &ctx);
        frame(&mut app, &ctx);
        let canvas = app.canvas_rect;
        {
            let response = ctx
                .read_response(egui::Id::new("copy-selected-path"))
                .expect("complete path visible");
            let pos = response.rect.center();
            for pressed in [true, false] {
                let input = egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(1180.0, 780.0),
                    )),
                    events: vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: Default::default(),
                        },
                    ],
                    ..Default::default()
                };
                let output = ctx.run(input, |ctx| app.ui(ctx));
                if !pressed {
                    assert!(output.platform_output.commands.iter().any(|command| matches!(command,egui::OutputCommand::CopyText(text) if text=="/用户/文档/报告.txt")));
                }
                assert_eq!(app.canvas_rect, canvas);
            }
        }
    }
    #[test]
    fn errors_take_priority_over_copy_feedback_until_dismissed() {
        let mut app = App::default();
        app.message = Some("文件无法恢复，请检查原目录权限。".into());
        app.action_notice = Some("已复制完整路径。".into());
        app.collect_notices();
        assert_eq!(app.notice.as_ref().unwrap().kind, NoticeKind::Error);
        assert!(app.notice.as_ref().unwrap().text.contains("恢复"));
        app.action_notice = Some("已复制完整路径。".into());
        app.collect_notices();
        assert_eq!(app.notice.as_ref().unwrap().kind, NoticeKind::Error);
        app.notice = None;
        app.action_notice = Some("已复制完整路径。".into());
        app.collect_notices();
        assert_eq!(app.notice.as_ref().unwrap().kind, NoticeKind::Success);
    }
    #[test]
    fn stopped_scan_progress_and_errors_preserve_empty_view_bounds() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.recoveries_loaded = true;
        for _ in 0..3 {
            frame(&mut app, &ctx);
        }
        let rect = app.canvas_rect;
        let (_tx, rx) = mpsc::channel();
        app.job = Some(Job {
            cancel: Arc::default(),
            progress: Arc::default(),
            rx,
        });
        for _ in 0..3 {
            frame(&mut app, &ctx);
            assert_eq!(app.canvas_rect, rect);
        }
        defer_drop(app.job.take());
        app.message = Some("无法读取磁盘，请检查权限。".repeat(30));
        for _ in 0..3 {
            frame(&mut app, &ctx);
            assert_eq!(app.canvas_rect, rect);
        }
    }
    #[test]
    fn notification_lifecycle_and_background_status_never_resize_canvas() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.recoveries_loaded = true;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        settle(&mut app, &ctx);
        settle(&mut app, &ctx);
        let canvas = app.canvas_rect.unwrap();
        let key = app.layout_key;
        let generation = app.layout_generation.load(Ordering::Relaxed);
        let check = |app: &mut App| {
            for _ in 0..3 {
                frame(app, &ctx);
                assert_eq!(app.canvas_rect, Some(canvas));
                assert_eq!(app.layout_key, key);
                assert_eq!(app.layout_generation.load(Ordering::Relaxed), generation);
            }
        };
        app.action_notice = Some("已复制完整路径。".into());
        check(&mut app);
        assert_eq!(app.notice.as_ref().unwrap().kind, NoticeKind::Success);
        app.message = Some("macOS 拒绝移动文件，请检查权限。".repeat(30));
        check(&mut app);
        assert_eq!(app.notice.as_ref().unwrap().kind, NoticeKind::Error);
        app.notice = Some(Notice {
            text: format!("正在移到{}…", platform::TRASH_NAME),
            kind: NoticeKind::Info,
            expires: None,
        });
        check(&mut app);
        app.notice.as_mut().unwrap().expires = Some(Instant::now() - Duration::from_secs(1));
        check(&mut app);
        assert!(app.notice.is_none());
        let (_scan_tx, scan_rx) = mpsc::channel();
        app.job = Some(Job {
            cancel: Arc::default(),
            progress: Arc::new(Mutex::new(scan::ScanProgress {
                files: 3_500_000,
                bytes: 900_000_000_000,
                skipped: 12345,
            })),
            rx: scan_rx,
        });
        check(&mut app);
        defer_drop(app.job.take());
        let (_filter_tx, filter_rx) = mpsc::channel();
        app.filter_rx = Some(filter_rx);
        check(&mut app);
        app.filter_rx = None;
        let (_layout_tx, layout_rx) = mpsc::channel();
        app.layout_rx = Some(layout_rx);
        check(&mut app);
        app.layout_rx = None;
        let mut with_errors = snapshot();
        with_errors.errors = vec!["/System/受保护文件：无访问权限".repeat(12)];
        app.data = Some(Arc::new(with_errors));
        check(&mut app);
        app.show_errors = true;
        check(&mut app);
        app.show_errors = false;
        app.show_recoveries = true;
        check(&mut app);
        app.show_recoveries = false;
    }
    #[test]
    fn wrapped_selected_path_does_not_expand_sidebar_or_canvas() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut data = snapshot();
        data.files[0].path = PathBuf::from(format!(
            "/{}/{}",
            (0..25)
                .map(|i| format!("长目录{i}"))
                .collect::<Vec<_>>()
                .join("/"),
            "long-file-name".repeat(30)
        ));
        data.files[0].parent = data.files[0].path.parent().unwrap().to_path_buf();
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.recoveries_loaded = true;
        app.data = Some(Arc::new(data));
        settle(&mut app, &ctx);
        settle(&mut app, &ctx);
        settle(&mut app, &ctx);
        let rect = app.canvas_rect;
        let key = app.layout_key;
        app.selected = Some(0);
        for _ in 0..4 {
            frame(&mut app, &ctx);
            assert_eq!(app.canvas_rect, rect);
            assert_eq!(app.layout_key, key);
        }
        app.action_notice = Some("已复制完整路径。".into());
        frame(&mut app, &ctx);
        assert_eq!(app.canvas_rect, rect);
    }
    fn text_shape<'a>(
        output: &'a egui::FullOutput,
        text: &str,
    ) -> (&'a egui::epaint::TextShape, Rect) {
        fn find<'a>(shape: &'a egui::Shape, text: &str) -> Option<&'a egui::epaint::TextShape> {
            match shape {
                egui::Shape::Text(shape) if shape.galley.job.text == text => Some(shape),
                egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, text)),
                _ => None,
            }
        }
        output
            .shapes
            .iter()
            .find_map(|clipped| find(&clipped.shape, text).map(|shape| (shape, clipped.clip_rect)))
            .expect("complete text must be painted")
    }

    #[test]
    fn realistic_paths_and_long_file_names_wrap_fully_without_a_scroll_view() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let paths = [
            "/Users/Corwin/Library/Application Support/Google/Chrome/Default/Service Worker/CacheStorage/https_www.example.com_0/8b92f63e36ca904bce67a92cdb6ac9ea/8b3bb59860c2cf7266ab9f7b0e4fd756ad86ef8f6fc59e3640b57849fb103c7a".to_owned(),
            format!("/Users/Corwin/Downloads/{}", "a".repeat(255)),
        ];
        for size in [Vec2::new(1180.0, 780.0), Vec2::new(860.0, 560.0)] {
            for path in &paths {
                let mut data = snapshot();
                data.files[0].path = path.into();
                data.files[0].parent = data.files[0].path.parent().unwrap().into();
                let mut app = App::default();
                app.access = permission::Access::Granted;
                app.recoveries_loaded = true;
                app.data = Some(Arc::new(data));
                app.selected = Some(0);
                let mut last = None;
                for _ in 0..5 {
                    last = Some(ctx.run(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)),
                            ..Default::default()
                        },
                        |ctx| app.ui(ctx),
                    ));
                }
                let (shape, clip) = text_shape(last.as_ref().unwrap(), path);
                assert!(!shape.galley.elided);
                assert!(shape.galley.rows.len() > 1);
                let rect = shape.galley.rect.translate(shape.pos.to_vec2());
                assert!(
                    rect.left() >= clip.left() - 1.0 && rect.right() <= clip.right() + 1.0,
                    "path escaped horizontal bounds: {rect:?} / {clip:?}"
                );
                assert!(
                    rect.bottom() <= clip.bottom() + 1.0,
                    "realistic path should be fully visible: {rect:?} / {clip:?}"
                );
            }
        }
    }

    #[test]
    fn recovery_rows_wrap_complete_paths_without_overlapping_next_record() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let paths = [
            format!(
                "/Users/Corwin/Library/Application Support/ChromeCache/{}",
                "a".repeat(200)
            ),
            "/Users/Corwin/Documents/第二个恢复文件.pdf".to_owned(),
        ];
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.recoveries_loaded = true;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        app.show_recoveries = true;
        app.recovery_records = paths
            .iter()
            .enumerate()
            .map(|(index, path)| file_actions::RecoveryRecord {
                id: format!("fixture-{index}"),
                original_path: path.into(),
                trash_path: None,
                identity: scan::FileIdentity::default(),
                original_links: 1,
            })
            .collect();
        let mut output = None;
        for _ in 0..5 {
            output = Some(ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(1180.0, 780.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.ui(ctx),
            ));
        }
        let output = output.as_ref().unwrap();
        let (first, first_clip) = text_shape(output, &paths[0]);
        let (second, _) = text_shape(output, &paths[1]);
        assert!(!first.galley.elided && !second.galley.elided);
        assert!(first.galley.rows.len() > 1);
        let first_rect = first.galley.rect.translate(first.pos.to_vec2());
        let second_rect = second.galley.rect.translate(second.pos.to_vec2());
        assert!(first_rect.right() <= first_clip.right() + 1.0);
        assert!(
            second_rect.top() >= first_rect.bottom(),
            "wrapped recovery rows overlap"
        );
    }

    #[test]
    fn treemap_tooltip_opens_on_entry_and_updates_while_pointer_moves() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut data = snapshot();
        for file in &mut data.files {
            file.size = 100;
        }
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(data));
        settle(&mut app, &ctx);
        let canvas = app.canvas_rect.unwrap();
        let points: Vec<_> = {
            let map = &app.map_cache.as_ref().unwrap().map;
            (0..2)
                .map(|index| {
                    let rect = map.rect(index).unwrap();
                    canvas.min
                        + Vec2::new(
                            (rect.x + rect.w * 0.5) * canvas.width() / map.bounds.w,
                            (rect.y + rect.h * 0.5) * canvas.height() / map.bounds.h,
                        )
                })
                .collect()
        };
        let start = ctx.input(|input| input.time);
        for (step, point) in points.into_iter().enumerate() {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1180.0, 780.0),
                )),
                time: Some(start + (step + 1) as f64 * 0.01),
                events: vec![egui::Event::PointerMoved(point)],
                ..Default::default()
            };
            let mut output = ctx.run(input.clone(), |ctx| app.ui(ctx));
            if step == 0 {
                // egui measures a newly opened tooltip in an invisible sizing pass,
                // then requests an immediate repaint; no hover dwell is needed.
                assert_eq!(
                    output.viewport_output[&egui::ViewportId::ROOT].repaint_delay,
                    Duration::ZERO
                );
                output = ctx.run(
                    egui::RawInput {
                        time: Some(start + 0.011),
                        events: vec![],
                        ..input
                    },
                    |ctx| app.ui(ctx),
                );
            }
            let path = app.data.as_ref().unwrap().files[step]
                .path
                .to_string_lossy();
            text_shape(&output, &path);
        }
    }

    #[test]
    fn file_tooltips_only_paint_the_hovered_file_and_resize_their_shadow() {
        fn frames(shape: &egui::Shape, result: &mut Vec<(Rect, Rect)>) {
            if let egui::Shape::Vec(shapes) = shape {
                if let [egui::Shape::Rect(shadow), egui::Shape::Rect(body)] = shapes.as_slice()
                    && shadow.blur_width > 0.0
                {
                    result.push((shadow.rect, body.rect));
                } else {
                    for shape in shapes {
                        frames(shape, result);
                    }
                }
            }
        }
        fn paths<'a>(shape: &'a egui::Shape, result: &mut Vec<&'a str>) {
            match shape {
                egui::Shape::Text(shape) if shape.galley.job.text.starts_with("/tooltip/") => {
                    result.push(&shape.galley.job.text);
                }
                egui::Shape::Vec(shapes) => {
                    for shape in shapes {
                        paths(shape, result);
                    }
                }
                _ => {}
            }
        }
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut data = snapshot();
        for (index, file) in data.files.iter_mut().enumerate() {
            file.size = 100;
            let name = if index == 0 {
                "long_movie_name_".repeat(8)
            } else {
                format!("short_{index}")
            };
            file.path = format!("/tooltip/{name}.mov").into();
            file.parent = "/tooltip".into();
        }
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(data));
        settle(&mut app, &ctx);
        let canvas = app.canvas_rect.unwrap();
        let map = &app.map_cache.as_ref().unwrap().map;
        let points: Vec<_> = (0..3)
            .map(|index| {
                let rect = map.rect(index).unwrap();
                canvas.min
                    + Vec2::new(
                        (rect.x + rect.w * 0.5) * canvas.width() / map.bounds.w,
                        (rect.y + rect.h * 0.5) * canvas.height() / map.bounds.h,
                    )
            })
            .collect();
        let start = ctx.input(|input| input.time);
        let mut list_points = Vec::new();
        let mut heights = Vec::new();
        // Warm up the first Area sizing pass, then alternate long/short files,
        // leave the canvas, and move across list rows and back to the canvas.
        for (step, hovered) in [
            Some(0),
            Some(0),
            Some(1),
            Some(0),
            Some(1),
            None,
            Some(0),
            Some(0),
            Some(1),
            Some(1),
            Some(2),
            Some(2),
            None,
            Some(1),
        ]
        .into_iter()
        .enumerate()
        {
            let point = match (step, hovered) {
                (6..=11, Some(index)) => list_points[index],
                (_, Some(index)) => points[index],
                _ => egui::pos2(1.0, 1.0),
            };
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(1180.0, 780.0),
                    )),
                    time: Some(start + (step + 1) as f64 * 0.01),
                    events: vec![egui::Event::PointerMoved(point)],
                    ..Default::default()
                },
                |ctx| app.ui(ctx),
            );
            if list_points.is_empty() {
                list_points = app
                    .data
                    .as_ref()
                    .unwrap()
                    .files
                    .iter()
                    .map(|file| {
                        let name = file.path.file_name().unwrap().to_string_lossy();
                        text_shape(&output, &name).0.pos + Vec2::splat(3.0)
                    })
                    .collect();
            }
            let mut frame_rects = Vec::new();
            let mut tooltip_paths = Vec::new();
            for shape in &output.shapes {
                frames(&shape.shape, &mut frame_rects);
                paths(&shape.shape, &mut tooltip_paths);
            }
            assert!(
                frame_rects.len() <= 1,
                "step {step}: multiple file tooltips painted"
            );
            if (1..=4).contains(&step) || [7, 9, 11, 13].contains(&step) {
                assert_eq!(frame_rects.len(), 1, "step {step}: current tooltip missing");
                assert_eq!(tooltip_paths.len(), 1);
                if step <= 4 {
                    heights.push(frame_rects[0].1.height());
                }
            }
            if let Some(index) = hovered {
                for path in tooltip_paths {
                    assert_eq!(
                        path,
                        app.data.as_ref().unwrap().files[index]
                            .path
                            .to_str()
                            .unwrap(),
                        "step {step}: non-hovered file tooltip painted"
                    );
                }
            } else {
                assert!(
                    frame_rects.is_empty(),
                    "unhovered files must not create tooltips"
                );
            }
            for (shadow, body) in frame_rects {
                let token = ctx.style().visuals.popup_shadow;
                let offset = Vec2::new(token.offset[0] as f32, token.offset[1] as f32);
                assert_eq!(shadow, body.translate(offset).expand(token.spread as f32));
            }
        }
        assert!(heights[0] > heights[1]);
        assert_eq!(heights[0], heights[2]);
        assert_eq!(heights[1], heights[3]);
    }

    #[test]
    fn hover_near_right_edge_keeps_a_readable_full_text_tooltip_on_screen() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut file = snapshot().files.remove(0);
        file.path = format!("/Users/Corwin/Downloads/{}.pdf", "长文件名".repeat(30)).into();
        let mut last = None;
        let mut tooltip_rect = None;
        for pass in 0..6 {
            last = Some(ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(860.0, 560.0),
                    )),
                    time: Some(pass as f64),
                    events: vec![egui::Event::PointerMoved(egui::pos2(844.0, 200.0))],
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let response = ui.allocate_rect(ui.max_rect(), Sense::hover());
                        tooltip_rect = show_file_tooltip(&response, &file)
                            .map(|response| response.response.rect);
                    });
                },
            ));
        }
        let rect = tooltip_rect.expect("hover must open the tooltip");
        assert!(
            rect.width() >= 350.0 && rect.width() <= 460.0,
            "tooltip width: {rect:?}"
        );
        assert!(
            rect.left() >= 0.0 && rect.right() <= 860.0,
            "tooltip escaped screen: {rect:?}"
        );
        let path = file.path.to_string_lossy();
        let (shape, _) = text_shape(last.as_ref().unwrap(), &path);
        assert!(!shape.galley.elided);
        assert!(shape.galley.rows.len() > 1);
    }

    #[test]
    #[ignore = "release performance benchmark"]
    fn full_app_3_5m_steady_frame() {
        let n = 3_500_000;
        let files: Vec<_> = (0..n)
            .into_par_iter()
            .map(|i| scan::FileEntry {
                path: format!("/group{}/file{i}", i / 1000).into(),
                parent: format!("/group{}", i / 1000).into(),
                size: (i % 10000 + 1) as u64,
                identity: scan::FileIdentity::default(),
            })
            .collect();
        let data = Arc::new(scan::ScanResult {
            root: "/".into(),
            files,
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(data);
        let begin = std::time::Instant::now();
        settle(&mut app, &ctx);
        let build = begin.elapsed();
        settle(&mut app, &ctx);
        settle(&mut app, &ctx);
        app.selected = Some(n / 2);
        for _ in 0..5 {
            frame(&mut app, &ctx);
        }
        let mut times = Vec::new();
        for _ in 0..60 {
            let now = std::time::Instant::now();
            let position = egui::pos2(250.0 + (times.len() % 20) as f32, 350.0);
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1180.0, 780.0),
                )),
                events: vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed: times.len() % 2 == 0,
                        modifiers: egui::Modifiers::default(),
                    },
                ],
                ..Default::default()
            };
            let output = ctx.run(input, |ctx| app.ui(ctx));
            let _ = ctx.tessellate(output.shapes, output.pixels_per_point);
            times.push(now.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        let now = std::time::Instant::now();
        let resized = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(1600.0, 1000.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(resized, |ctx| app.ui(ctx));
        let _ = ctx.tessellate(output.shapes, output.pixels_per_point);
        let resize_request = now.elapsed();
        let now = std::time::Instant::now();
        app.query = "file123".into();
        app.dirty = true;
        frame(&mut app, &ctx);
        let search_request = now.elapsed();
        app.query = "file999".into();
        app.dirty = true;
        frame(&mut app, &ctx);
        assert!(resize_request.as_secs_f64() * 1000.0 < 16.7);
        assert!(search_request.as_secs_f64() * 1000.0 < 16.7);
        eprintln!("request CPU resize={resize_request:?}, search={search_request:?}");
        eprintln!(
            "3.5m build={build:?}, steady p50={:.3}ms p95={:.3}ms max={:.3}ms rayon_threads={}",
            times[30],
            times[57],
            times[59],
            rayon::current_num_threads()
        );
        assert!(times[57] < 16.7, "steady frame p95 exceeds budget");
    }
}
