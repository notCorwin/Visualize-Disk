#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod demo;
mod file_actions;
mod permission;
mod platform;
mod render;
mod scan;
mod scene;
mod treemap;
mod watcher;

use eframe::egui::{self, Color32, FontId, Pos2, Rect, RichText, Sense, Vec2};
use scene::{NodeId, TileInfo, TransitionBatch, TransitionKind, ViewMode};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
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

    [
        PINE, BLUE_GRAY, SLATE, BRASS, MOSS, TERRACOTTA, BLUE_GRAY, SLATE, PINE, PINE, MOSS,
        BLUE_GRAY,
    ]
};

const TRANSITION_SECONDS: f64 = 0.24;
const WATCH_DEBOUNCE: Duration = Duration::from_millis(180);
const RESIZE_DEBOUNCE: Duration = Duration::from_millis(120);

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

pub(crate) fn color(path: &Path) -> Color32 {
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

fn tooltip_pointer(response: &egui::Response) -> Option<Pos2> {
    response.hover_pos().or_else(|| {
        egui::Tooltip::was_tooltip_open_last_frame(&response.ctx, response.id)
            .then(|| response.ctx.pointer_hover_pos())
            .flatten()
            .filter(|pos| response.rect.contains(*pos))
    })
}

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

fn tooltip_size(size: u64, folder: bool) -> String {
    if folder {
        format!("文件夹 {}", bytes(size))
    } else {
        bytes(size)
    }
}

fn show_tile_tooltip(response: &egui::Response, tile: &TileInfo) {
    if !response.enabled() || tooltip_pointer(response).is_none() {
        return;
    }
    let available = (response.ctx.content_rect().width() - 32.0).max(1.0);
    let natural = response.ctx.fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(
                tile.path.to_string_lossy().into_owned(),
                FontId::proportional(14.0),
                MUTED,
            )
            .size()
            .x
    });
    let width = natural.max(220.0).min(360.0).min(available);
    egui::Tooltip::for_widget(response)
        .at_pointer()
        .width(width)
        .show(|ui| {
            ui.set_width(width);
            let path = wrap_path(ui.ctx(), &tile.path.to_string_lossy(), width);
            let name = tile
                .path
                .file_name()
                .unwrap_or_else(|| tile.path.as_os_str())
                .to_string_lossy();
            let content_height = ui.ctx().fonts_mut(|fonts| {
                let layout = |text: String, size| {
                    let mut job = egui::text::LayoutJob::simple(
                        text,
                        FontId::proportional(size),
                        TEXT,
                        width,
                    );
                    job.wrap.break_anywhere = true;
                    job
                };
                fonts.layout_job(layout(name.into_owned(), 15.0)).size().y
                    + fonts
                        .layout_job(layout(tooltip_size(tile.size, tile.is_folder), 14.0))
                        .size()
                        .y
                    + fonts.layout_job(layout(path, 14.0)).size().y
            }) + 2.0 * ui.spacing().item_spacing.y;
            let max_height = (ui.ctx().content_rect().height() * 0.5 - 50.0).max(1.0);
            if content_height <= max_height {
                tile_tooltip_content(ui, tile);
                return;
            }
            let scroll_id = egui::Id::new(("file-tooltip-scroll", &tile.path));
            let previous_offset = ui
                .ctx()
                .data(|data| data.get_temp::<f32>(scroll_id))
                .unwrap_or(0.0);
            let wheel = ui.ctx().input(|input| input.raw_scroll_delta.y);
            let content = egui::ScrollArea::vertical()
                .id_salt(scroll_id)
                .max_height(max_height)
                .auto_shrink([false, true])
                .vertical_scroll_offset((previous_offset - wheel).max(0.0))
                .show(ui, |ui| tile_tooltip_content(ui, tile));
            ui.ctx()
                .data_mut(|data| data.insert_temp(scroll_id, content.state.offset.y));
            if content.content_size.y > content.inner_rect.height() {
                ui.label(RichText::new("滚动查看完整路径").color(MUTED).small());
            }
        });
}

fn tile_tooltip_content(ui: &mut egui::Ui, tile: &TileInfo) {
    wrapped_text(
        ui,
        tile.path
            .file_name()
            .unwrap_or_else(|| tile.path.as_os_str())
            .to_string_lossy()
            .into_owned(),
        15.0,
        TEXT,
    );
    ui.label(tooltip_size(tile.size, tile.is_folder));
    let path = wrap_path(ui.ctx(), &tile.path.to_string_lossy(), ui.available_width());
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct ViewportKey {
    logical_size: Vec2,
    physical_size: [usize; 2],
}

impl ViewportKey {
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

    fn bounds(self) -> treemap::Rect {
        treemap::Rect {
            x: 0.0,
            y: 0.0,
            w: self.logical_size.x,
            h: self.logical_size.y,
        }
    }
}

struct SceneStats {
    root: PathBuf,
    real_root: PathBuf,
    total_files: usize,
    total_bytes: u64,
    skipped: u64,
    errors: Vec<String>,
    elapsed: Duration,
}

enum WorkerCmd {
    LoadScan {
        generation: u64,
        scan: scan::ScanResult,
        mode: ViewMode,
        bounds: treemap::Rect,
        physical_size: [usize; 2],
        reconcile: bool,
    },
    SwitchMode {
        generation: u64,
        mode: ViewMode,
        bounds: treemap::Rect,
        physical_size: [usize; 2],
    },
    Resize {
        generation: u64,
        physical_size: [usize; 2],
    },
    RemoveFile {
        generation: u64,
        path: PathBuf,
        identity: scan::FileIdentity,
    },
    RemoveDirectory {
        generation: u64,
        path: PathBuf,
        identity: scan::FileIdentity,
    },
    DemoAction {
        generation: u64,
        action: demo::DemoAction,
        seed: u64,
    },
    WatchEvents {
        generation: u64,
        events: Vec<watcher::WatchEvent>,
    },
    Shutdown,
}

enum WorkerEvent {
    FullFrame {
        generation: u64,
        mode: ViewMode,
        bounds: treemap::Rect,
        patch: render::RasterPatch,
        tiles: HashMap<NodeId, TileInfo>,
        stats: SceneStats,
        new_root: bool,
    },
    Resized {
        generation: u64,
        patch: render::RasterPatch,
        tiles: HashMap<NodeId, TileInfo>,
    },
    Patch {
        generation: u64,
        patches: Vec<render::RasterPatch>,
        removed_ids: Vec<NodeId>,
        upserted_tiles: Vec<TileInfo>,
        transitions: TransitionBatch,
        stats: SceneStats,
    },
    RootChanged {
        generation: u64,
    },
    Idle {
        generation: u64,
    },
}

struct SceneWorker {
    tx: mpsc::Sender<WorkerCmd>,
    rx: mpsc::Receiver<WorkerEvent>,
    token: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
}

impl SceneWorker {
    fn spawn(wake: Arc<dyn Fn() + Send + Sync>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCmd>();
        let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();
        let token = Arc::new(AtomicU64::new(1));
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_token = token.clone();
        let worker_cancel = cancel.clone();

        std::thread::spawn(move || {
            let mut scene_tree: Option<scene::SceneTree> = None;
            let mut layout_tree: Option<scene::LayoutTree> = None;
            let mut physical_size = [1usize, 1usize];

            let make_stats = |s: &scene::SceneTree| SceneStats {
                root: s.root.clone(),
                real_root: s.real_root.clone(),
                total_files: s.total_files(),
                total_bytes: s.total_bytes(),
                skipped: s.skipped,
                errors: s.errors.clone(),
                elapsed: s.elapsed,
            };

            let emit_idle = |generation: u64, evt_tx: &mpsc::Sender<WorkerEvent>| {
                let _ = evt_tx.send(WorkerEvent::Idle { generation });
                wake();
            };

            let emit_dirty = |s: &scene::SceneTree,
                              l: &mut scene::LayoutTree,
                              dirty: &[usize],
                              generation: u64,
                              phys: [usize; 2],
                              evt_tx: &mpsc::Sender<WorkerEvent>| {
                if dirty.is_empty() {
                    emit_idle(generation, evt_tx);
                    return;
                }
                let px = render::logical_pixel_size(l.bounds, phys);
                let Some(diff) = l.apply_dirty(s, dirty, px, &worker_token, generation) else {
                    return;
                };
                if diff.dirty_rects.is_empty() {
                    emit_idle(generation, evt_tx);
                    return;
                }
                let Some(patches) =
                    l.raster_dirty(&diff.dirty_rects, phys, &worker_token, generation)
                else {
                    return;
                };
                let _ = evt_tx.send(WorkerEvent::Patch {
                    generation,
                    patches,
                    removed_ids: diff.removed_ids,
                    upserted_tiles: diff.upserted_tiles,
                    transitions: diff.transitions,
                    stats: make_stats(s),
                });
                wake();
            };

            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    WorkerCmd::Shutdown => break,
                    WorkerCmd::LoadScan {
                        generation,
                        scan,
                        mode,
                        bounds,
                        physical_size: phys,
                        reconcile,
                    } => {
                        if worker_token.load(Ordering::Relaxed) != generation {
                            continue;
                        }
                        physical_size = render::capped_size(phys);
                        let same_root = reconcile
                            && scene_tree.as_ref().is_some_and(|s| s.root == scan.root)
                            && layout_tree
                                .as_ref()
                                .is_some_and(|l| l.mode == mode && l.bounds == bounds);
                        if same_root {
                            if let (Some(s), Some(l)) = (scene_tree.as_mut(), layout_tree.as_mut())
                                && let Some(dirty) = s.reconcile_scan(scan)
                            {
                                emit_dirty(s, l, &dirty, generation, physical_size, &evt_tx);
                            } else {
                                emit_idle(generation, &evt_tx);
                            }
                            continue;
                        }

                        let tree = scene::SceneTree::from_scan(scan);
                        let px = render::logical_pixel_size(bounds, physical_size);
                        let Some(layout) = scene::LayoutTree::build(
                            &tree,
                            mode,
                            bounds,
                            px,
                            &worker_token,
                            generation,
                        ) else {
                            continue;
                        };
                        let Some(patch) =
                            layout.raster_full(physical_size, &worker_token, generation)
                        else {
                            continue;
                        };
                        let tiles = layout.collect_visible_tiles(&tree);
                        let stats = make_stats(&tree);
                        scene_tree = Some(tree);
                        layout_tree = Some(layout);
                        let _ = evt_tx.send(WorkerEvent::FullFrame {
                            generation,
                            mode,
                            bounds,
                            patch,
                            tiles,
                            stats,
                            new_root: true,
                        });
                        wake();
                    }
                    WorkerCmd::SwitchMode {
                        generation,
                        mode,
                        bounds,
                        physical_size: phys,
                    } => {
                        if worker_token.load(Ordering::Relaxed) != generation {
                            continue;
                        }
                        let Some(s) = scene_tree.as_ref() else {
                            continue;
                        };
                        physical_size = render::capped_size(phys);
                        let px = render::logical_pixel_size(bounds, physical_size);
                        let Some(layout) = scene::LayoutTree::build(
                            s,
                            mode,
                            bounds,
                            px,
                            &worker_token,
                            generation,
                        ) else {
                            continue;
                        };
                        let Some(patch) =
                            layout.raster_full(physical_size, &worker_token, generation)
                        else {
                            continue;
                        };
                        let tiles = layout.collect_visible_tiles(s);
                        let stats = make_stats(s);
                        layout_tree = Some(layout);
                        let _ = evt_tx.send(WorkerEvent::FullFrame {
                            generation,
                            mode,
                            bounds,
                            patch,
                            tiles,
                            stats,
                            new_root: false,
                        });
                        wake();
                    }
                    WorkerCmd::Resize {
                        generation,
                        physical_size: phys,
                    } => {
                        if worker_token.load(Ordering::Relaxed) != generation {
                            continue;
                        }
                        let (Some(s), Some(l)) = (scene_tree.as_ref(), layout_tree.as_mut()) else {
                            continue;
                        };
                        let next_size = render::capped_size(phys);
                        if next_size == physical_size {
                            emit_idle(generation, &evt_tx);
                            continue;
                        }
                        physical_size = next_size;
                        l.pixel_size = render::logical_pixel_size(l.bounds, physical_size);
                        let Some(patch) = l.raster_full(physical_size, &worker_token, generation)
                        else {
                            continue;
                        };
                        let tiles = l.collect_visible_tiles(s);
                        let _ = evt_tx.send(WorkerEvent::Resized {
                            generation,
                            patch,
                            tiles,
                        });
                        wake();
                    }
                    WorkerCmd::RemoveFile {
                        generation,
                        path,
                        identity,
                    } => {
                        if worker_token.load(Ordering::Relaxed) != generation {
                            continue;
                        }
                        let (Some(s), Some(l)) = (scene_tree.as_mut(), layout_tree.as_mut()) else {
                            continue;
                        };
                        let dirty = s.remove_file(&path, identity);
                        emit_dirty(s, l, &dirty, generation, physical_size, &evt_tx);
                    }
                    WorkerCmd::RemoveDirectory {
                        generation,
                        path,
                        identity,
                    } => {
                        if worker_token.load(Ordering::Relaxed) != generation {
                            continue;
                        }
                        let (Some(s), Some(l)) = (scene_tree.as_mut(), layout_tree.as_mut()) else {
                            continue;
                        };
                        let dirty = s.remove_directory(&path, identity);
                        emit_dirty(s, l, &dirty, generation, physical_size, &evt_tx);
                    }
                    WorkerCmd::DemoAction {
                        generation,
                        action,
                        seed,
                    } => {
                        if worker_token.load(Ordering::Relaxed) != generation {
                            continue;
                        }
                        let (Some(s), Some(l)) = (scene_tree.as_mut(), layout_tree.as_mut()) else {
                            continue;
                        };
                        let dirty = s.apply_demo_action(action, seed, l.mode);
                        emit_dirty(s, l, &dirty, generation, physical_size, &evt_tx);
                    }
                    WorkerCmd::WatchEvents { generation, events } => {
                        if worker_token.load(Ordering::Relaxed) != generation {
                            continue;
                        }
                        let (Some(s), Some(l)) = (scene_tree.as_mut(), layout_tree.as_mut()) else {
                            continue;
                        };
                        if events
                            .iter()
                            .any(|e| matches!(e, watcher::WatchEvent::RootChanged))
                        {
                            let _ = evt_tx.send(WorkerEvent::RootChanged { generation });
                            wake();
                            continue;
                        }

                        // Collect unique directories to shallow-rescan, expanding
                        // `RescanSubtree` and newly created subdirectories via BFS.
                        let mut queue = VecDeque::new();
                        let mut seen = BTreeSet::new();
                        for event in events {
                            match event {
                                watcher::WatchEvent::DirtyDirs(dirs) => {
                                    for d in dirs {
                                        if d.starts_with(&s.root) && seen.insert(d.clone()) {
                                            queue.push_back((d, false));
                                        }
                                    }
                                }
                                watcher::WatchEvent::RescanSubtree(dir) => {
                                    if dir.starts_with(&s.root) && seen.insert(dir.clone()) {
                                        queue.push_back((dir, true));
                                    }
                                }
                                watcher::WatchEvent::RootChanged => {}
                            }
                        }

                        let mut all_dirty = Vec::new();
                        let root = s.root.clone();
                        let real_root = s.real_root.clone();
                        while let Some((dir_path, recurse_existing)) = queue.pop_front() {
                            if worker_token.load(Ordering::Relaxed) != generation
                                || worker_cancel.load(Ordering::Relaxed)
                            {
                                break;
                            }
                            let Ok(snapshot) = scan::scan_single_directory(
                                &root,
                                &real_root,
                                &dir_path,
                                &worker_cancel,
                            ) else {
                                break;
                            };
                            let existing_subdirs = if recurse_existing {
                                snapshot
                                    .as_ref()
                                    .map(|snap| {
                                        snap.subdirs
                                            .iter()
                                            .map(|(p, _)| p.clone())
                                            .collect::<Vec<_>>()
                                    })
                                    .unwrap_or_default()
                            } else {
                                Vec::new()
                            };
                            let (dirty, new_subdirs) = s.apply_dir_snapshot(&dir_path, snapshot);
                            all_dirty.extend(dirty);
                            for sub in new_subdirs {
                                if seen.insert(sub.clone()) {
                                    queue.push_back((sub, true));
                                }
                            }
                            for sub in existing_subdirs {
                                if seen.insert(sub.clone()) {
                                    queue.push_back((sub, true));
                                }
                            }
                        }

                        if worker_token.load(Ordering::Relaxed) == generation {
                            emit_dirty(s, l, &all_dirty, generation, physical_size, &evt_tx);
                        }
                    }
                }
            }
        });

        Self {
            tx: cmd_tx,
            rx: evt_rx,
            token,
            cancel,
        }
    }

    fn bump_generation(&self) -> u64 {
        self.token.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn current_generation(&self) -> u64 {
        self.token.load(Ordering::Relaxed)
    }
}

impl Drop for SceneWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.bump_generation();
        let _ = self.tx.send(WorkerCmd::Shutdown);
    }
}

struct ActiveTransition {
    batch: TransitionBatch,
    started: f64,
}

struct UiScene {
    generation: u64,
    mode: ViewMode,
    bounds: treemap::Rect,
    physical_size: [usize; 2],
    texture: egui::TextureHandle,
    owners: render::OwnerGrid,
    tiles: HashMap<NodeId, TileInfo>,
    transitions: Vec<ActiveTransition>,
}

impl UiScene {
    fn is_animating_at(&self, now: f64, world_point: Option<(f32, f32)>) -> bool {
        self.transitions.iter().any(|tr| {
            if now - tr.started >= TRANSITION_SECONDS {
                return false;
            }
            match world_point {
                None => true,
                Some((wx, wy)) => tr
                    .batch
                    .dirty_rects
                    .iter()
                    .any(|r| wx >= r.x && wx <= r.x + r.w && wy >= r.y && wy <= r.y + r.h),
            }
        })
    }
}

struct ScanJob {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<scan::ScanProgress>>,
    rx: mpsc::Receiver<Result<scan::ScanResult, String>>,
}

struct TrashResult {
    generation: u64,
    path: PathBuf,
    identity: scan::FileIdentity,
    is_folder: bool,
    result: Result<(), String>,
}

const DEMO_AUTO_INTERVAL: Duration = Duration::from_millis(650);

struct App {
    demo: bool,
    demo_seed: u64,
    demo_auto: bool,
    demo_next_tick: Option<Instant>,
    access: permission::Access,
    permission_rx: Option<mpsc::Receiver<permission::Access>>,
    pending_scan: bool,
    mode: ViewMode,
    job: Option<ScanJob>,
    pending_scan_result: Option<scan::ScanResult>,
    stats: Option<SceneStats>,
    worker: Option<SceneWorker>,
    worker_busy: bool,
    ui_scene: Option<UiScene>,
    pending_resize: Option<([usize; 2], Instant)>,
    watcher: Option<watcher::FsWatcher>,
    watch_rx: Option<mpsc::Receiver<watcher::WatchEvent>>,
    pending_watch_events: Vec<watcher::WatchEvent>,
    watch_flush_at: Option<Instant>,
    reveal_rx: Option<mpsc::Receiver<Result<(), String>>>,
    trash_rx: Option<mpsc::Receiver<TrashResult>>,
    selected: Option<NodeId>,
    message: Option<String>,
    action_notice: Option<String>,
    notice: Option<Notice>,
    show_errors: bool,
    canvas_rect: Option<Rect>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            demo: false,
            demo_seed: 1,
            demo_auto: false,
            demo_next_tick: None,
            access: permission::Access::Unverified,
            permission_rx: None,
            pending_scan: false,
            mode: ViewMode::default(),
            job: None,
            pending_scan_result: None,
            stats: None,
            worker: None,
            worker_busy: false,
            ui_scene: None,
            pending_resize: None,
            watcher: None,
            watch_rx: None,
            pending_watch_events: Vec::new(),
            watch_flush_at: None,
            reveal_rx: None,
            trash_rx: None,
            selected: None,
            message: None,
            action_notice: None,
            notice: None,
            show_errors: false,
            canvas_rect: None,
        }
    }
}

impl App {
    fn ensure_worker(&mut self, ctx: &egui::Context) -> &SceneWorker {
        if self.worker.is_none() {
            let ctx = ctx.clone();
            self.worker = Some(SceneWorker::spawn(Arc::new(move || ctx.request_repaint())));
        }
        self.worker.as_ref().unwrap()
    }

    fn start(&mut self) {
        if self.demo {
            self.reset_demo();
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.queue_permission_check(rx);
        std::thread::spawn(move || {
            let _ = tx.send(permission::check());
        });
    }

    fn reset_demo(&mut self) {
        self.access = permission::Access::Granted;
        self.watcher = None;
        self.watch_rx = None;
        self.pending_watch_events.clear();
        self.selected = None;
        self.demo_seed = 1;
        self.demo_auto = false;
        self.demo_next_tick = None;
        self.message = None;
        self.pending_scan_result = Some(demo::virtual_scan());
    }

    fn toggle_demo_auto(&mut self) {
        self.demo_auto = !self.demo_auto;
        self.demo_next_tick = self.demo_auto.then(Instant::now);
    }

    fn next_demo_auto_action(&self) -> demo::DemoAction {
        let count = self.stats.as_ref().map_or(0, |s| s.total_files);
        if count < 200 {
            demo::DemoAction::AddFile
        } else if count > 1400 {
            demo::DemoAction::DeleteRandom
        } else {
            match self.demo_seed % 3 {
                0 => demo::DemoAction::AddFile,
                1 => demo::DemoAction::ResizeRandom,
                _ => demo::DemoAction::DeleteRandom,
            }
        }
    }

    fn trigger_demo_action(&mut self, action: demo::DemoAction) {
        let Some(generation) = self.ui_scene.as_ref().map(|s| s.generation) else {
            return;
        };
        let seed = self.demo_seed;
        self.demo_seed = self.demo_seed.wrapping_add(1);
        if let Some(worker) = &self.worker
            && worker.current_generation() == generation
        {
            let _ = worker.tx.send(WorkerCmd::DemoAction {
                generation,
                action,
                seed,
            });
            self.worker_busy = true;
        }
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
        if self.demo {
            self.reset_demo();
            return;
        }
        if self.access != permission::Access::Granted {
            return;
        }
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            defer_drop(job);
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new(scan::ScanProgress::default()));
        let (tx, rx) = mpsc::channel();
        let (c, stats) = (cancel.clone(), progress.clone());
        std::thread::spawn(move || {
            let result = platform::scan_root().and_then(|root| scan::scan(root, c, stats));
            let _ = tx.send(result);
        });
        self.job = Some(ScanJob {
            cancel,
            progress,
            rx,
        });
        self.message = None;
    }

    fn start_watcher(&mut self, root: PathBuf, real_root: PathBuf, ctx: &egui::Context) {
        self.watcher = None;
        self.watch_rx = None;
        self.pending_watch_events.clear();
        self.watch_flush_at = None;
        if self.demo || root == Path::new(demo::VIRTUAL_ROOT) {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let wake_ctx = ctx.clone();
        if let Some(w) = watcher::FsWatcher::start(
            root,
            real_root,
            tx,
            Arc::new(move || wake_ctx.request_repaint()),
        ) {
            self.watcher = Some(w);
            self.watch_rx = Some(rx);
        }
    }

    fn submit_scan_result(
        &mut self,
        scan: scan::ScanResult,
        viewport: ViewportKey,
        ctx: &egui::Context,
    ) {
        let same_root = self.stats.as_ref().is_some_and(|s| s.root == scan.root)
            && self.ui_scene.as_ref().is_some_and(|u| u.mode == self.mode);
        let mode = self.mode;
        let bounds = self
            .ui_scene
            .as_ref()
            .filter(|_| same_root)
            .map_or_else(|| viewport.bounds(), |u| u.bounds);
        let physical_size = self
            .ui_scene
            .as_ref()
            .filter(|_| same_root)
            .map_or(viewport.physical_size, |u| u.physical_size);

        if !same_root {
            self.selected = None;
            self.pending_resize = None;
        }
        let worker = self.ensure_worker(ctx);
        let generation = if same_root {
            worker.current_generation()
        } else {
            worker.bump_generation()
        };
        let _ = worker.tx.send(WorkerCmd::LoadScan {
            generation,
            scan,
            mode,
            bounds,
            physical_size,
            reconcile: same_root,
        });
        self.worker_busy = true;
    }

    fn set_mode(&mut self, mode: ViewMode, ctx: &egui::Context) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.selected = None;
        if self.stats.is_none() {
            return;
        }
        let key = self
            .canvas_rect
            .map(|r| ViewportKey::new(r.size(), ctx.pixels_per_point()))
            .unwrap_or_else(|| ViewportKey::new(Vec2::new(1180.0, 700.0), ctx.pixels_per_point()));
        let worker = self.ensure_worker(ctx);
        let generation = worker.bump_generation();
        let _ = worker.tx.send(WorkerCmd::SwitchMode {
            generation,
            mode,
            bounds: key.bounds(),
            physical_size: key.physical_size,
        });
        self.worker_busy = true;
    }

    fn selected_tile(&self) -> Option<&TileInfo> {
        let id = self.selected?;
        self.ui_scene.as_ref()?.tiles.get(&id)
    }

    fn trash_selected(&mut self, id: NodeId) {
        if self.access != permission::Access::Granted || self.trash_rx.is_some() {
            return;
        }
        let Some((generation, tile)) = self
            .ui_scene
            .as_ref()
            .and_then(|u| u.tiles.get(&id).map(|t| (u.generation, t.clone())))
        else {
            return;
        };
        if self.demo {
            // Pure in-memory removal for animation testing: zero disk or Trash I/O.
            if let Some(worker) = &self.worker
                && worker.current_generation() == generation
            {
                let cmd = if tile.is_folder {
                    WorkerCmd::RemoveDirectory {
                        generation,
                        path: tile.path,
                        identity: tile.identity,
                    }
                } else {
                    WorkerCmd::RemoveFile {
                        generation,
                        path: tile.path,
                        identity: tile.identity,
                    }
                };
                let _ = worker.tx.send(cmd);
                self.worker_busy = true;
            }
            return;
        }
        let target_kind = if tile.is_folder {
            file_actions::TargetKind::Directory
        } else {
            file_actions::TargetKind::File
        };
        if !tile.can_trash || !file_actions::can_move_target(&tile.path, target_kind) {
            return;
        }
        let path = tile.path.clone();
        let identity = tile.identity;
        let is_folder = tile.is_folder;
        let (tx, rx) = mpsc::channel();
        self.trash_rx = Some(rx);
        self.action_notice = None;
        self.set_notice(Notice {
            text: format!("正在移到{}…", platform::TRASH_NAME),
            kind: NoticeKind::Info,
            expires: None,
        });
        rayon::spawn(move || {
            let result =
                file_actions::move_target_to_trash(&path, identity, target_kind).map(|_| ());
            let _ = tx.send(TrashResult {
                generation,
                path,
                identity,
                is_folder,
                result,
            });
        });
    }

    fn reveal(&mut self, id: NodeId) {
        if self.demo || self.reveal_rx.is_some() {
            return;
        }
        let Some(path) = self
            .ui_scene
            .as_ref()
            .and_then(|u| u.tiles.get(&id))
            .map(|t| t.path.clone())
        else {
            return;
        };
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
                Err(mpsc::TryRecvError::Empty) => {}
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
                            Ok(data) => {
                                self.pending_scan_result = Some(data);
                            }
                            Err(e) => self.message = Some(e),
                        }
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.job = None;
                    self.message = Some("扫描任务意外结束，请重新扫描。".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.trash_rx {
            match rx.try_recv() {
                Ok(output) => {
                    self.trash_rx = None;
                    self.message = None;
                    match output.result {
                        Ok(()) => {
                            if let Some(worker) = &self.worker
                                && worker.current_generation() == output.generation
                            {
                                let cmd = if output.is_folder {
                                    WorkerCmd::RemoveDirectory {
                                        generation: output.generation,
                                        path: output.path,
                                        identity: output.identity,
                                    }
                                } else {
                                    WorkerCmd::RemoveFile {
                                        generation: output.generation,
                                        path: output.path,
                                        identity: output.identity,
                                    }
                                };
                                let _ = worker.tx.send(cmd);
                                self.worker_busy = true;
                            }
                            self.action_notice = Some(format!("已移到{}。", platform::TRASH_NAME));
                        }
                        Err(error) => self.message = Some(error),
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

        // Drain FSEvents into debounce buffer.
        if let Some(rx) = &self.watch_rx {
            while let Ok(event) = rx.try_recv() {
                self.pending_watch_events.push(event);
                self.watch_flush_at = Some(Instant::now() + WATCH_DEBOUNCE);
            }
        }
        if !self.pending_watch_events.is_empty()
            && self.job.is_none()
            && self.trash_rx.is_none()
            && self
                .watch_flush_at
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.watch_flush_at = None;
            let events = std::mem::take(&mut self.pending_watch_events);
            if let Some(worker) = &self.worker {
                let generation = worker.current_generation();
                let _ = worker
                    .tx
                    .send(WorkerCmd::WatchEvents { generation, events });
                self.worker_busy = true;
            }
        }

        // Drain SceneWorker events and apply atomically to the UI frame.
        let now = ctx.input(|i| i.time);
        let mut new_watch_root: Option<(PathBuf, PathBuf)> = None;
        let mut need_rescan = false;
        if let Some(worker) = &self.worker {
            while let Ok(event) = worker.rx.try_recv() {
                match event {
                    WorkerEvent::FullFrame {
                        generation,
                        mode,
                        bounds,
                        patch,
                        tiles,
                        stats,
                        new_root,
                    } => {
                        if generation != worker.current_generation() {
                            defer_drop((patch, tiles));
                            continue;
                        }
                        self.worker_busy = false;
                        let owners = render::OwnerGrid::from_patch(bounds, &patch);
                        let physical_size = patch.size;
                        let texture = ctx.load_texture(
                            "disk-map",
                            patch.image,
                            egui::TextureOptions::NEAREST,
                        );
                        if new_root {
                            new_watch_root = Some((stats.root.clone(), stats.real_root.clone()));
                        }
                        self.stats = Some(stats);
                        if let Some(old) = self.ui_scene.replace(UiScene {
                            generation,
                            mode,
                            bounds,
                            physical_size,
                            texture,
                            owners,
                            tiles,
                            transitions: Vec::new(),
                        }) {
                            defer_drop(old);
                        }
                    }
                    WorkerEvent::Resized {
                        generation,
                        patch,
                        tiles,
                    } => {
                        if generation != worker.current_generation() {
                            defer_drop((patch, tiles));
                            continue;
                        }
                        self.worker_busy = false;
                        if let Some(scene) = self.ui_scene.as_mut()
                            && scene.generation == generation
                        {
                            scene.owners = render::OwnerGrid::from_patch(scene.bounds, &patch);
                            scene.physical_size = patch.size;
                            scene
                                .texture
                                .set(patch.image, egui::TextureOptions::NEAREST);
                            scene.tiles = tiles;
                        }
                    }
                    WorkerEvent::Patch {
                        generation,
                        patches,
                        removed_ids,
                        upserted_tiles,
                        transitions,
                        stats,
                    } => {
                        if generation != worker.current_generation() {
                            defer_drop((patches, upserted_tiles));
                            continue;
                        }
                        self.worker_busy = false;
                        self.stats = Some(stats);
                        if let Some(scene) = self.ui_scene.as_mut()
                            && scene.generation == generation
                        {
                            for patch in patches {
                                scene.owners.apply_patch(&patch);
                                scene.texture.set_partial(
                                    patch.origin,
                                    patch.image,
                                    egui::TextureOptions::NEAREST,
                                );
                            }
                            for id in &removed_ids {
                                scene.tiles.remove(id);
                                if self.selected == Some(*id) {
                                    self.selected = None;
                                }
                            }
                            for tile in upserted_tiles {
                                scene.tiles.insert(tile.id, tile);
                            }
                            if !transitions.items.is_empty() {
                                scene.transitions.push(ActiveTransition {
                                    batch: transitions,
                                    started: now,
                                });
                            }
                        }
                    }
                    WorkerEvent::RootChanged { generation } => {
                        if generation == worker.current_generation() {
                            self.worker_busy = false;
                            need_rescan = true;
                        }
                    }
                    WorkerEvent::Idle { generation } => {
                        if generation == worker.current_generation() {
                            self.worker_busy = false;
                        }
                    }
                }
            }
        }
        if let Some((root, real_root)) = new_watch_root {
            self.start_watcher(root, real_root, ctx);
        }
        if need_rescan && self.job.is_none() {
            self.launch_scan();
        }
        if self.demo
            && self.demo_auto
            && self.ui_scene.is_some()
            && !self.worker_busy
            && self
                .demo_next_tick
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let action = self.next_demo_auto_action();
            self.trigger_demo_action(action);
            self.demo_next_tick = Some(Instant::now() + DEMO_AUTO_INTERVAL);
        }
        if let Some(scene) = self.ui_scene.as_mut() {
            scene
                .transitions
                .retain(|tr| now - tr.started < TRANSITION_SECONDS);
        }
    }

    fn handle_resize(&mut self, key: ViewportKey, ctx: &egui::Context) {
        let target = render::capped_size(key.physical_size);
        let Some(scene) = self.ui_scene.as_ref() else {
            return;
        };
        if scene.physical_size == target {
            self.pending_resize = None;
            return;
        }
        let now = Instant::now();
        match self.pending_resize {
            Some((prev, _)) if prev == target => {}
            _ => {
                self.pending_resize = Some((target, now + RESIZE_DEBOUNCE));
            }
        }
        if let Some((size, deadline)) = self.pending_resize {
            if now >= deadline && !self.worker_busy {
                self.pending_resize = None;
                let generation = scene.generation;
                if let Some(worker) = &self.worker {
                    let _ = worker.tx.send(WorkerCmd::Resize {
                        generation,
                        physical_size: size,
                    });
                    self.worker_busy = true;
                }
            } else {
                ctx.request_repaint_after(deadline.saturating_duration_since(now));
            }
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
                        .stroke(egui::Stroke::new(1.0_f32, MUTED.gamma_multiply(0.3)))
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
                    if let Some(stats) = &self.stats {
                        egui::ScrollArea::vertical()
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            )
                            .max_height(360.0)
                            .show(ui, |ui| {
                                for error in &stats.errors {
                                    ui.add(egui::Label::new(error).wrap());
                                }
                            });
                    }
                });
            self.show_errors = open;
        }
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status")
            .exact_height(36.0)
            .frame(egui::Frame::new().fill(PANEL).inner_margin(8.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let status = if self.demo {
                        "虚拟演示 · 零磁盘文件 · 可用顶部按钮或删除选中项测试动画".to_owned()
                    } else if let Some(job) = &self.job {
                        let p = *job.progress.lock().unwrap_or_else(|e| e.into_inner());
                        format!(
                            "扫描中 · {} 个文件 · {} · 跳过 {} 项{}",
                            p.files,
                            bytes(p.bytes),
                            p.skipped,
                            if self.stats.is_some() {
                                " · 当前为上次扫描结果"
                            } else {
                                ""
                            }
                        )
                    } else if let Some(stats) = &self.stats {
                        let watch_tag = if self.watcher.is_some() {
                            " · 实时监控"
                        } else {
                            ""
                        };
                        format!(
                            "启动磁盘 · {:.1} 秒 · 跳过 {} 项{watch_tag}",
                            stats.elapsed.as_secs_f64(),
                            stats.skipped
                        )
                    } else {
                        "启动磁盘 · 扫描已停止".into()
                    };
                    ui.add(egui::Label::new(RichText::new(status).small().color(MUTED)).truncate());
                    if self.worker_busy {
                        ui.spinner();
                    }
                    if let Some(stats) = &self.stats {
                        let count = stats.errors.len();
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
            self.trash_selected(id);
        }
        let now = ctx.input(|i| i.time);
        let animating = self
            .ui_scene
            .as_ref()
            .is_some_and(|s| s.is_animating_at(now, None));
        if animating {
            ctx.request_repaint();
        } else if self.job.is_some()
            || self.reveal_rx.is_some()
            || self.trash_rx.is_some()
            || self.permission_rx.is_some()
            || self.worker_busy
            || self.watch_flush_at.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(80));
        } else if self.demo
            && self.demo_auto
            && let Some(deadline) = self.demo_next_tick
        {
            ctx.request_repaint_after(deadline.saturating_duration_since(Instant::now()));
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
        let mut next_mode = None;
        let mut demo_action = None;
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::new().fill(PANEL).inner_margin(16.0))
            .show(ctx, |ui| {
                let padding = ui.spacing().button_padding;
                let control_height =
                    ui.text_style_height(&egui::TextStyle::Button) + 2.0 * padding.y;
                ui.spacing_mut().interact_size.y = control_height;
                ui.horizontal(|ui| {
                    if self.demo {
                        ui.label(RichText::new("虚拟演示").strong().color(ACCENT))
                            .on_hover_text("纯内存模拟数据，不读写任何真实文件或废纸篓。");
                        if ui.button("重置演示").clicked() {
                            self.reset_demo();
                        }
                        if ui
                            .add_enabled(self.ui_scene.is_some(), egui::Button::new("随机新增"))
                            .clicked()
                        {
                            demo_action = Some(demo::DemoAction::AddFile);
                        }
                        if ui
                            .add_enabled(self.ui_scene.is_some(), egui::Button::new("随机改大小"))
                            .clicked()
                        {
                            demo_action = Some(demo::DemoAction::ResizeRandom);
                        }
                        if ui
                            .add_enabled(self.ui_scene.is_some(), egui::Button::new("随机删除"))
                            .clicked()
                        {
                            demo_action = Some(demo::DemoAction::DeleteRandom);
                        }
                        if ui
                            .add_enabled(
                                self.ui_scene.is_some(),
                                egui::Button::selectable(self.demo_auto, "自动演示"),
                            )
                            .on_hover_text("定时自动触发新增、改大小与删除以持续观察动画")
                            .clicked()
                        {
                            self.toggle_demo_auto();
                        }
                    } else {
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
                    }
                    if ui
                        .selectable_label(self.mode == ViewMode::Files, "文件")
                        .clicked()
                    {
                        next_mode = Some(ViewMode::Files);
                    }
                    if ui
                        .selectable_label(self.mode == ViewMode::Folders, "文件夹")
                        .on_hover_text("按文件夹汇总")
                        .clicked()
                    {
                        next_mode = Some(ViewMode::Folders);
                    }
                    if ui
                        .selectable_label(self.mode == ViewMode::Hybrid, "折中")
                        .clicked()
                    {
                        next_mode = Some(ViewMode::Hybrid);
                    }

                    let selected = self.selected_tile().cloned();
                    if !self.demo
                        && ui
                            .add_enabled(
                                selected.is_some() && self.reveal_rx.is_none(),
                                egui::Button::new(platform::REVEAL_LABEL),
                            )
                            .clicked()
                    {
                        reveal = selected.as_ref().map(|t| t.id);
                    }
                    let can_show_trash = self.demo
                        || selected.as_ref().is_none_or(|t| {
                            t.can_trash
                                && file_actions::can_move_target(
                                    &t.path,
                                    if t.is_folder {
                                        file_actions::TargetKind::Directory
                                    } else {
                                        file_actions::TargetKind::File
                                    },
                                )
                        });
                    let delete_label = if self.demo {
                        "删除选中项".to_owned()
                    } else {
                        format!("移到{}", platform::TRASH_NAME)
                    };
                    if can_show_trash
                        && ui
                            .add_enabled(
                                selected.is_some() && self.trash_rx.is_none(),
                                egui::Button::new(delete_label),
                            )
                            .clicked()
                    {
                        trash = selected.as_ref().map(|t| t.id);
                    }
                    if let Some(stats) = &self.stats {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(bytes(stats.total_bytes))
                                    .strong()
                                    .color(ACCENT),
                            )
                            .on_hover_text("文件逻辑大小");
                            ui.label(format!("{} 个文件", stats.total_files));
                        });
                    }
                });
            });

        if self.stats.is_none() && self.pending_scan_result.is_none() {
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
            if let Some(mode) = next_mode {
                self.set_mode(mode, ctx);
            }
            self.overlays(ctx);
            return;
        }

        self.status_bar(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(12.0))
            .show(ctx, |ui| {
                let viewport = ui.available_size().max(Vec2::splat(1.0));
                let key = ViewportKey::new(viewport, ctx.pixels_per_point());
                let (canvas, response) = ui.allocate_exact_size(viewport, Sense::click());
                self.canvas_rect = Some(canvas);

                if let Some(scan_data) = self.pending_scan_result.take() {
                    self.submit_scan_result(scan_data, key, ctx);
                }
                if let Some(mode) = next_mode {
                    self.set_mode(mode, ctx);
                }
                self.handle_resize(key, ctx);

                if self.stats.as_ref().is_some_and(|s| s.total_files == 0)
                    && self
                        .ui_scene
                        .as_ref()
                        .is_none_or(|s| s.transitions.is_empty())
                {
                    ui.painter_at(canvas).text(
                        canvas.center(),
                        egui::Align2::CENTER_CENTER,
                        "没有可显示的文件",
                        FontId::proportional(16.0),
                        MUTED,
                    );
                    return;
                }

                let painter = ui.painter_at(canvas);
                let pointer = tooltip_pointer(&response);
                let now = ctx.input(|i| i.time);
                let mut hovered: Option<NodeId> = None;

                if let Some(scene) = &self.ui_scene {
                    let scale = Vec2::new(
                        canvas.width() / scene.bounds.w.max(f32::EPSILON),
                        canvas.height() / scene.bounds.h.max(f32::EPSILON),
                    );
                    painter.image(
                        scene.texture.id(),
                        canvas,
                        Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                        Color32::WHITE,
                    );
                    if !scene.transitions.is_empty() {
                        paint_transitions(
                            &painter,
                            canvas,
                            scene.bounds,
                            scale,
                            &scene.transitions,
                            now,
                        );
                    }

                    if let Some(p) = pointer {
                        let wx = (p.x - canvas.min.x) / scale.x;
                        let wy = (p.y - canvas.min.y) / scale.y;
                        if !scene.is_animating_at(now, Some((wx, wy))) {
                            hovered = scene.owners.hit_test(wx, wy).map(NodeId);
                        }
                    }

                    for (id, width) in [(self.selected, 2.0_f32), (hovered, 1.5_f32)] {
                        if let Some(tile) = id.and_then(|i| scene.tiles.get(&i)) {
                            let r = tile.rect;
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

                    if let Some(id) = hovered
                        && let Some(tile) = scene.tiles.get(&id)
                    {
                        if response.clicked() {
                            self.selected = Some(id);
                        }
                        show_tile_tooltip(&response, tile);
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
            });

        if let Some(action) = demo_action {
            self.trigger_demo_action(action);
        }
        if let Some(id) = reveal {
            self.reveal(id);
        }
        if let Some(id) = trash {
            self.trash_selected(id);
        }
        self.overlays(ctx);
    }
}

fn paint_transitions(
    painter: &egui::Painter,
    canvas: Rect,
    bounds: treemap::Rect,
    scale: Vec2,
    transitions: &[ActiveTransition],
    now: f64,
) {
    let to_screen = |r: treemap::Rect| -> Rect {
        Rect::from_min_size(
            canvas.min + Vec2::new((r.x - bounds.x) * scale.x, (r.y - bounds.y) * scale.y),
            Vec2::new(r.w * scale.x, r.h * scale.y),
        )
    };
    let mut mesh = egui::Mesh::default();
    for tr in transitions {
        let raw_t = ((now - tr.started) / TRANSITION_SECONDS).clamp(0.0, 1.0) as f32;
        if raw_t >= 1.0 {
            continue;
        }
        // Cubic ease-out for smooth physical motion.
        let ease = 1.0 - (1.0 - raw_t).powi(3);
        // Keep moving layer opaque while tiles travel, then cross-fade smoothly
        // into the crisp hill-shaded raster over the final 30% of the duration.
        let fade_out_overlay = if raw_t < 0.70 {
            1.0
        } else {
            let u = ((raw_t - 0.70) / 0.30).clamp(0.0, 1.0);
            1.0 - u * u * (3.0 - 2.0 * u)
        };

        let bg_alpha = (fade_out_overlay * 235.0).round() as u8;
        let bg_color = Color32::from_rgba_unmultiplied(BG.r(), BG.g(), BG.b(), bg_alpha);
        for &dirty in &tr.batch.dirty_rects {
            let screen = to_screen(dirty).intersect(canvas);
            if screen.is_positive() {
                mesh.add_colored_rect(screen, bg_color);
            }
        }

        for item in &tr.batch.items {
            let (rect, alpha_mul) = match item.kind {
                TransitionKind::Move => (lerp_rect(item.from, item.to, ease), fade_out_overlay),
                TransitionKind::FadeOut => (
                    lerp_rect(item.from, item.to, ease),
                    (1.0 - ease) * fade_out_overlay,
                ),
                TransitionKind::FadeIn => {
                    let start = treemap::Rect {
                        x: item.to.x + item.to.w * 0.15,
                        y: item.to.y + item.to.h * 0.15,
                        w: item.to.w * 0.70,
                        h: item.to.h * 0.70,
                    };
                    (lerp_rect(start, item.to, ease), ease * fade_out_overlay)
                }
            };
            let screen = to_screen(rect).intersect(canvas);
            if screen.width() < 0.5 || screen.height() < 0.5 || alpha_mul <= 0.01 {
                continue;
            }
            add_shaded_quad(&mut mesh, screen, item.color, alpha_mul);
        }
    }
    if !mesh.is_empty() {
        painter.add(mesh);
    }
}

fn lerp_rect(a: treemap::Rect, b: treemap::Rect, t: f32) -> treemap::Rect {
    treemap::Rect {
        x: a.x + (b.x - a.x) * t,
        y: a.y + (b.y - a.y) * t,
        w: (a.w + (b.w - a.w) * t).max(0.0),
        h: (a.h + (b.h - a.h) * t).max(0.0),
    }
}

/// Push one quad whose 4 corners match `render.rs`'s top-left-to-bottom-right
/// directional shading so GPU motion blends seamlessly into the final raster.
fn add_shaded_quad(mesh: &mut egui::Mesh, rect: Rect, base: Color32, alpha_mul: f32) {
    let a = (alpha_mul.clamp(0.0, 1.0) * 255.0).round() as u8;
    let tint = |shade: f64| -> Color32 {
        let c = render::shaded(base, shade as f32);
        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
    };
    let tl = tint(render::FILE_SHADE_LIGHT);
    let tr = tint(render::FILE_SHADE_LIGHT - render::FILE_SHADE_X);
    let bl = tint(render::FILE_SHADE_LIGHT - render::FILE_SHADE_Y);
    let br = tint(render::FILE_SHADE_LIGHT - render::FILE_SHADE_X - render::FILE_SHADE_Y);
    let base_idx = mesh.vertices.len() as u32;
    let uv = Pos2::ZERO;
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.left_top(),
        uv,
        color: tl,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.right_top(),
        uv,
        color: tr,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.right_bottom(),
        uv,
        color: br,
    });
    mesh.vertices.push(egui::epaint::Vertex {
        pos: rect.left_bottom(),
        uv,
        color: bl,
    });
    mesh.indices.extend_from_slice(&[
        base_idx,
        base_idx + 1,
        base_idx + 2,
        base_idx,
        base_idx + 2,
        base_idx + 3,
    ]);
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
        self.watcher = None;
        if let Some(scene) = self.ui_scene.take() {
            defer_drop(scene);
        }
    }
}

fn native_options() -> eframe::Result<eframe::NativeOptions> {
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
    let demo = std::env::args_os().any(|arg| arg == "--demo")
        || (cfg!(target_os = "macos")
            && std::env::current_exe()
                .ok()
                .and_then(|path| path.file_name().map(|s| s == "visualize-ssd-demo"))
                .unwrap_or(false));
    eframe::run_native(
        if demo {
            "Visualize SSD · 演示"
        } else {
            "Visualize SSD"
        },
        native_options()?,
        Box::new(move |cc| {
            configure(&cc.egui_ctx);
            let mut app = App::default();
            app.demo = demo;
            app.start();
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> scan::ScanResult {
        let root = PathBuf::from("/disk");
        scan::ScanResult {
            root: root.clone(),
            files: vec![
                scan::FileEntry {
                    path: PathBuf::from("/disk/a/f1"),
                    parent: Arc::from(Path::new("/disk/a")),
                    size: 500,
                    identity: scan::FileIdentity {
                        device: 1,
                        inode: 10,
                    },
                    parent_identity: scan::FileIdentity {
                        device: 1,
                        inode: 2,
                    },
                    parent_is_leaf: true,
                    can_trash: true,
                },
                scan::FileEntry {
                    path: PathBuf::from("/disk/a/f2"),
                    parent: Arc::from(Path::new("/disk/a")),
                    size: 500,
                    identity: scan::FileIdentity {
                        device: 1,
                        inode: 11,
                    },
                    parent_identity: scan::FileIdentity {
                        device: 1,
                        inode: 2,
                    },
                    parent_is_leaf: true,
                    can_trash: true,
                },
                scan::FileEntry {
                    path: PathBuf::from("/disk/b/g1"),
                    parent: Arc::from(Path::new("/disk/b")),
                    size: 1000,
                    identity: scan::FileIdentity {
                        device: 1,
                        inode: 12,
                    },
                    parent_identity: scan::FileIdentity {
                        device: 1,
                        inode: 3,
                    },
                    parent_is_leaf: true,
                    can_trash: true,
                },
            ],
            directories: vec![
                scan::DirectoryFact {
                    path: root,
                    identity: scan::FileIdentity {
                        device: 1,
                        inode: 1,
                    },
                    enumeration_complete: true,
                    has_subdirectories: true,
                    excluded: false,
                },
                scan::DirectoryFact {
                    path: PathBuf::from("/disk/a"),
                    identity: scan::FileIdentity {
                        device: 1,
                        inode: 2,
                    },
                    enumeration_complete: true,
                    has_subdirectories: false,
                    excluded: false,
                },
                scan::DirectoryFact {
                    path: PathBuf::from("/disk/b"),
                    identity: scan::FileIdentity {
                        device: 1,
                        inode: 3,
                    },
                    enumeration_complete: true,
                    has_subdirectories: false,
                    excluded: false,
                },
            ]
            .into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        }
    }

    fn run_frame(app: &mut App, ctx: &egui::Context, raw: egui::RawInput) {
        let _ = ctx.run(raw, |ctx| app.ui(ctx));
    }

    fn settle(app: &mut App, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 600.0))),
                ..Default::default()
            };
            run_frame(app, ctx, input);
            if app.ui_scene.is_some() && !app.worker_busy && app.pending_scan_result.is_none() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("app did not settle in time");
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
        assert_eq!(options.viewport.maximized, Some(true));
    }

    #[test]
    fn default_view_is_hybrid_and_folder_size_uses_folder_label() {
        assert_eq!(App::default().mode, ViewMode::Hybrid);
        assert_eq!(tooltip_size(42, true), "文件夹 42 B");
        assert_eq!(tooltip_size(42, false), "42 B");
    }

    #[test]
    fn incremental_file_and_folder_removal_preserves_texture_and_animates() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.mode = ViewMode::Files;
        app.pending_scan_result = Some(snapshot());
        settle(&mut app, &ctx);

        let initial_tex = app.ui_scene.as_ref().unwrap().texture.id();
        let generation = app.ui_scene.as_ref().unwrap().generation;
        assert_eq!(app.stats.as_ref().unwrap().total_files, 3);

        // Remove one file -> texture handle is preserved (set_partial), transition created!
        app.worker
            .as_ref()
            .unwrap()
            .tx
            .send(WorkerCmd::RemoveFile {
                generation,
                path: PathBuf::from("/disk/a/f1"),
                identity: scan::FileIdentity {
                    device: 1,
                    inode: 10,
                },
            })
            .unwrap();
        app.worker_busy = true;
        settle(&mut app, &ctx);

        let scene = app.ui_scene.as_ref().unwrap();
        assert_eq!(scene.texture.id(), initial_tex);
        assert_eq!(app.stats.as_ref().unwrap().total_files, 2);
        assert!(!scene.transitions.is_empty());
    }

    #[test]
    fn mode_switch_and_same_root_rescan_work_incrementally() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.pending_scan_result = Some(snapshot());
        settle(&mut app, &ctx);
        // Hybrid mode: 2 leaf folders (/disk/a, /disk/b).
        assert_eq!(app.ui_scene.as_ref().unwrap().tiles.len(), 2);

        // Switch to Files mode -> 3 individual file tiles without rescanning.
        app.set_mode(ViewMode::Files, &ctx);
        settle(&mut app, &ctx);
        assert_eq!(app.ui_scene.as_ref().unwrap().tiles.len(), 3);

        // Same-root rescan with 1 file removed keeps same texture handle!
        let tex_before = app.ui_scene.as_ref().unwrap().texture.id();
        let mut updated = snapshot();
        updated.files.remove(0);
        app.pending_scan_result = Some(updated);
        settle(&mut app, &ctx);
        assert_eq!(app.ui_scene.as_ref().unwrap().texture.id(), tex_before);
        assert_eq!(app.stats.as_ref().unwrap().total_files, 2);
    }

    #[test]
    fn command_backspace_respects_protection_and_starts_folder_delete_without_confirmation() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.pending_scan_result = Some(snapshot());
        settle(&mut app, &ctx);

        // Select a leaf folder tile in Hybrid mode.
        let folder_id = app
            .ui_scene
            .as_ref()
            .unwrap()
            .tiles
            .values()
            .find(|t| t.is_folder)
            .unwrap()
            .id;
        app.selected = Some(folder_id);

        // Press Cmd+Backspace -> starts trash immediately (no confirmation dialog).
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 600.0))),
            events: vec![egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
            ..Default::default()
        };
        run_frame(&mut app, &ctx, input);
        assert!(app.trash_rx.is_some());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn live_fsevents_updates_scene_incrementally_without_replacing_texture() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("visualize-ssd-live-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/initial.bin"), vec![1u8; 4096]).unwrap();

        let scanned = scan::scan(
            dir.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(scan::ScanProgress::default())),
        )
        .unwrap();

        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.mode = ViewMode::Files;
        app.pending_scan_result = Some(scanned);
        settle(&mut app, &ctx);

        assert!(app.watcher.is_some());
        assert_eq!(app.stats.as_ref().unwrap().total_files, 1);
        let tex_before = app.ui_scene.as_ref().unwrap().texture.id();

        // Externally add a new file on disk and wait for FSEvents -> incremental patch!
        std::fs::write(dir.join("sub/added.bin"), vec![2u8; 8192]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 600.0))),
                ..Default::default()
            };
            run_frame(&mut app, &ctx, input);
            if app.stats.as_ref().is_some_and(|s| s.total_files == 2) && !app.worker_busy {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }

        let total_files = app.stats.as_ref().unwrap().total_files;
        let tex_after = app.ui_scene.as_ref().unwrap().texture.id();
        drop(app);
        let _ = std::process::Command::new("/usr/bin/trash")
            .arg(&dir)
            .status();

        assert_eq!(total_files, 2, "expected live FSEvents update to add file");
        assert_eq!(tex_after, tex_before, "expected incremental texture patch");
    }

    #[test]
    fn virtual_demo_animates_in_memory_without_disk_or_trash() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.demo = true;
        app.start();
        settle(&mut app, &ctx);

        assert!(app.watcher.is_none());
        let initial_files = app.stats.as_ref().unwrap().total_files;
        let tex_id = app.ui_scene.as_ref().unwrap().texture.id();
        assert!(initial_files > 800);

        // Delete a selected tile in Demo -> instant in-memory update, zero trash_rx!
        let tile_id = *app.ui_scene.as_ref().unwrap().tiles.keys().next().unwrap();
        app.trash_selected(tile_id);
        assert!(app.trash_rx.is_none());
        settle(&mut app, &ctx);
        assert!(app.stats.as_ref().unwrap().total_files < initial_files);
        assert_eq!(app.ui_scene.as_ref().unwrap().texture.id(), tex_id);
        assert!(!app.ui_scene.as_ref().unwrap().transitions.is_empty());

        // AddFile and ResizeRandom work in-memory on the same texture.
        let count_after_del = app.stats.as_ref().unwrap().total_files;
        app.trigger_demo_action(demo::DemoAction::AddFile);
        settle(&mut app, &ctx);
        assert_eq!(app.stats.as_ref().unwrap().total_files, count_after_del + 1);
        assert_eq!(app.ui_scene.as_ref().unwrap().texture.id(), tex_id);

        app.trigger_demo_action(demo::DemoAction::ResizeRandom);
        settle(&mut app, &ctx);
        assert_eq!(app.ui_scene.as_ref().unwrap().texture.id(), tex_id);

        app.trigger_demo_action(demo::DemoAction::DeleteRandom);
        settle(&mut app, &ctx);
        assert!(app.stats.as_ref().unwrap().total_files <= count_after_del);
        assert_eq!(app.ui_scene.as_ref().unwrap().texture.id(), tex_id);

        // Reset animates back to initial count on the same texture, and a no-op reset settles cleanly.
        app.reset_demo();
        settle(&mut app, &ctx);
        assert_eq!(app.stats.as_ref().unwrap().total_files, initial_files);
        assert_eq!(app.ui_scene.as_ref().unwrap().texture.id(), tex_id);
        app.reset_demo();
        settle(&mut app, &ctx);
        assert_eq!(app.stats.as_ref().unwrap().total_files, initial_files);
    }
}
