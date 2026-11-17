mod file_actions;
mod permission;
mod render;
mod scan;
mod treemap;

use eframe::egui::{self, Color32, FontId, Rect, RichText, Sense, Vec2};
use rayon::prelude::*;
use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    time::Duration,
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
    for path in [
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
    ] {
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

fn defer_drop<T: Send + 'static>(value: T) {
    rayon::spawn(move || drop(value));
}

struct TrashOutput {
    source: Arc<scan::ScanResult>,
    result: Result<Option<scan::ScanResult>, String>,
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
}

fn filter_snapshot(
    data: &scan::ScanResult,
    query: &str,
    known_bytes: Option<u64>,
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
                    (query.is_empty()
                        || file.path.to_string_lossy().to_lowercase().contains(&query))
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
    job: Option<Job>,
    reveal_rx: Option<mpsc::Receiver<Result<(), String>>>,
    trash_rx: Option<mpsc::Receiver<TrashOutput>>,
    query: String,
    selected: Option<usize>,
    message: Option<String>,
    action_notice: Option<String>,
    filtered: Vec<usize>,
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
            job: None,
            reveal_rx: None,
            trash_rx: None,
            query: String::new(),
            selected: None,
            message: None,
            action_notice: None,
            filtered: Vec::new(),
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
            let _ = tx.send(scan::scan(PathBuf::from("/"), c, stats));
        });
        self.job = Some(Job {
            cancel,
            progress,
            rx,
        });
        self.message = None;
    }
    fn replace_snapshot(&mut self, data: scan::ScanResult) {
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
        if self.trash_rx.is_some() {
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
        self.message = Some("正在移到废纸篓…".into());
        rayon::spawn(move || {
            let result = file_actions::move_to_trash(&path, identity).map(|moved| {
                let moved = moved?;
                // This operation can clone millions of paths; never do it on the
                // UI thread. Source identity guards against a newer scan winning.
                let mut files: Vec<_> = source
                    .files
                    .par_iter()
                    .enumerate()
                    .map(|(i, file)| if i == id { moved.clone() } else { file.clone() })
                    .collect();
                files.par_sort_unstable_by(|a, b| {
                    b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path))
                });
                Some(scan::ScanResult {
                    root: source.root.clone(),
                    files,
                    skipped: source.skipped,
                    errors: source.errors.clone(),
                    elapsed: source.elapsed,
                })
            });
            let _ = tx.send(TrashOutput { source, result });
        });
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
                    self.message = Some("Finder 操作未完成，请重试。".into());
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
                        self.message = Some("已取消扫描，保留上次扫描结果。".into());
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
                            self.action_notice = Some("已移到废纸篓。".into());
                        }
                        Ok(data) => {
                            defer_drop(data);
                            self.action_notice =
                                Some("已移到废纸篓，正在重新扫描以更新磁盘结果。".into());
                            self.start();
                        }
                        Err(error) => self.message = Some(error),
                    }
                    defer_drop(output.source);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.trash_rx = None;
                    self.message = Some("废纸篓操作未完成，请重新扫描后重试。".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
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
                            let cache = MapCache {
                                generation: output.generation,
                                key: output.key,
                                map: output.map,
                                texture: ctx.load_texture(
                                    "disk-map",
                                    image,
                                    egui::TextureOptions::NEAREST,
                                ),
                            };
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
            || self.permission_rx.is_some()
            || self.filter_rx.is_some()
            || self.layout_rx.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(100));
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
        let token = self.filter_generation.clone();
        let (tx, rx) = mpsc::channel();
        self.filter_rx = Some(rx);
        rayon::spawn(move || {
            if let Some(output) = filter_snapshot(&data, &query, known, &token, generation) {
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
            let result = match path.try_exists() {
                Ok(false) => Err("文件已移动或删除，请重新扫描。".into()),
                Err(_) => Err("无法读取文件，请检查文件访问权限。".into()),
                Ok(true) => match std::process::Command::new("/usr/bin/open")
                    .arg("-R")
                    .arg(path)
                    .output()
                {
                    Ok(output) if output.status.success() => Ok(()),
                    _ => Err("无法在 Finder 中显示文件，请确认 Finder 可用后重试。".into()),
                },
            };
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
    fn ui(&mut self, ctx: &egui::Context) {
        self.poll(ctx);
        if self.access != permission::Access::Granted {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BG))
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((ui.available_height() * 0.28).max(24.0));
                        ui.label(RichText::new("Visualize SSD").size(22.0).strong());
                        ui.add_space(20.0);
                        ui.label(RichText::new("需要完全磁盘访问权限").size(28.0).strong());
                        ui.add_space(12.0);
                        if self.permission_rx.is_some() {
                            ui.spinner();
                            ui.label("正在检查权限，检查通过后自动扫描启动磁盘。");
                        } else {
                            let reason = if self.access == permission::Access::Denied {
                                "macOS 尚未允许此应用读取受保护文件，扫描已阻止。"
                            } else {
                                "尚未验证完全磁盘访问权限，扫描已阻止。"
                            };
                            ui.label(RichText::new(reason).color(MUTED));
                            ui.add_space(12.0);
                            ui.label("在系统设置中添加并开启 /Applications/Visualize SSD.app。");
                            ui.label("授权后点击重新检查；若系统要求，请退出并重新打开应用。");
                            ui.add_space(20.0);
                            ui.horizontal(|ui| {
                                ui.add_space((ui.available_width() * 0.5 - 150.0).max(0.0));
                                if ui.button("打开完全磁盘访问权限设置").clicked() {
                                    self.open_permissions();
                                }
                                if ui.button("重新检查").clicked() {
                                    self.start();
                                }
                            });
                        }
                        if let Some(message) = &self.message {
                            ui.label(RichText::new(message).color(MUTED));
                        }
                    });
                });
            return;
        }
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(16.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Visualize SSD").size(22.0).strong());
                    ui.add_space(16.0);
                    if ui
                        .add_enabled(self.job.is_none(), egui::Button::new("重新扫描启动磁盘"))
                        .clicked()
                    {
                        self.start();
                    }
                    if let Some(job) = &self.job {
                        if ui.button("取消扫描").clicked() {
                            job.cancel.store(true, Ordering::Relaxed);
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("文件大小 · 逻辑字节").color(MUTED));
                    });
                });
                if let Some(job) = &self.job {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let p = job.progress.lock().unwrap_or_else(|e| e.into_inner());
                        ui.label(format!(
                            "扫描中 · {} 个文件 · {} · 跳过 {} 项",
                            p.files,
                            bytes(p.bytes),
                            p.skipped
                        ));
                    });
                    if self.data.is_some() {
                        ui.label(RichText::new("下方显示上次完成的扫描结果").color(MUTED));
                    }
                }
                if let Some(message) = &self.message {
                    ui.label(RichText::new(message).color(Color32::from_rgb(235, 180, 116)));
                }
                if let Some(notice) = &self.action_notice {
                    ui.label(RichText::new(notice).color(ACCENT));
                }
            });
        self.filter();
        if self.data.is_none() {
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(BG))
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((ui.available_height() * 0.32).max(32.0));
                        let heading = if self.job.is_some() {
                            "正在扫描整个启动磁盘"
                        } else {
                            "扫描已停止"
                        };
                        ui.label(RichText::new(heading).size(30.0).strong());
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new("每个色块是一个文件，同一文件夹使用相同颜色。")
                                .color(MUTED),
                        );
                        ui.add_space(20.0);
                        if self.job.is_none() && ui.button("重新扫描启动磁盘").clicked() {
                            self.start();
                        }
                    });
                });
            return;
        }
        egui::TopBottomPanel::top("navigation")
            .frame(egui::Frame::new().fill(BG).inner_margin(16.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("搜索文件列表")
                            .desired_width(260.0),
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
                    ui.separator();
                    ui.label("启动磁盘 /");
                    ui.label(format!(
                        "{} 个文件",
                        self.data.as_ref().unwrap().files.len()
                    ));
                    if self.layout_rx.is_some() {
                        ui.add(
                            egui::Label::new(RichText::new("正在生成整盘视图…").color(MUTED))
                                .truncate(),
                        );
                    }
                    ui.label(
                        RichText::new(if self.totals_ready {
                            bytes(self.total_bytes)
                        } else {
                            "统计中…".into()
                        })
                        .strong()
                        .color(ACCENT),
                    );
                });
            });
        self.filter();
        let mut reveal = None;
        let mut trash = None;
        egui::SidePanel::right("files")
            .default_width(280.0)
            .min_width(230.0)
            .max_width(450.0)
            .resizable(true)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(16.0))
            .show(ctx, |ui| {
                if let Some(i) = self.selected {
                    let f = &self.data.as_ref().unwrap().files[i];
                    ui.label(
                        RichText::new(f.path.file_name().unwrap_or_default().to_string_lossy())
                            .size(18.0)
                            .strong(),
                    );
                    ui.label(RichText::new(bytes(f.size)).size(24.0).color(ACCENT));
                    let path = f.path.to_string_lossy();
                    if ui
                        .add(
                            egui::Label::new(RichText::new(path.as_ref()).small().color(MUTED))
                                .wrap()
                                .sense(Sense::click()),
                        )
                        .on_hover_text("点击复制完整路径")
                        .clicked()
                    {
                        if let Some(exact) = f.path.to_str() {
                            ctx.copy_text(exact.to_owned());
                            self.action_notice = Some("已复制完整路径。".into());
                        } else {
                            self.message =
                                Some("路径包含无法复制为文本的字符，请在 Finder 中查看。".into());
                        }
                    }
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("在 Finder 中显示").clicked() {
                            reveal = Some(i);
                        }
                        if ui
                            .add_enabled(self.trash_rx.is_none(), egui::Button::new("移到废纸篓"))
                            .clicked()
                        {
                            trash = Some(i);
                        }
                    });
                    ui.separator();
                }
                ui.label(RichText::new("文件").strong());
                ui.label(RichText::new("按大小排序 · 点击查看").small().color(MUTED));
                if self.filter_rx.is_some() {
                    ui.label(RichText::new("正在更新文件列表…").color(MUTED));
                }
                if self.filtered.is_empty() && self.filter_rx.is_none() {
                    ui.label(RichText::new("没有匹配的文件").color(MUTED));
                }
                let row_height = 42.0;
                egui::ScrollArea::vertical()
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .id_salt("file-list")
                    .show_rows(ui, row_height, self.filtered.len(), |ui, rows| {
                        for row in rows {
                            let i = self.filtered[row];
                            let f = &self.data.as_ref().unwrap().files[i];
                            let response = ui
                                .horizontal(|ui| {
                                    let (r, _) =
                                        ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                                    ui.painter().rect_filled(r, 2.0, color(&f.parent));
                                    ui.selectable_label(
                                        self.selected == Some(i),
                                        format!(
                                            "{}\n{}",
                                            f.path
                                                .file_name()
                                                .unwrap_or_default()
                                                .to_string_lossy(),
                                            bytes(f.size)
                                        ),
                                    )
                                })
                                .inner;
                            if response.clicked() {
                                self.selected = Some(i);
                            }
                            response.on_hover_text(f.path.to_string_lossy());
                        }
                    });
            });
        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(10.0))
            .show(ctx, |ui| {
                let data = self.data.as_ref().unwrap();
                if data.root == std::path::Path::new("/") {
                    ui.label(RichText::new("启动磁盘 · 不含其他挂载磁盘与网络磁盘").small().color(MUTED));
                }
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "扫描完成 · {:.1} 秒 · {} 个文件 · 跳过 {} 项（重复目录、链接、特殊文件或读取失败）",
                            data.elapsed.as_secs_f64(),
                            data.files.len(),
                            data.skipped
                        ))
                        .small()
                        .color(MUTED),
                    );
                    if !data.errors.is_empty() {
                        ui.label(
                            RichText::new(
                                "部分项目无法读取，请展开下方路径查看原因。",
                            )
                            .small()
                            .color(Color32::from_rgb(235, 180, 116)),
                        );
                    }
                });
                if !data.errors.is_empty() {
                    egui::CollapsingHeader::new("无法读取的路径").show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                            .max_height(100.0)
                            .show(ui, |ui| {
                                for error in &data.errors {
                                    ui.label(RichText::new(error).small());
                                }
                            });
                    });
                }
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(12.0))
            .show(ctx, |ui| {
                if self.data.as_ref().unwrap().files.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("启动磁盘中没有可显示的文件");
                    });
                    return;
                }
                let viewport = ui.available_size().max(Vec2::splat(1.0));
                self.layout(viewport, ctx.pixels_per_point());
                let (canvas, response) = ui.allocate_exact_size(viewport, Sense::click());
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
                        "正在生成整盘视图…",
                        FontId::proportional(16.0),
                        MUTED,
                    );
                }
                if let Some(i) = hovered {
                    let f = &data.files[i];
                    if response.clicked() {
                        self.selected = Some(i);
                    }
                    response.on_hover_ui_at_pointer(|ui| {
                        ui.label(
                            RichText::new(f.path.file_name().unwrap_or_default().to_string_lossy())
                                .strong(),
                        );
                        ui.label(bytes(f.size));
                        ui.label(RichText::new(f.path.to_string_lossy()).small().color(MUTED));
                    });
                }
            });
        if let Some(i) = reveal {
            self.reveal(i);
        }
        if let Some(i) = trash {
            self.trash(i);
        }
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
fn main() -> eframe::Result {
    eframe::run_native(
        "Visualize SSD",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([1180.0, 780.0])
                .with_min_inner_size([860.0, 560.0]),
            renderer: eframe::Renderer::Glow,
            ..Default::default()
        },
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
        let list = filter_snapshot(&data, "empty", None, &token, 1).unwrap();
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
        assert!(filter_snapshot(&data, "", None, &token, 0).is_none());
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
    fn trash_success_updates_matching_snapshot_and_error_preserves_it() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.data = Some(Arc::new(snapshot()));
        let source = app.data.clone().unwrap();
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        tx.send(TrashOutput {
            source: source.clone(),
            result: Err("无法移到废纸篓。".into()),
        })
        .unwrap();
        app.poll(&ctx);
        assert!(Arc::ptr_eq(app.data.as_ref().unwrap(), &source));
        let mut updated = snapshot();
        updated.files[1].path = "/Users/test/.Trash/large".into();
        updated.files[1].parent = "/Users/test/.Trash".into();
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        tx.send(TrashOutput {
            source,
            result: Ok(Some(updated)),
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
            app.data.as_ref().unwrap().files[1].parent,
            std::path::Path::new("/Users/test/.Trash")
        );
        assert!(app.selected.is_none());
        assert!(app.map_cache.is_none());
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
        let mut replacement = snapshot();
        replacement.files.remove(0);
        tx.send(TrashOutput {
            source,
            result: Ok(Some(replacement)),
        })
        .unwrap();
        app.poll(&ctx);
        assert!(Arc::ptr_eq(app.data.as_ref().unwrap(), &current));
        assert_eq!(app.data.as_ref().unwrap().files.len(), 3);
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
