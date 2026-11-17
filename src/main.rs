mod permission;
mod scan;
mod treemap;

use eframe::egui::{self, Color32, FontId, Rect, RichText, Sense, Vec2};
use std::{
    collections::BTreeMap,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
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

struct Job {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<scan::ScanProgress>>,
    rx: mpsc::Receiver<Result<scan::ScanResult, String>>,
    path: PathBuf,
}
struct App {
    access: permission::Access,
    permission_rx: Option<mpsc::Receiver<permission::Access>>,
    pending_scan: Option<PathBuf>,
    data: Option<scan::ScanResult>,
    job: Option<Job>,
    picker: Option<mpsc::Receiver<Option<PathBuf>>>,
    reveal_rx: Option<mpsc::Receiver<Result<(), String>>>,
    scope: Option<PathBuf>,
    query: String,
    selected: Option<usize>,
    message: Option<String>,
    filtered: Vec<usize>,
    filtered_bytes: u64,
    dirty: bool,
    layout: Vec<(usize, treemap::Rect)>,
    layout_size: Vec2,
    zoom: f32,
}
impl Default for App {
    fn default() -> Self {
        Self {
            access: permission::Access::Unverified,
            permission_rx: None,
            pending_scan: None,
            data: None,
            job: None,
            picker: None,
            reveal_rx: None,
            scope: None,
            query: String::new(),
            selected: None,
            message: None,
            filtered: Vec::new(),
            filtered_bytes: 0,
            dirty: true,
            layout: Vec::new(),
            layout_size: Vec2::ZERO,
            zoom: 1.0,
        }
    }
}
impl App {
    fn start(&mut self, path: PathBuf) {
        let (tx, rx) = mpsc::channel();
        self.queue_permission_check(path, rx);
        std::thread::spawn(move || {
            let _ = tx.send(permission::check());
        });
    }
    fn queue_permission_check(&mut self, path: PathBuf, rx: mpsc::Receiver<permission::Access>) {
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.picker = None;
        self.pending_scan = Some(path);
        self.access = permission::Access::Unverified;
        self.permission_rx = Some(rx);
        self.message = None;
    }
    fn permission_result(&mut self, access: permission::Access) -> Option<PathBuf> {
        self.access = access;
        self.permission_rx = None;
        if access == permission::Access::Granted {
            self.pending_scan.take()
        } else {
            None
        }
    }
    fn launch_scan(&mut self, path: PathBuf) {
        if self.access != permission::Access::Granted {
            return;
        }
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(scan::ScanProgress {
            files: 0,
            bytes: 0,
            skipped: 0,
        }));
        let (tx, rx) = mpsc::channel();
        let (p, c, stats) = (path.clone(), cancel.clone(), progress.clone());
        std::thread::spawn(move || {
            let _ = tx.send(scan::scan(p, c, stats));
        });
        self.job = Some(Job {
            cancel,
            progress,
            rx,
            path,
        });
        self.message = None;
    }
    fn choose(&mut self) {
        if self.access != permission::Access::Granted || self.picker.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.picker = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(
                rfd::FileDialog::new()
                    .set_title("选择要分析的文件夹")
                    .pick_folder(),
            );
        });
    }
    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.permission_rx {
            match rx.try_recv() {
                Ok(access) => {
                    if let Some(path) = self.permission_result(access) {
                        self.launch_scan(path);
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
        if let Some(rx) = &self.picker {
            match rx.try_recv() {
                Ok(path) => {
                    self.picker = None;
                    if let Some(p) = path {
                        self.start(p);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.picker = None;
                    self.message = Some("文件夹选择器已关闭，请重试。".into());
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
                        self.message = Some("已取消扫描，保留上次扫描结果。".into());
                    } else {
                        match result {
                            Ok(data) => {
                                self.scope = Some(data.root.clone());
                                self.data = Some(data);
                                self.selected = None;
                                self.query.clear();
                                self.zoom = 1.0;
                                self.dirty = true;
                            }
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
        if self.job.is_some()
            || self.picker.is_some()
            || self.reveal_rx.is_some()
            || self.permission_rx.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
    fn filter(&mut self) {
        if !self.dirty {
            return;
        }
        self.filtered.clear();
        self.filtered_bytes = 0;
        if let Some(data) = &self.data {
            let query = self.query.to_lowercase();
            for (i, file) in data.files.iter().enumerate() {
                if self
                    .scope
                    .as_ref()
                    .is_some_and(|p| !file.path.starts_with(p))
                {
                    continue;
                }
                if !query.is_empty() && !file.path.to_string_lossy().to_lowercase().contains(&query)
                {
                    continue;
                }
                self.filtered.push(i);
                self.filtered_bytes = self.filtered_bytes.saturating_add(file.size);
            }
            self.filtered.sort_unstable_by(|&a, &b| {
                data.files[b]
                    .size
                    .cmp(&data.files[a].size)
                    .then_with(|| data.files[a].path.cmp(&data.files[b].path))
            });
        }
        self.layout_size = Vec2::ZERO;
        self.dirty = false;
    }
    fn layout(&mut self, size: Vec2) {
        if self.layout_size == size {
            return;
        }
        self.layout_size = size;
        self.layout.clear();
        let Some(data) = &self.data else {
            return;
        };
        let mut groups: BTreeMap<&std::path::Path, Vec<(usize, u64)>> = BTreeMap::new();
        for &i in &self.filtered {
            let file = &data.files[i];
            groups
                .entry(&file.parent)
                .or_default()
                .push((i, file.size.max(1)));
        }
        let groups: Vec<_> = groups.into_values().collect();
        let weights: Vec<_> = groups
            .iter()
            .enumerate()
            .map(|(i, g)| (i, g.iter().fold(0u64, |n, (_, s)| n.saturating_add(*s))))
            .collect();
        for (i, r) in treemap::layout(
            &weights,
            treemap::Rect {
                x: 0.0,
                y: 0.0,
                w: size.x,
                h: size.y,
            },
        ) {
            self.layout.extend(treemap::layout(&groups[i], r));
        }
    }
    fn navigate(&mut self, p: PathBuf) {
        self.scope = Some(p);
        self.selected = None;
        self.zoom = 1.0;
        self.dirty = true;
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
                                    self.start(
                                        self.pending_scan
                                            .clone()
                                            .unwrap_or_else(|| PathBuf::from("/")),
                                    );
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
                        .add_enabled(self.picker.is_none(), egui::Button::new("选择文件夹"))
                        .clicked()
                    {
                        self.choose();
                    }
                    if ui
                        .add_enabled(self.job.is_none(), egui::Button::new("重新扫描启动磁盘"))
                        .clicked()
                    {
                        self.start(PathBuf::from("/"));
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
                    ui.label(
                        RichText::new(job.path.to_string_lossy())
                            .small()
                            .color(MUTED),
                    );
                    if self.data.is_some() {
                        ui.label(RichText::new("下方显示上次完成的扫描结果").color(MUTED));
                    }
                }
                if let Some(message) = &self.message {
                    ui.label(RichText::new(message).color(Color32::from_rgb(235, 180, 116)));
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
                            "正在扫描文件"
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
                        if ui
                            .add_enabled(
                                self.picker.is_none(),
                                egui::Button::new("选择文件夹开始")
                                    .min_size(Vec2::new(180.0, 44.0)),
                            )
                            .clicked()
                        {
                            self.choose();
                        }
                    });
                });
            return;
        }
        egui::TopBottomPanel::top("navigation")
            .frame(egui::Frame::new().fill(BG).inner_margin(16.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let root = self.data.as_ref().unwrap().root.clone();
                    let scope = self.scope.clone().unwrap_or_else(|| root.clone());
                    if ui
                        .add_enabled(scope != root, egui::Button::new("← 上一级"))
                        .clicked()
                    {
                        if let Some(p) = scope.parent() {
                            self.navigate(p.to_path_buf());
                        }
                    }
                    if ui.button("扫描根目录").clicked() {
                        self.navigate(root);
                    }
                    ui.label(RichText::new(scope.to_string_lossy()).color(MUTED));
                });
                ui.horizontal(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .hint_text("搜索文件名或路径")
                            .desired_width(260.0),
                    );
                    if response.changed() {
                        self.dirty = true;
                    }
                    if !self.query.is_empty() && ui.button("清除").clicked() {
                        self.query.clear();
                        self.dirty = true;
                    }
                    ui.separator();
                    ui.label(format!("{} 个文件", self.filtered.len()));
                    ui.label(
                        RichText::new(bytes(self.filtered_bytes))
                            .strong()
                            .color(ACCENT),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Slider::new(&mut self.zoom, 1.0..=20.0)
                                .logarithmic(true)
                                .text("放大"),
                        );
                    });
                });
            });
        self.filter();
        let mut navigate = None;
        let mut reveal = None;
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
                    ui.add(
                        egui::Label::new(
                            RichText::new(f.path.to_string_lossy()).small().color(MUTED),
                        )
                        .wrap(),
                    );
                    ui.horizontal_wrapped(|ui| {
                        if ui.button("在 Finder 中显示").clicked() {
                            reveal = Some(i);
                        }
                        if ui.button("聚焦文件夹").clicked() {
                            navigate = Some(f.parent.clone());
                        }
                    });
                    ui.separator();
                }
                ui.label(RichText::new("文件").strong());
                ui.label(
                    RichText::new("按大小排序 · 点击查看 · 双击聚焦文件夹")
                        .small()
                        .color(MUTED),
                );
                let row_height = 42.0;
                egui::ScrollArea::vertical().id_salt("file-list").show_rows(
                    ui,
                    row_height,
                    self.filtered.len(),
                    |ui, rows| {
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
                            if response.double_clicked() {
                                navigate = Some(f.parent.clone());
                            }
                            response.on_hover_text(f.path.to_string_lossy());
                        }
                    },
                );
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
                if self.filtered.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.label("没有匹配的文件");
                    });
                    return;
                }
                let size = ui.available_size().max(Vec2::splat(1.0)) * self.zoom;
                self.layout(size);
                egui::ScrollArea::both()
                    .id_salt("treemap-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let (canvas, response) = ui.allocate_exact_size(size, Sense::click());
                        let painter = ui.painter_at(canvas);
                        let pointer = response.hover_pos();
                        let mut hovered = None;
                        let data = self.data.as_ref().unwrap();
                        for &(i, r) in &self.layout {
                            let rect = Rect::from_min_size(
                                canvas.min + Vec2::new(r.x, r.y),
                                Vec2::new(r.w, r.h),
                            );
                            if !rect.intersects(ui.clip_rect()) {
                                continue;
                            }
                            let f = &data.files[i];
                            let c = color(&f.parent);
                            let inset = if r.w > 4.0 && r.h > 4.0 { 0.7 } else { 0.0 };
                            painter.rect_filled(rect.shrink(inset), 1.0, c);
                            if r.w > 84.0 && r.h > 38.0 {
                                painter.with_clip_rect(rect.shrink(5.0)).text(
                                    rect.min + Vec2::splat(7.0),
                                    egui::Align2::LEFT_TOP,
                                    f.path.file_name().unwrap_or_default().to_string_lossy(),
                                    FontId::proportional(12.0),
                                    Color32::WHITE,
                                );
                            }
                            if r.w > 84.0 && r.h > 60.0 {
                                painter.text(
                                    rect.min + Vec2::new(7.0, 26.0),
                                    egui::Align2::LEFT_TOP,
                                    bytes(f.size),
                                    FontId::proportional(11.0),
                                    Color32::from_white_alpha(190),
                                );
                            }
                            if self.selected == Some(i) {
                                painter.rect_stroke(
                                    rect.shrink(1.0),
                                    1.0,
                                    egui::Stroke::new(2.0, Color32::WHITE),
                                    egui::StrokeKind::Inside,
                                );
                            }
                            if pointer.is_some_and(|p| rect.contains(p)) {
                                hovered = Some(i);
                                painter.rect_stroke(
                                    rect.shrink(1.0),
                                    1.0,
                                    egui::Stroke::new(1.5, Color32::WHITE),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                        if let Some(i) = hovered {
                            let f = &data.files[i];
                            if response.clicked() {
                                self.selected = Some(i);
                            }
                            if response.double_clicked() {
                                navigate = Some(f.parent.clone());
                            }
                            response.on_hover_ui_at_pointer(|ui| {
                                ui.label(
                                    RichText::new(
                                        f.path.file_name().unwrap_or_default().to_string_lossy(),
                                    )
                                    .strong(),
                                );
                                ui.label(bytes(f.size));
                                ui.label(
                                    RichText::new(f.path.to_string_lossy()).small().color(MUTED),
                                );
                            });
                        }
                    });
            });
        if let Some(p) = navigate {
            self.navigate(p);
        }
        if let Some(i) = reveal {
            self.reveal(i);
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
            app.start(PathBuf::from("/"));
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bytes_are_binary_and_zero_is_exact() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1024), "1.0 KiB");
    }
    fn snapshot(root: &str) -> scan::ScanResult {
        scan::ScanResult {
            root: PathBuf::from(root),
            files: vec![],
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        }
    }
    #[test]
    fn permission_gate_blocks_scanning_and_folder_picker() {
        let mut app = App::default();
        app.launch_scan(PathBuf::from("/"));
        app.choose();
        assert!(app.job.is_none());
        assert!(app.picker.is_none());
        app.pending_scan = Some(PathBuf::from("/"));
        assert!(app.permission_result(permission::Access::Denied).is_none());
        assert!(
            app.permission_result(permission::Access::Unverified)
                .is_none()
        );
        assert!(app.job.is_none());
        assert_eq!(app.pending_scan.as_deref(), Some(std::path::Path::new("/")));
    }
    #[test]
    fn disconnected_permission_check_fails_closed() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        let (tx, rx) = mpsc::channel();
        app.queue_permission_check(PathBuf::from("/"), rx);
        drop(tx);
        app.poll(&ctx);
        assert_eq!(app.access, permission::Access::Unverified);
        assert!(app.job.is_none());
        assert!(app.permission_rx.is_none());
        assert_eq!(app.pending_scan, Some(PathBuf::from("/")));
    }
    #[test]
    fn permission_grant_consumes_startup_request_only_once() {
        let mut app = App::default();
        app.pending_scan = Some(PathBuf::from("/"));
        assert_eq!(
            app.permission_result(permission::Access::Granted),
            Some(PathBuf::from("/"))
        );
        assert!(app.permission_result(permission::Access::Granted).is_none());
        app.permission_result(permission::Access::Denied);
        assert!(app.permission_result(permission::Access::Granted).is_none());
    }
    #[test]
    fn every_scan_rechecks_permission_and_invalidates_old_probe() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.access = permission::Access::Granted;
        let cancel = Arc::new(AtomicBool::new(false));
        let (_tx, rx) = mpsc::channel();
        app.job = Some(Job {
            cancel: cancel.clone(),
            progress: Arc::default(),
            rx,
            path: PathBuf::from("/old"),
        });
        let (old_tx, old_rx) = mpsc::channel();
        app.queue_permission_check(PathBuf::from("/"), old_rx);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(app.job.is_none());
        assert_eq!(app.access, permission::Access::Unverified);
        let (new_tx, new_rx) = mpsc::channel();
        app.queue_permission_check(PathBuf::from("/new"), new_rx);
        assert!(old_tx.send(permission::Access::Granted).is_err());
        new_tx.send(permission::Access::Denied).unwrap();
        app.poll(&ctx);
        assert_eq!(app.access, permission::Access::Denied);
        assert!(app.job.is_none());
        assert_eq!(app.pending_scan, Some(PathBuf::from("/new")));
    }
    #[test]
    fn cancelled_result_keeps_previous_snapshot() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(snapshot("/old"));
        let (tx, rx) = mpsc::channel();
        app.job = Some(Job {
            cancel: Arc::new(AtomicBool::new(true)),
            progress: Arc::new(Mutex::new(scan::ScanProgress::default())),
            rx,
            path: PathBuf::from("/new"),
        });
        tx.send(Ok(snapshot("/new"))).unwrap();
        app.poll(&ctx);
        assert_eq!(app.data.as_ref().unwrap().root, PathBuf::from("/old"));
        assert!(app.job.is_none());
        assert!(app.message.as_ref().unwrap().contains("取消"));
    }
    #[test]
    fn replaced_channel_cannot_publish_old_scan() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        let (old_tx, old_rx) = mpsc::channel();
        app.job = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Mutex::new(scan::ScanProgress::default())),
            rx: old_rx,
            path: PathBuf::from("/old"),
        });
        let (new_tx, new_rx) = mpsc::channel();
        let old = app.job.take().unwrap();
        old.cancel.store(true, Ordering::Relaxed);
        drop(old);
        app.job = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(Mutex::new(scan::ScanProgress::default())),
            rx: new_rx,
            path: PathBuf::from("/new"),
        });
        assert!(old_tx.send(Ok(snapshot("/old"))).is_err());
        new_tx.send(Ok(snapshot("/new"))).unwrap();
        app.poll(&ctx);
        assert_eq!(app.data.as_ref().unwrap().root, PathBuf::from("/new"));
        assert_eq!(app.scope, Some(PathBuf::from("/new")));
    }
    #[test]
    fn files_remain_addressable_and_folder_filter_is_component_based() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(scan::ScanResult {
            root: PathBuf::from("/data"),
            files: vec![
                scan::FileEntry {
                    path: PathBuf::from("/data/a/empty"),
                    parent: PathBuf::from("/data/a"),
                    size: 0,
                },
                scan::FileEntry {
                    path: PathBuf::from("/data/a/large"),
                    parent: PathBuf::from("/data/a"),
                    size: 1_000_000,
                },
                scan::FileEntry {
                    path: PathBuf::from("/data/ab/other"),
                    parent: PathBuf::from("/data/ab"),
                    size: 42,
                },
            ],
            skipped: 0,
            errors: vec![],
            elapsed: Duration::from_secs(1),
        });
        app.scope = Some(PathBuf::from("/data"));
        app.filter();
        app.layout(Vec2::new(800.0, 600.0));
        assert_eq!(app.filtered.len(), 3);
        assert_eq!(app.layout.len(), 3);
        app.navigate(PathBuf::from("/data/a"));
        app.filter();
        assert_eq!(app.filtered.len(), 2);
        assert_eq!(app.filtered_bytes, 1_000_000);
        app.query = "empty".into();
        app.dirty = true;
        app.filter();
        assert_eq!(app.filtered, vec![0]);
        app.selected = Some(0);
        let output = ctx.run(egui::RawInput::default(), |ctx| app.ui(ctx));
        assert!(!output.shapes.is_empty());
    }
    #[test]
    fn empty_ui_renders_without_native_window() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| app.ui(ctx));
        assert!(!output.shapes.is_empty());
    }
}
