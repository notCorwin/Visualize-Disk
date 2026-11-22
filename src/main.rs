#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod directory_map;
mod file_actions;
mod permission;
mod platform;
mod render;
mod scan;
mod treemap;

use eframe::egui::{self, Color32, FontId, Rect, RichText, Sense, Vec2};
use rayon::prelude::*;
use std::{
    collections::{BTreeSet, HashMap},
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
const PALETTE: [Color32; 12] = {
    const PINE: Color32 = Color32::from_rgb(0x53, 0x7c, 0x6e);
    const BLUE_GRAY: Color32 = Color32::from_rgb(0x66, 0x81, 0x8d);
    const BRASS: Color32 = Color32::from_rgb(0xa4, 0x8b, 0x62);
    const TERRACOTTA: Color32 = Color32::from_rgb(0xb4, 0x7c, 0x66);
    const MOSS: Color32 = Color32::from_rgb(0x7c, 0x8b, 0x70);
    const SLATE: Color32 = Color32::from_rgb(0x6c, 0x74, 0x80);

    // Repeat the cooler shades so the warm accents occupy fewer hash slots.
    [
        PINE, BLUE_GRAY, SLATE, BRASS, MOSS, TERRACOTTA, BLUE_GRAY, SLATE, PINE, PINE, MOSS,
        BLUE_GRAY,
    ]
};

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

fn show_file_tooltip(
    response: &egui::Response,
    file: &scan::FileEntry,
) -> Option<egui::InnerResponse<()>> {
    show_entry_tooltip(response, file, false)
}

fn show_entry_tooltip(
    response: &egui::Response,
    file: &scan::FileEntry,
    folder: bool,
) -> Option<egui::InnerResponse<()>> {
    if !response.enabled() || tooltip_pointer(response).is_none() {
        return None;
    }
    let available = (response.ctx.content_rect().width() - 32.0).max(1.0);
    let natural = response.ctx.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(
                file.path.to_string_lossy().into_owned(),
                FontId::proportional(14.0),
                MUTED,
            )
            .size()
            .x
    });
    let width = natural.max(220.0).min(360.0).min(available);
    // Skip the standard tooltip delay so moving across files updates immediately.
    egui::Tooltip::for_widget(response)
        .at_pointer()
        .width(width)
        .show(|ui| file_tooltip(ui, file, width, folder))
}

fn tooltip_pointer(response: &egui::Response) -> Option<egui::Pos2> {
    response.hover_pos().or_else(|| {
        egui::Tooltip::was_tooltip_open_last_frame(&response.ctx, response.id)
            .then(|| response.ctx.pointer_hover_pos())
            .flatten()
            .filter(|pos| response.rect.contains(*pos))
    })
}

// Prefer breaks after directory separators while allowing the text layout to
// break an individual component that is wider than the available space.
fn wrap_path(ctx: &egui::Context, path: &str, width: f32) -> String {
    let mut result = String::with_capacity(path.len());
    let mut line = String::new();
    for part in path.split_inclusive(['/', '\\']) {
        let mut candidate = line.clone();
        candidate.push_str(part);
        let candidate_width = ctx.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(candidate, FontId::proportional(14.0), MUTED)
                .size()
                .x
        });
        if !line.is_empty() && candidate_width > width {
            result.push_str(&line);
            result.push('\n');
            line.clear();
        }
        line.push_str(part);
    }
    result.push_str(&line);
    result
}

fn file_tooltip(ui: &mut egui::Ui, file: &scan::FileEntry, width: f32, folder: bool) {
    ui.set_width(width);
    let path = wrap_path(ui.ctx(), &file.path.to_string_lossy(), width);
    let name = file.path.file_name().unwrap_or_default().to_string_lossy();
    let content_height = ui.ctx().fonts_mut(|fonts| {
        let layout = |text: String, size| {
            let mut job =
                egui::text::LayoutJob::simple(text, FontId::proportional(size), TEXT, width);
            job.wrap.break_anywhere = true;
            job
        };
        fonts.layout_job(layout(name.into_owned(), 15.0)).size().y
            + fonts
                .layout_job(layout(tooltip_size(file.size, folder), 14.0))
                .size()
                .y
            + fonts.layout_job(layout(path, 14.0)).size().y
    }) + 2.0 * ui.spacing().item_spacing.y;
    let max_height = (ui.ctx().content_rect().height() * 0.5 - 50.0).max(1.0);
    if content_height <= max_height {
        file_tooltip_content(ui, file, folder);
        return;
    }
    let scroll_id = egui::Id::new(("file-tooltip-scroll", &file.path));
    let previous_offset = ui
        .ctx()
        .data(|data| data.get_temp::<f32>(scroll_id))
        .unwrap_or(0.0);
    // The pointer remains over the file tile, so forward its wheel input to the
    // tooltip instead of requiring the user to move into the transient popup.
    let wheel = ui.ctx().input(|input| input.raw_scroll_delta.y);
    let content = egui::ScrollArea::vertical()
        .id_salt(scroll_id)
        .max_height(max_height)
        .auto_shrink([false, true])
        .vertical_scroll_offset((previous_offset - wheel).max(0.0))
        .show(ui, |ui| file_tooltip_content(ui, file, folder));
    ui.ctx()
        .data_mut(|data| data.insert_temp(scroll_id, content.state.offset.y));
    if content.content_size.y > content.inner_rect.height() {
        ui.label(RichText::new("滚动查看完整路径").color(MUTED).small());
    }
}

fn tooltip_size(size: u64, folder: bool) -> String {
    if folder {
        format!("直接文件 {}", bytes(size))
    } else {
        bytes(size)
    }
}

fn file_tooltip_content(ui: &mut egui::Ui, file: &scan::FileEntry, folder: bool) {
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
    ui.label(tooltip_size(file.size, folder));
    let path = wrap_path(ui.ctx(), &file.path.to_string_lossy(), ui.available_width());
    wrapped_text(ui, path, 14.0, MUTED);
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

struct TotalsOutput {
    generation: u64,
    bytes: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ViewMode {
    #[default]
    Files,
    Folders,
}

struct FolderOutput {
    generation: u64,
    data: scan::ScanResult,
}

// Each directory tile represents only files directly inside that directory.
// Child directories receive their own tiles, so every scanned file is counted once.
fn folder_snapshot(
    data: &scan::ScanResult,
    hidden: &BTreeSet<usize>,
    token: &AtomicU64,
    generation: u64,
) -> Option<scan::ScanResult> {
    let mut groups: HashMap<&std::path::Path, u64> = HashMap::new();
    for (id, file) in data.files.iter().enumerate() {
        if id % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
            return None;
        }
        if !hidden.contains(&id) {
            let size = groups.entry(file.parent.as_ref()).or_default();
            *size = size.saturating_add(file.size);
        }
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let files = groups
        .into_iter()
        .map(|(path, size)| scan::FileEntry {
            path: path.to_path_buf(),
            parent: Arc::from(path.parent().unwrap_or(path)),
            size,
            identity: scan::FileIdentity::default(),
            can_trash: false,
        })
        .collect();
    (token.load(Ordering::Relaxed) == generation).then(|| scan::ScanResult {
        root: data.root.clone(),
        files,
        skipped: data.skipped,
        errors: Vec::new(),
        elapsed: data.elapsed,
    })
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

fn totals_snapshot(
    data: &scan::ScanResult,
    token: &AtomicU64,
    generation: u64,
) -> Option<TotalsOutput> {
    let bytes = data
        .files
        .par_iter()
        .map(|file| file.size)
        .reduce(|| 0, u64::saturating_add);
    (token.load(Ordering::Relaxed) == generation).then_some(TotalsOutput { generation, bytes })
}

fn layout_snapshot(
    data: &scan::ScanResult,
    key: LayoutKey,
    token: &AtomicU64,
    generation: u64,
) -> Option<render::RenderMap> {
    let grouped: Result<HashMap<&std::path::Path, Vec<(usize, u64)>>, ()> = data
        .files
        .par_iter()
        .enumerate()
        .try_fold(HashMap::new, |mut groups, (i, file)| {
            if i % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
                return Err(());
            }
            groups
                .entry(file.parent.as_ref())
                .or_insert_with(Vec::new)
                .push((i, file.size.max(1)));
            Ok(groups)
        })
        .try_reduce(HashMap::new, |mut a, b| {
            for (index, (path, mut files)) in b.into_iter().enumerate() {
                if index % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
                    return Err(());
                }
                a.entry(path).or_insert_with(Vec::new).append(&mut files);
            }
            Ok(a)
        });
    let bounds = treemap::Rect {
        x: 0.0,
        y: 0.0,
        w: key.logical_size.x,
        h: key.logical_size.y,
    };
    let layout = directory_map::layout(
        grouped.ok()?,
        &data.root,
        bounds,
        render::logical_pixel_size(bounds, key.physical_size),
        token,
        generation,
    )?;
    render::RenderMap::build_with_platforms(
        layout.entries,
        layout.platforms,
        bounds,
        key.physical_size,
        token,
        generation,
    )
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
    mode: ViewMode,
    folder_data: Option<Arc<scan::ScanResult>>,
    folder_rx: Option<mpsc::Receiver<FolderOutput>>,
    folder_generation: Arc<AtomicU64>,
    hidden: Arc<BTreeSet<usize>>,
    job: Option<Job>,
    reveal_rx: Option<mpsc::Receiver<Result<(), String>>>,
    trash_rx: Option<mpsc::Receiver<TrashOutput>>,
    selected: Option<usize>,
    message: Option<String>,
    action_notice: Option<String>,
    notice: Option<Notice>,
    show_errors: bool,
    canvas_rect: Option<Rect>,
    total_bytes: u64,
    totals_rx: Option<mpsc::Receiver<TotalsOutput>>,
    totals_generation: Arc<AtomicU64>,
    totals_ready: bool,
    layout_rx: Option<mpsc::Receiver<LayoutOutput>>,
    layout_generation: Arc<AtomicU64>,
    layout_key: Option<LayoutKey>,
    map_cache: Option<MapCache>,
    inactive_map_cache: Option<MapCache>,
}
impl Default for App {
    fn default() -> Self {
        Self {
            access: permission::Access::Unverified,
            permission_rx: None,
            pending_scan: false,
            data: None,
            mode: ViewMode::Files,
            folder_data: None,
            folder_rx: None,
            folder_generation: Arc::new(AtomicU64::new(0)),
            hidden: Arc::default(),
            job: None,
            reveal_rx: None,
            trash_rx: None,
            selected: None,
            message: None,
            action_notice: None,
            notice: None,
            show_errors: false,
            canvas_rect: None,
            total_bytes: 0,
            totals_rx: None,
            totals_generation: Arc::new(AtomicU64::new(0)),
            totals_ready: false,
            layout_rx: None,
            layout_generation: Arc::new(AtomicU64::new(0)),
            layout_key: None,
            map_cache: None,
            inactive_map_cache: None,
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
        self.invalidate_folders();
        defer_drop(std::mem::take(&mut self.hidden));
        if let Some(old) = self.data.replace(Arc::new(data)) {
            defer_drop(old);
        }
        self.totals_generation.fetch_add(1, Ordering::Relaxed);
        self.layout_generation.fetch_add(1, Ordering::Relaxed);
        if let Some(old) = self.totals_rx.take() {
            defer_drop(old);
        }
        if let Some(old) = self.layout_rx.take() {
            defer_drop(old);
        }
        if let Some(old) = self.map_cache.take() {
            defer_drop(old);
        }
        if let Some(old) = self.inactive_map_cache.take() {
            defer_drop(old);
        }
        self.total_bytes = 0;
        self.totals_ready = false;
        self.selected = None;
        self.layout_key = None;
    }
    fn trash(&mut self, id: usize) {
        if self.mode != ViewMode::Files
            || self.access != permission::Access::Granted
            || self.trash_rx.is_some()
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
        if !file.can_trash || !file_actions::can_move_to_trash(&file.path) {
            return;
        }
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
    }
    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.folder_rx {
            match rx.try_recv() {
                Ok(output) => {
                    self.folder_rx = None;
                    if output.generation == self.folder_generation.load(Ordering::Relaxed) {
                        self.folder_data = Some(Arc::new(output.data));
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.folder_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
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
        if let Some(rx) = &self.totals_rx {
            match rx.try_recv() {
                Ok(output) => {
                    self.totals_rx = None;
                    if output.generation == self.totals_generation.load(Ordering::Relaxed) {
                        self.total_bytes = output.bytes;
                        self.totals_ready = true;
                    } else {
                        defer_drop(output);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.totals_rx = None,
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
                            if self.mode == ViewMode::Files {
                                for &id in self.hidden.iter() {
                                    cache.hide(id);
                                }
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
    }
    fn hide_file(&mut self, id: usize) {
        if self.data.as_ref().is_none_or(|data| id >= data.files.len())
            || !Arc::make_mut(&mut self.hidden).insert(id)
        {
            return;
        }
        if self.mode == ViewMode::Files && self.selected == Some(id) {
            self.selected = None;
        }
        self.invalidate_folders();
        if self.mode == ViewMode::Files {
            if let Some(cache) = &mut self.map_cache {
                cache.hide(id);
            }
        } else if let Some(cache) = &mut self.inactive_map_cache {
            cache.hide(id);
        }
    }
    fn invalidate_folders(&mut self) {
        self.folder_generation.fetch_add(1, Ordering::Relaxed);
        if let Some(rx) = self.folder_rx.take() {
            defer_drop(rx);
        }
        if let Some(data) = self.folder_data.take() {
            defer_drop(data);
        }
        if self.mode == ViewMode::Folders {
            self.reset_layout();
        } else if let Some(cache) = self.inactive_map_cache.take() {
            defer_drop(cache);
        }
    }
    fn reset_layout(&mut self) {
        self.layout_generation.fetch_add(1, Ordering::Relaxed);
        self.layout_key = None;
        if let Some(rx) = self.layout_rx.take() {
            defer_drop(rx);
        }
        if let Some(cache) = self.map_cache.take() {
            defer_drop(cache);
        }
        self.selected = None;
    }
    fn set_mode(&mut self, mode: ViewMode) {
        if self.mode != mode {
            self.mode = mode;
            self.layout_generation.fetch_add(1, Ordering::Relaxed);
            if let Some(rx) = self.layout_rx.take() {
                defer_drop(rx);
            }
            self.selected = None;
            std::mem::swap(&mut self.map_cache, &mut self.inactive_map_cache);
            let generation = self.layout_generation.load(Ordering::Relaxed);
            self.layout_key = self.map_cache.as_mut().map(|cache| {
                cache.generation = generation;
                cache.key
            });
        }
    }
    fn view_data(&self) -> Option<&Arc<scan::ScanResult>> {
        match self.mode {
            ViewMode::Files => self.data.as_ref(),
            ViewMode::Folders => self.folder_data.as_ref(),
        }
    }
    fn prepare_folders(&mut self, ctx: &egui::Context) {
        if self.mode != ViewMode::Folders || self.folder_data.is_some() || self.folder_rx.is_some()
        {
            return;
        }
        let Some(data) = self.data.clone() else {
            return;
        };
        let hidden = self.hidden.clone();
        let token = self.folder_generation.clone();
        let generation = token.load(Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.folder_rx = Some(rx);
        let ctx = ctx.clone();
        rayon::spawn(move || {
            if let Some(data) = folder_snapshot(&data, &hidden, &token, generation) {
                let _ = tx.send(FolderOutput { generation, data });
            }
            ctx.request_repaint();
        });
    }
    fn totals(&mut self, ctx: &egui::Context) {
        let generation = self.totals_generation.load(Ordering::Relaxed);
        if self.totals_rx.is_some() || self.totals_ready {
            return;
        }
        let Some(data) = self.data.clone() else {
            return;
        };
        let token = self.totals_generation.clone();
        let (tx, rx) = mpsc::channel();
        self.totals_rx = Some(rx);
        let ctx = ctx.clone();
        rayon::spawn(move || {
            if let Some(output) = totals_snapshot(&data, &token, generation) {
                let _ = tx.send(output);
            }
            drop(tx);
            ctx.request_repaint();
        });
    }
    fn layout(&mut self, size: Vec2, pixels_per_point: f32, ctx: &egui::Context) {
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
        let Some(data) = self.view_data().cloned() else {
            return;
        };
        let token = self.layout_generation.clone();
        let (tx, rx) = mpsc::channel();
        self.layout_rx = Some(rx);
        let ctx = ctx.clone();
        rayon::spawn(move || {
            if let Some(map) = layout_snapshot(&data, key, &token, generation) {
                let _ = tx.send(LayoutOutput {
                    generation,
                    key,
                    map,
                });
            }
            drop(tx);
            ctx.request_repaint();
        });
    }
    fn reveal(&mut self, i: usize) {
        if self.reveal_rx.is_some() {
            return;
        }
        let Some(data) = self.view_data() else {
            return;
        };
        let Some(file) = data.files.get(i) else {
            return;
        };
        let path = file.path.clone();
        let (tx, rx) = mpsc::channel();
        self.reveal_rx = Some(rx);
        std::thread::spawn(move || {
            let result = platform::reveal(&path);
            let _ = tx.send(result);
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
        self.ui_contents(ctx);
        if !ctx.wants_keyboard_input()
            && ctx.input_mut(|input| {
                input.consume_key(egui::Modifiers::COMMAND, egui::Key::Backspace)
            })
            && let Some(id) = self.selected
        {
            self.trash(id);
        }
        if self.job.is_some()
            || self.reveal_rx.is_some()
            || self.trash_rx.is_some()
            || self.permission_rx.is_some()
            || self.totals_rx.is_some()
            || self.layout_rx.is_some()
            || self.folder_rx.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
    fn ui_contents(&mut self, ctx: &egui::Context) {
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
        let mut reveal = None;
        let mut trash = None;
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
                        .selectable_label(self.mode == ViewMode::Files, "文件")
                        .clicked()
                    {
                        self.set_mode(ViewMode::Files);
                    }
                    if ui
                        .selectable_label(self.mode == ViewMode::Folders, "文件夹")
                        .on_hover_text("按文件夹中的直接文件汇总")
                        .clicked()
                    {
                        self.set_mode(ViewMode::Folders);
                    }
                    let selected = self.selected.and_then(|i| {
                        self.view_data()
                            .and_then(|data| data.files.get(i))
                            .map(|f| (i, f))
                    });
                    if ui
                        .add_enabled(
                            selected.is_some() && self.reveal_rx.is_none(),
                            egui::Button::new(platform::REVEAL_LABEL),
                        )
                        .clicked()
                    {
                        reveal = selected.map(|(i, _)| i);
                    }
                    if self.mode == ViewMode::Files
                        && selected.is_none_or(|(_, f)| {
                            f.can_trash && file_actions::can_move_to_trash(&f.path)
                        })
                        && ui
                            .add_enabled(
                                selected.is_some() && self.trash_rx.is_none(),
                                egui::Button::new(format!("移到{}", platform::TRASH_NAME)),
                            )
                            .clicked()
                    {
                        trash = selected.map(|(i, _)| i);
                    }
                    if let Some(data) = &self.data {
                        let total = if self.totals_ready {
                            bytes(self.total_bytes)
                        } else {
                            "统计中…".into()
                        };
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(total).strong().color(ACCENT))
                                .on_hover_text("文件逻辑大小");
                            ui.label(format!("{} 个文件", data.files.len()));
                        });
                    }
                });
            });
        self.totals(ctx);
        self.prepare_folders(ctx);
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
        self.status_bar(ctx);
        if self.view_data().is_none() {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BG))
                .show(ctx, |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.label("正在生成文件夹视图…");
                    });
                });
            self.overlays(ctx);
            return;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(12.0))
            .show(ctx, |ui| {
                if self.view_data().unwrap().files.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("没有可显示的文件");
                    });
                    return;
                }
                let viewport = ui.available_size().max(Vec2::splat(1.0));
                self.layout(viewport, ctx.pixels_per_point(), ctx);
                let (canvas, response) = ui.allocate_exact_size(viewport, Sense::click());
                self.canvas_rect = Some(canvas);
                let painter = ui.painter_at(canvas);
                let pointer = tooltip_pointer(&response);
                let mut hovered = None;
                let data = self.view_data().unwrap().clone();
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
                    if self.mode == ViewMode::Folders {
                        show_entry_tooltip(&response, f, true);
                    } else {
                        show_file_tooltip(&response, f);
                    }
                }
            });
        if let Some(i) = reveal {
            self.reveal(i);
        }
        if let Some(i) = trash {
            self.trash(i);
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
        self.totals_generation.fetch_add(1, Ordering::Relaxed);
        self.layout_generation.fetch_add(1, Ordering::Relaxed);
        self.folder_generation.fetch_add(1, Ordering::Relaxed);
        if let Some(data) = self.data.take() {
            defer_drop(data);
        }
        if let Some(map) = self.map_cache.take() {
            defer_drop(map);
        }
        if let Some(map) = self.inactive_map_cache.take() {
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
            .with_min_inner_size([860.0, 560.0])
            .with_maximized(true),
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
    use std::path::{Path, PathBuf};

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
                    can_trash: true,
                    path: "/a/empty".into(),
                    parent: Path::new("/a").into(),
                    size: 0,
                    identity: scan::FileIdentity::default(),
                },
                scan::FileEntry {
                    can_trash: true,
                    path: "/a/large".into(),
                    parent: Path::new("/a").into(),
                    size: 1_000_000,
                    identity: scan::FileIdentity::default(),
                },
                scan::FileEntry {
                    can_trash: true,
                    path: "/b/other".into(),
                    parent: Path::new("/b").into(),
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
        let start = std::time::Instant::now();
        loop {
            frame(app, ctx);
            if app.totals_rx.is_none()
                && app.layout_rx.is_none()
                && app.totals_ready
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
    fn command_backspace_respects_protection_and_keyboard_focus() {
        for (protected, focused, expected) in [
            (false, false, true),
            (true, false, false),
            (false, true, false),
        ] {
            let ctx = egui::Context::default();
            configure(&ctx);
            let mut app = App::default();
            app.access = permission::Access::Granted;
            let mut data = snapshot();
            data.files[0].can_trash = !protected;
            app.data = Some(Arc::new(data));
            app.selected = Some(0);
            let modifiers = egui::Modifiers::COMMAND;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1180.0, 780.0),
                )),
                modifiers,
                events: vec![egui::Event::Key {
                    key: egui::Key::Backspace,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
                ..Default::default()
            };
            let mut text = String::new();
            let _ = ctx.run(input, |ctx| {
                if focused {
                    egui::Area::new(egui::Id::new("test-input")).show(ctx, |ui| {
                        ui.text_edit_singleline(&mut text).request_focus();
                    });
                }
                app.ui(ctx);
            });
            assert_eq!(
                app.trash_rx.is_some(),
                expected,
                "protected={protected}, focused={focused}"
            );
        }
    }

    #[test]
    fn folder_mode_groups_direct_files_once_and_excludes_hidden() {
        let mut data = snapshot();
        data.files.push(scan::FileEntry {
            path: "/a/nested/file".into(),
            parent: Path::new("/a/nested").into(),
            size: 7,
            identity: scan::FileIdentity::default(),
            can_trash: true,
        });
        let token = AtomicU64::new(1);
        let folders = folder_snapshot(&data, &BTreeSet::from([1]), &token, 1).unwrap();
        let actual: Vec<_> = folders
            .files
            .iter()
            .map(|f| (f.path.as_path(), f.size))
            .collect();
        assert_eq!(
            actual,
            vec![
                (Path::new("/a"), 0),
                (Path::new("/a/nested"), 7),
                (Path::new("/b"), 42)
            ]
        );
        assert!(folders.files.iter().all(|f| !f.can_trash));
        assert!(folder_snapshot(&data, &BTreeSet::new(), &token, 0).is_none());
    }
    #[test]
    fn mode_switch_isolates_selection_and_rebuilds_after_deletion() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let file_texture = app.map_cache.as_ref().unwrap().texture.id();
        app.selected = Some(1);
        app.hide_file(1);
        app.set_mode(ViewMode::Folders);
        assert_eq!(app.selected, None);
        settle(&mut app, &ctx);
        let folder_texture = app.map_cache.as_ref().unwrap().texture.id();
        assert_eq!(app.folder_data.as_ref().unwrap().files[0].size, 0);
        app.selected = Some(0);
        app.trash(0);
        assert!(app.trash_rx.is_none());
        app.set_mode(ViewMode::Files);
        assert_eq!(app.selected, None);
        settle(&mut app, &ctx);
        assert!(app.hidden.contains(&1));
        assert!(!app.map_cache.as_ref().unwrap().holes.is_empty());
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), file_texture);
        app.set_mode(ViewMode::Folders);
        settle(&mut app, &ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), folder_texture);
        // A file deletion can complete after switching to folder mode.
        app.hide_file(2);
        settle(&mut app, &ctx);
        assert_eq!(app.folder_data.as_ref().unwrap().files.len(), 1);
        assert_ne!(app.map_cache.as_ref().unwrap().texture.id(), folder_texture);
        app.set_mode(ViewMode::Files);
        settle(&mut app, &ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), file_texture);
        assert!(app.hidden.contains(&2));
    }
    #[test]
    fn folder_mode_delete_shortcut_does_not_delete() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        app.set_mode(ViewMode::Folders);
        settle(&mut app, &ctx);
        app.selected = Some(0);
        let modifiers = egui::Modifiers::COMMAND;
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(1180.0, 780.0),
            )),
            modifiers,
            events: vec![egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }],
            ..Default::default()
        };
        let _ = ctx.run(input, |ctx| app.ui(ctx));
        assert!(app.trash_rx.is_none());
        assert_eq!(app.selected, Some(0));
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
        let list = totals_snapshot(&data, &token, 1).unwrap();
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
        assert!(totals_snapshot(&data, &token, 0).is_none());
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
    fn idle_context_with_wake_receiver() -> (egui::Context, mpsc::Receiver<()>) {
        let ctx = egui::Context::default();
        for _ in 0..5 {
            drop(ctx.run(egui::RawInput::default(), |_| {}));
        }
        let (tx, rx) = mpsc::channel();
        ctx.set_request_repaint_callback(move |info| {
            if info.delay.is_zero() {
                let _ = tx.send(());
            }
        });
        (ctx, rx)
    }
    #[test]
    fn completed_scan_schedules_jobs_started_after_poll() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.access = permission::Access::Granted;
        for _ in 0..5 {
            frame(&mut app, &ctx);
        }
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(snapshot())).unwrap();
        app.job = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::default(),
            rx,
        });
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
        assert!(app.job.is_none());
        assert!(app.totals_rx.is_some());
        assert!(app.layout_rx.is_some());
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT].repaint_delay
                <= Duration::from_millis(100),
            "newly queued jobs must schedule their next frame"
        );
    }
    #[test]
    fn completed_workers_wake_idle_ui_without_input_or_polling_frames() {
        for layout in [false, true] {
            let (ctx, wake) = idle_context_with_wake_receiver();
            let mut app = App::default();
            app.data = Some(Arc::new(snapshot()));
            if layout {
                app.layout(Vec2::new(800.0, 600.0), 1.0, &ctx);
            } else {
                app.totals(&ctx);
            }
            // No UI frames run while the worker computes. Its completion must
            // wake the host before the result can be consumed by the next frame.
            wake.recv_timeout(Duration::from_secs(5))
                .expect("completed worker must wake the idle event loop");
            app.poll(&ctx);
            if layout {
                assert!(app.layout_rx.is_none());
                assert!(app.map_cache.is_some());
            } else {
                assert!(app.totals_rx.is_none());
                assert!(app.totals_ready);
            }
        }
    }
    #[test]
    fn cancelled_workers_close_their_channels_before_waking_idle_ui() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        for layout in [false, true] {
            let (ctx, wake) = idle_context_with_wake_receiver();
            let mut app = App::default();
            app.data = Some(Arc::new(snapshot()));
            pool.install(|| {
                if layout {
                    app.layout(Vec2::new(800.0, 600.0), 1.0, &ctx);
                    app.layout_generation.fetch_add(1, Ordering::Relaxed);
                } else {
                    app.totals(&ctx);
                    app.totals_generation.fetch_add(1, Ordering::Relaxed);
                }
            });
            wake.recv_timeout(Duration::from_secs(5))
                .expect("cancelled worker must wake the idle event loop");
            app.poll(&ctx);
            assert!(app.totals_rx.is_none());
            assert!(app.layout_rx.is_none());
            assert!(!app.totals_ready);
            assert!(app.map_cache.is_none());
        }
    }
    #[test]
    fn completed_scan_shows_all_files_and_totals() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.replace_snapshot(snapshot());
        settle(&mut app, &ctx);
        assert_eq!(app.total_bytes, 1_000_042);
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
        app.layout(Vec2::new(400.0, 900.0), 2.0, &ctx);
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
        app.layout(size, 2.0, &ctx);
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
        app.totals(&ctx);
        assert!(Arc::ptr_eq(app.data.as_ref().unwrap(), &source));
        assert_eq!(app.data.as_ref().unwrap().files.len(), 3);
        assert_eq!(app.total_bytes, 1_000_042); // Trash still occupies disk space.
        assert_eq!(app.selected, None);
        assert!(app.totals_rx.is_none());
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
        assert!(app.totals_rx.is_none());
    }
    #[test]
    fn deletions_mask_inflight_resizes() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        app.hide_file(1);
        settle(&mut app, &ctx);
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
        // A fresh scan resets holes and slot identities.
        app.replace_snapshot(snapshot());
        assert!(app.hidden.is_empty());
        settle(&mut app, &ctx);
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
    fn errors_take_priority_over_success_feedback_until_dismissed() {
        let mut app = App::default();
        app.message = Some("文件无法移动，请检查目录权限。".into());
        app.action_notice = Some("已移到废纸篓。".into());
        app.collect_notices();
        assert_eq!(app.notice.as_ref().unwrap().kind, NoticeKind::Error);
        assert!(app.notice.as_ref().unwrap().text.contains("移动"));
        app.action_notice = Some("已移到废纸篓。".into());
        app.collect_notices();
        assert_eq!(app.notice.as_ref().unwrap().kind, NoticeKind::Error);
        app.notice = None;
        app.action_notice = Some("已移到废纸篓。".into());
        app.collect_notices();
        assert_eq!(app.notice.as_ref().unwrap().kind, NoticeKind::Success);
    }
    #[test]
    fn stopped_scan_progress_and_errors_preserve_empty_view_bounds() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
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
        app.action_notice = Some("已移到废纸篓。".into());
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
        let (_totals_tx, totals_rx) = mpsc::channel();
        app.totals_rx = Some(totals_rx);
        check(&mut app);
        app.totals_rx = None;
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
        check(&mut app);
    }
    #[test]
    fn long_selected_path_does_not_resize_canvas() {
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
        data.files[0].parent = data.files[0].path.parent().unwrap().into();
        let mut app = App::default();
        app.access = permission::Access::Granted;
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
        app.action_notice = Some("已移到废纸篓。".into());
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
            file.parent = Path::new("/tooltip").into();
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
        let mut heights = Vec::new();
        // Warm up the first Area sizing pass, alternate files, and leave the canvas.
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
            let point = hovered
                .map(|index| points[index])
                .unwrap_or(egui::pos2(1.0, 1.0));
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
                        path.replace('\n', ""),
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
        let wrapped = wrap_path(&ctx, &path, 360.0);
        let (shape, _) = text_shape(last.as_ref().unwrap(), &wrapped);
        assert!(!shape.galley.elided);
        assert!(shape.galley.rows.len() > 1);
    }

    #[test]
    fn tooltip_wraps_at_path_separators_without_losing_unicode() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let path = "/Users/用户/Library/Application Support/示例/file.txt";
        let mut wrapped = String::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            wrapped = wrap_path(ctx, path, 130.0);
        });
        assert_eq!(wrapped.replace('\n', ""), path);
        assert!(wrapped.contains('\n'));
        assert!(wrapped.split('\n').all(|line| !line.is_empty()));
        assert!(
            wrapped
                .split('\n')
                .skip(1)
                .all(|line| !line.starts_with('/'))
        );
        assert!(
            wrapped
                .split('\n')
                .take_while(|line| !line.ends_with("file.txt"))
                .all(|line| line.ends_with('/'))
        );
    }

    #[test]
    fn tooltip_wraps_long_components_on_a_narrow_screen() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut file = snapshot().files.remove(0);
        file.path = format!("/用户/{}", "longfilename".repeat(10)).into();
        let mut output = None;
        let mut tooltip_rect = None;
        for pass in 0..3 {
            output = Some(ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(240.0, 560.0),
                    )),
                    time: Some(pass as f64),
                    events: vec![egui::Event::PointerMoved(egui::pos2(220.0, 30.0))],
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
        let rect = tooltip_rect.unwrap();
        assert!(rect.left() >= 0.0 && rect.right() <= 240.0, "{rect:?}");
        let path = file.path.to_string_lossy();
        let wrapped = wrap_path(&ctx, &path, 208.0);
        let (shape, _) = text_shape(output.as_ref().unwrap(), &wrapped);
        assert!(!shape.galley.elided);
        assert!(shape.galley.rows.len() > 1);
        assert!(shape.galley.rows.iter().all(|row| row.size.x <= 208.0));
    }

    #[test]
    fn deep_path_tooltip_fits_screen_and_scrolls_from_the_hovered_tile() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut file = snapshot().files.remove(0);
        file.path = format!("/{}/last.txt", "folder/".repeat(60)).into();
        let mut tooltip_rect = None;
        let mut last_output = None;
        for pass in 0..5 {
            let mut events = vec![egui::Event::PointerMoved(egui::pos2(200.0, 60.0))];
            if pass == 3 {
                events.push(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(0.0, -100_000.0),
                    modifiers: egui::Modifiers::NONE,
                });
            }
            last_output = Some(ctx.run(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(240.0, 160.0),
                    )),
                    time: Some(pass as f64),
                    events,
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
        let rect = tooltip_rect.expect("tooltip remains open while scrolling");
        assert!(rect.top() >= 0.0 && rect.bottom() <= 160.0, "{rect:?}");
        let scroll_id = egui::Id::new(("file-tooltip-scroll", &file.path));
        let offset = ctx.data(|data| data.get_temp::<f32>(scroll_id)).unwrap();
        assert!(
            offset > 0.0,
            "scroll input must reveal the rest of the path"
        );
        let output = last_output.unwrap();
        let path = file.path.to_string_lossy();
        let (shape, clip) = output
            .shapes
            .iter()
            .find_map(|item| {
                if let egui::Shape::Text(shape) = &item.shape
                    && shape.galley.job.text.replace('\n', "") == path
                {
                    Some((shape, item.clip_rect))
                } else {
                    None
                }
            })
            .expect("full path is laid out");
        let last_row = shape.galley.rows.last().unwrap();
        let bottom = shape.pos.y + last_row.pos.y + last_row.size.y;
        assert!(
            bottom <= clip.bottom() + 1.0,
            "last path segment must be visible after scrolling: {bottom} / {clip:?}"
        );
    }

    #[test]
    #[ignore = "release performance benchmark"]
    fn full_app_3_5m_steady_frame() {
        let n = 3_500_000;
        let files: Vec<_> = (0..n)
            .into_par_iter()
            .map(|i| scan::FileEntry {
                can_trash: true,
                path: format!(
                    "/volume{}/folder{}/group{}/file{i}",
                    i / 350000,
                    i / 100000,
                    i / 1000
                )
                .into(),
                parent: Path::new(&format!(
                    "/volume{}/folder{}/group{}",
                    i / 350000,
                    i / 100000,
                    i / 1000
                ))
                .into(),
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
        let platform_count = app.map_cache.as_ref().unwrap().map.platform_count();
        assert!(
            platform_count > 10,
            "benchmark must include multiple visible directory levels"
        );
        eprintln!("visible_platforms={platform_count}");
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
        assert!(resize_request.as_secs_f64() * 1000.0 < 16.7);
        eprintln!("request CPU resize={resize_request:?}");
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
