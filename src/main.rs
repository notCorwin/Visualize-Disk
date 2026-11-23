#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod deep_scope;
mod demo;
mod directory_map;
mod file_actions;
mod hybrid;
mod incremental_geometry;
mod incremental_layout;
mod incremental_update;
mod permission;
mod platform;
mod render;
#[cfg(test)]
mod root_aggregate_tests;
mod root_transaction;
mod scan;
mod scan_root_fold;
mod scene_actor;
mod scene_coordinator;
mod scene_journal;
mod scene_model;
mod snapshot_diff;
mod sparse_projection;
mod structural_split;
mod treemap;

use eframe::egui::{self, Color32, FontId, Rect, RichText, Sense, Vec2};
use rayon::prelude::*;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
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
        format!("文件夹 {}", bytes(size))
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
    id: Option<usize>,
    file_change: Option<scene_model::Change>,
    folder_hidden: Option<Arc<BTreeSet<usize>>>,
    result: Result<(), String>,
}

struct TotalsOutput {
    generation: u64,
    bytes: u64,
    incomplete: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ViewMode {
    Files,
    Folders,
    #[default]
    Hybrid,
}

impl ViewMode {
    fn index(self) -> usize {
        match self {
            Self::Files => 0,
            Self::Folders => 1,
            Self::Hybrid => 2,
        }
    }
    fn scene_view(self) -> scene_model::View {
        match self {
            Self::Files => scene_model::View::Files,
            Self::Folders => scene_model::View::Folders,
            Self::Hybrid => scene_model::View::Hybrid,
        }
    }
    fn from_scene(view: scene_model::View) -> Self {
        match view {
            scene_model::View::Files => Self::Files,
            scene_model::View::Folders => Self::Folders,
            scene_model::View::Hybrid => Self::Hybrid,
        }
    }
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
    let mut groups: HashMap<&std::path::Path, (u64, scan::FileIdentity, bool)> = HashMap::new();
    for (id, file) in data.files.iter().enumerate() {
        if id % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
            return None;
        }
        if !hidden.contains(&id) {
            let group =
                groups
                    .entry(file.parent.as_ref())
                    .or_insert((0, file.parent_identity, true));
            group.0 = group.0.saturating_add(file.size);
            group.2 &= group.1 == file.parent_identity && file.can_trash;
        }
    }
    // A directory tile only displays direct files, but deleting it also
    // affects all descendants. Never offer deletion when a descendant was
    // excluded or could not be enumerated completely.
    let mut unsafe_directories: HashSet<&std::path::Path> = HashSet::new();
    for (index, fact) in data.directories.iter().enumerate() {
        if index % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
            return None;
        }
        if fact.excluded || !fact.enumeration_complete {
            for ancestor in fact.path.ancestors() {
                if groups.contains_key(ancestor) {
                    unsafe_directories.insert(ancestor);
                }
                if ancestor == data.root {
                    break;
                }
            }
        }
    }
    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_unstable_by(|a, b| a.0.cmp(b.0));
    let files = groups
        .into_iter()
        .map(|(path, (size, identity, safe))| scan::FileEntry {
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: false,
            path: path.to_path_buf(),
            parent: Arc::from(path.parent().unwrap_or(path)),
            size,
            identity,
            can_trash: safe
                && !unsafe_directories.contains(path)
                && path != data.root
                && file_actions::can_move_target(path, file_actions::TargetKind::Directory),
        })
        .collect();
    (token.load(Ordering::Relaxed) == generation).then(|| scan::ScanResult {
        root: data.root.clone(),
        files,
        directories: data.directories.clone(),
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
    holes: Vec<treemap::Rect>,
    partitions: Option<Vec<directory_map::DirectoryPartition>>,
    previous: Option<PreparedFrame>,
}
#[derive(Clone)]
struct ResizeStamp {
    mode: ViewMode,
    texture: egui::TextureId,
    frame_revision: u64,
    holes: Arc<Vec<treemap::Rect>>,
    size: [usize; 2],
}
impl ResizeStamp {
    fn matches(&self, other: &Self) -> bool {
        self.mode == other.mode
            && self.texture == other.texture
            && self.frame_revision == other.frame_revision
            && Arc::ptr_eq(&self.holes, &other.holes)
            && self.size == other.size
    }
}
struct ResizeOutput {
    stamp: ResizeStamp,
    map: render::RenderMap,
}
struct PreparedFrame {
    texture: egui::TextureId,
    revision: u64,
    holes: Arc<Vec<treemap::Rect>>,
    image: egui::ColorImage,
}
// One generation owns geometry, pixels and hit testing together. A resize may
// stretch this entire cache while its replacement is prepared off-thread.
struct MapCache {
    generation: u64,
    revision: u64,
    frame_revision: u64,
    needs_rebuild: bool,
    key: LayoutKey,
    map: render::RenderMap,
    texture: egui::TextureHandle,
    // Pixel-aligned holes reuse the existing texture without a full upload.
    holes: Arc<Vec<treemap::Rect>>,
    partitions: Option<Arc<Vec<directory_map::DirectoryPartition>>>,
    projection: Option<Arc<sparse_projection::SparseProjection>>,
    geometry: Option<Arc<incremental_geometry::IncrementalGeometry>>,
    recovery_projection: Option<Arc<sparse_projection::SparseProjection>>,
    transition: Option<Vec<MapTransition>>,
    full_transition: Option<FullFrameTransition>,
}

#[derive(Clone)]
struct MapTransition {
    atlas: Option<egui::TextureHandle>,
    background: Option<egui::TextureHandle>,
    previous: egui::TextureHandle,
    origin: [usize; 2],
    size: [usize; 2],
    started: f64,
    tiles: Vec<incremental_update::TransitionTile>,
}

// A retained frame contains no lookup table: it is displayed only, never hit
// tested, after the authoritative snapshot or layout has been replaced.
struct RetainedFrame {
    texture: egui::TextureHandle,
    revision: u64,
    holes: Arc<Vec<treemap::Rect>>,
    bounds: treemap::Rect,
    map: render::RenderMap,
    mode: ViewMode,
}

struct FullFrameTransition {
    previous: RetainedFrame,
    started: f64,
}

const TRANSITION_SECONDS: f64 = 0.24;

// Bake old deletion masks off-thread before cross-fading a rebuilt frame.
// The source geometry and ownership are immutable snapshots of the displayed
// version; deleted entries are excluded before rasterization.
fn prepare_previous_frame(
    texture: egui::TextureId,
    revision: u64,
    holes: Arc<Vec<treemap::Rect>>,
    map: render::RenderMap,
    token: &AtomicU64,
    generation: u64,
) -> Option<PreparedFrame> {
    if holes.is_empty() {
        return None;
    }
    let mut entries = Vec::new();
    for chunk in map.entries().pages() {
        if token.load(Ordering::Relaxed) != generation {
            return None;
        }
        entries.extend(
            chunk
                .iter()
                .copied()
                .filter(|entry| map.rect(entry.id).is_some()),
        );
    }
    let size = map.physical_size();
    let bounds = map.bounds;
    let mut patch = render::raster_patch(
        &entries,
        map.platforms(),
        bounds,
        size,
        [0, 0, size[0], size[1]],
        token,
        generation,
    )?;
    let sx = size[0] as f32 / bounds.w;
    let sy = size[1] as f32 / bounds.h;
    for (i, rect) in holes.iter().enumerate() {
        if i % 1024 == 0 && token.load(Ordering::Relaxed) != generation {
            return None;
        }
        let x0 = (((rect.x - bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(size[0]);
        let y0 = (((rect.y - bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(size[1]);
        let x1 =
            ((((rect.x + rect.w) - bounds.x) * sx - 0.5).ceil().max(0.0) as usize).min(size[0]);
        let y1 =
            ((((rect.y + rect.h) - bounds.y) * sy - 0.5).ceil().max(0.0) as usize).min(size[1]);
        patch.mask_background([x0, y0, x1, y1], BG);
    }
    Some(PreparedFrame {
        texture,
        revision,
        holes,
        image: patch.image,
    })
}

fn paint_retained_frame(
    painter: &egui::Painter,
    canvas: Rect,
    frame: &RetainedFrame,
    opacity: f32,
) {
    let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    let tint = Color32::from_white_alpha((opacity.clamp(0.0, 1.0) * 255.0).round() as u8);
    painter.image(frame.texture.id(), canvas, uv, tint);
    let scale = Vec2::new(
        canvas.width() / frame.bounds.w.max(f32::EPSILON),
        canvas.height() / frame.bounds.h.max(f32::EPSILON),
    );
    let mut old_background = egui::Mesh::default();
    for hole in frame.holes.iter() {
        let area = Rect::from_min_size(
            canvas.min
                + Vec2::new(
                    (hole.x - frame.bounds.x) * scale.x,
                    (hole.y - frame.bounds.y) * scale.y,
                ),
            Vec2::new(hole.w * scale.x, hole.h * scale.y),
        )
        .intersect(canvas);
        if !area.is_positive() {
            continue;
        }
        // While waiting for the background bake, retain exactly the current
        // displayed holes. Full-frame fades use the baked texture (no holes).
        old_background.add_colored_rect(area, BG);
    }
    if !old_background.is_empty() {
        painter.add(old_background);
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
    let incomplete = data
        .directories
        .par_iter()
        .any(|fact| !fact.excluded && !fact.enumeration_complete);
    (token.load(Ordering::Relaxed) == generation).then_some(TotalsOutput {
        generation,
        bytes,
        incomplete,
    })
}

#[cfg(test)]
fn layout_snapshot(
    data: &scan::ScanResult,
    key: LayoutKey,
    token: &AtomicU64,
    generation: u64,
) -> Option<render::RenderMap> {
    layout_snapshot_excluding(data, key, token, generation, &BTreeSet::new(), false)
        .map(|(map, _)| map)
}

fn layout_snapshot_excluding(
    data: &scan::ScanResult,
    key: LayoutKey,
    token: &AtomicU64,
    generation: u64,
    excluded: &BTreeSet<usize>,
    retain_partitions: bool,
) -> Option<(
    render::RenderMap,
    Option<Vec<directory_map::DirectoryPartition>>,
)> {
    let grouped: Result<HashMap<&std::path::Path, Vec<(usize, u64)>>, ()> = data
        .files
        .par_iter()
        .enumerate()
        .filter(|(i, _)| !excluded.contains(i))
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
    let groups = grouped.ok()?;
    let pixel_size = render::logical_pixel_size(bounds, key.physical_size);
    let layout = if retain_partitions {
        directory_map::layout_with_partitions(
            groups, &data.root, bounds, pixel_size, token, generation,
        )?
    } else {
        directory_map::layout(groups, &data.root, bounds, pixel_size, token, generation)?
    };
    let partitions = retain_partitions.then_some(layout.partitions);
    let map = render::RenderMap::build_with_slots(
        layout.entries,
        layout.platforms,
        bounds,
        key.physical_size,
        data.files.len(),
        token,
        generation,
    )?;
    Some((map, partitions))
}

struct Job {
    cancel: Arc<AtomicBool>,
    progress: Arc<Mutex<scan::ScanProgress>>,
    rx: mpsc::Receiver<Result<scan::ScanResult, String>>,
    mutation_epoch: u64,
}
/// Stable endpoints for a chunked hidden-set reconciliation. Mutations arriving
/// while this batch runs remain in `hidden` and are submitted as a later batch.
struct HiddenBatch {
    before: Arc<BTreeSet<usize>>,
    after: Arc<BTreeSet<usize>>,
    cursor: Option<usize>,
}
struct QueuedSceneChanges {
    payload: QueuedScenePayload,
    /// A leaf removal also consumes its source-file tombstones. Reconcile to
    /// `before` first, then advance to `after` only when that transaction commits.
    hidden: Option<(Arc<BTreeSet<usize>>, Arc<BTreeSet<usize>>)>,
}
struct ScanSummary {
    files: usize,
    elapsed: Duration,
    skipped: u64,
    errors: Vec<String>,
    reconciled: bool,
    incomplete: Option<bool>,
}
impl From<&scan::ScanResult> for ScanSummary {
    fn from(data: &scan::ScanResult) -> Self {
        Self {
            files: data.files.len(),
            elapsed: data.elapsed,
            skipped: data.skipped,
            errors: data.errors.clone(),
            reconciled: false,
            incomplete: None,
        }
    }
}
enum QueuedScenePayload {
    Explicit(Vec<scene_model::Change>),
    Reconcile(Arc<scan::ScanResult>),
}
struct App {
    demo: bool,
    demo_root: Option<demo::DemoRoot>,
    demo_rx: Option<mpsc::Receiver<Result<demo::DemoRoot, String>>>,
    access: permission::Access,
    permission_rx: Option<mpsc::Receiver<permission::Access>>,
    pending_scan: bool,
    data: Option<Arc<scan::ScanResult>>,
    files_view_data: Option<Arc<scan::ScanResult>>,
    scan_summary: Option<ScanSummary>,
    scan_totals_source: Option<Arc<scan::ScanResult>>,
    mode: ViewMode,
    folder_data: Option<Arc<scan::ScanResult>>,
    folder_rx: Option<mpsc::Receiver<FolderOutput>>,
    folder_generation: Arc<AtomicU64>,
    hybrid_data: Option<hybrid::HybridSnapshot>,
    hybrid_rx: Option<mpsc::Receiver<(u64, hybrid::HybridSnapshot)>>,
    projection_dirty: bool,
    projection_hidden: [Arc<BTreeSet<usize>>; 3],
    coordinator: Option<scene_coordinator::Coordinator>,
    coordinator_version: Option<scene_actor::Version>,
    coordinator_source: Option<Arc<scan::ScanResult>>,
    coordinator_journal: Option<scene_journal::Journal>,
    journal_checkpoint_pending: bool,
    coordinator_epoch: u64,
    coordinator_disabled: bool,
    coordinator_failures: u8,
    coordinator_bound: [bool; 3],
    coordinator_init_pending: [bool; 3],
    coordinator_export_pending: [bool; 3],
    coordinator_update_pending: bool,
    coordinator_hidden: Arc<BTreeSet<usize>>,
    coordinator_hidden_batch: Option<HiddenBatch>,
    pending_scene_changes: std::collections::VecDeque<QueuedSceneChanges>,
    inflight_scene_changes: Option<QueuedSceneChanges>,
    scene_source_dirty: bool,
    hidden: Arc<BTreeSet<usize>>,
    scene_mutation_epoch: u64,
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
    resize_generation: Arc<AtomicU64>,
    resize_target: Option<(ResizeStamp, Instant)>,
    resize_rx: Option<mpsc::Receiver<ResizeOutput>>,
    resize_failures: u8,
    resize_failed: Option<ResizeStamp>,
    map_cache: Option<MapCache>,
    mode_caches: [Option<MapCache>; 3],
    retained_frame: Option<RetainedFrame>,
    retained_empty_since: Option<f64>,
}
impl Default for App {
    fn default() -> Self {
        Self {
            demo: false,
            demo_root: None,
            demo_rx: None,
            access: permission::Access::Unverified,
            permission_rx: None,
            pending_scan: false,
            data: None,
            files_view_data: None,
            scan_summary: None,
            scan_totals_source: None,
            mode: ViewMode::default(),
            folder_data: None,
            folder_rx: None,
            folder_generation: Arc::new(AtomicU64::new(0)),
            hybrid_data: None,
            hybrid_rx: None,
            projection_dirty: false,
            projection_hidden: Default::default(),
            coordinator: None,
            coordinator_version: None,
            coordinator_source: None,
            coordinator_journal: None,
            journal_checkpoint_pending: false,
            coordinator_epoch: 0,
            coordinator_disabled: false,
            coordinator_failures: 0,
            coordinator_bound: [false; 3],
            coordinator_init_pending: [false; 3],
            coordinator_export_pending: [false; 3],
            coordinator_update_pending: false,
            coordinator_hidden: Arc::default(),
            coordinator_hidden_batch: None,
            pending_scene_changes: std::collections::VecDeque::new(),
            inflight_scene_changes: None,
            scene_source_dirty: false,
            hidden: Arc::default(),
            scene_mutation_epoch: 0,
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
            resize_generation: Arc::new(AtomicU64::new(0)),
            resize_target: None,
            resize_rx: None,
            resize_failures: 0,
            resize_failed: None,
            map_cache: None,
            mode_caches: [None, None, None],
            retained_frame: None,
            retained_empty_since: None,
        }
    }
}
impl App {
    fn start(&mut self) {
        // A manual retry must also re-enable the journal-backed worker. The
        // previously displayed scene remains authoritative until recovery and
        // the new scan are both reconciled.
        if self.coordinator_disabled {
            self.coordinator_disabled = false;
            self.coordinator_failures = 0;
        }
        if self.demo {
            self.access = permission::Access::Granted;
            if self.demo_root.is_some() {
                self.launch_scan();
            } else {
                self.prepare_demo();
            }
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.queue_permission_check(rx);
        std::thread::spawn(move || {
            let _ = tx.send(permission::check());
        });
    }
    fn prepare_demo(&mut self) {
        if self.demo_rx.is_some() || self.trash_rx.is_some() {
            return;
        }
        if let Some(job) = self.job.take() {
            job.cancel.store(true, Ordering::Relaxed);
            defer_drop(job);
        }
        let (tx, rx) = mpsc::channel();
        self.demo_rx = Some(rx);
        self.message = None;
        std::thread::spawn(move || {
            let _ = tx.send(demo::DemoRoot::create());
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
        let demo_root = self.demo_root.clone();
        let is_demo = self.demo;
        std::thread::spawn(move || {
            let result = if is_demo {
                demo_root
                    .ok_or_else(|| "演示目录尚未准备好。".to_owned())
                    .and_then(|demo| demo.scan(c, stats))
            } else {
                platform::scan_root().and_then(|root| scan::scan(root, c, stats))
            };
            let _ = tx.send(result);
        });
        self.job = Some(Job {
            cancel,
            progress,
            rx,
            mutation_epoch: self.scene_mutation_epoch,
        });
        self.message = None;
    }
    fn replace_snapshot(&mut self, data: scan::ScanResult) {
        self.scan_summary = Some(ScanSummary::from(&data));
        self.scan_totals_source = None;
        self.retain_visible_frame();
        if let Some(old) = self.coordinator.take() {
            defer_drop(old);
        }
        self.coordinator_version = None;
        self.coordinator_source = None;
        self.journal_checkpoint_pending = false;
        self.coordinator_journal = None;
        self.coordinator_epoch = self.coordinator_epoch.wrapping_add(1);
        self.coordinator_disabled = false;
        self.coordinator_failures = 0;
        self.coordinator_bound = [false; 3];
        self.coordinator_init_pending = [false; 3];
        self.coordinator_export_pending = [false; 3];
        self.files_view_data = None;
        self.coordinator_update_pending = false;
        self.coordinator_hidden = Arc::default();
        self.coordinator_hidden_batch = None;
        self.pending_scene_changes.clear();
        self.inflight_scene_changes = None;
        self.scene_source_dirty = false;
        self.projection_dirty = false;
        self.projection_hidden = Default::default();
        self.invalidate_folders();
        defer_drop(std::mem::take(&mut self.hidden));
        if let Some(old) = self.data.replace(Arc::new(data)) {
            defer_drop(old);
        }
        self.totals_generation.fetch_add(1, Ordering::Relaxed);
        self.layout_generation.fetch_add(1, Ordering::Relaxed);
        self.resize_generation.fetch_add(1, Ordering::Relaxed);
        if let Some(old) = self.totals_rx.take() {
            defer_drop(old);
        }
        if let Some(old) = self.layout_rx.take() {
            defer_drop(old);
        }
        if let Some(old) = self.map_cache.take() {
            defer_drop(old);
        }
        for cache in &mut self.mode_caches {
            if let Some(old) = cache.take() {
                defer_drop(old);
            }
        }
        self.total_bytes = 0;
        self.totals_ready = false;
        self.selected = None;
        self.layout_key = None;
    }
    fn trash(&mut self, id: usize) {
        if self.demo && self.demo_root.is_none() {
            return;
        }
        if self.is_folder_tile(id) {
            if self.trash_rx.is_some() {
                return;
            }
            if let (Some(source), Some(folder)) = (self.data.as_ref(), self.view_file(id)) {
                if self.access == permission::Access::Granted
                    && folder.can_trash
                    && file_actions::can_move_target(
                        &folder.path,
                        file_actions::TargetKind::Directory,
                    )
                {
                    self.trash_folder(source.clone(), folder.path.clone(), folder.identity);
                }
            }
            return;
        }
        let source_id = if let Some(projection) = self
            .map_cache
            .as_ref()
            .and_then(|cache| cache.projection.as_ref())
        {
            let Some(scene_model::TileId::File(entry)) = projection.id(id) else {
                return;
            };
            let Some(displayed) = projection.get(id) else {
                return;
            };
            self.data
                .as_ref()
                .and_then(|source| source.files.get(entry.slot as usize))
                .filter(|original| {
                    entry.generation == 0
                        && original.path == displayed.path
                        && original.identity == displayed.identity
                })
                .map(|_| entry.slot as usize)
        } else if self.mode == ViewMode::Hybrid {
            let Some(Some(id)) = self
                .hybrid_data
                .as_ref()
                .and_then(|data| data.source_ids.get(id))
            else {
                return;
            };
            Some(*id)
        } else {
            Some(id)
        };
        if self.access != permission::Access::Granted
            || self.trash_rx.is_some()
            || source_id.is_some_and(|id| self.hidden.contains(&id))
        {
            return;
        }
        let Some(source) = self.data.clone() else {
            return;
        };
        let Some(file) = self.view_file(id) else {
            return;
        };
        if !file.can_trash || !file_actions::can_move_to_trash(&file.path) {
            return;
        }
        let path = file.path.clone();
        let identity = file.identity;
        let demo_root = self.demo_root.clone();
        let file_change = source_id
            .is_none()
            .then(|| scene_model::Change::RemoveFile {
                path: path.clone(),
                identity,
            });
        let (tx, rx) = mpsc::channel();
        self.trash_rx = Some(rx);
        self.action_notice = None;
        self.set_notice(Notice {
            text: format!("正在移到{}…", platform::TRASH_NAME),
            kind: NoticeKind::Info,
            expires: None,
        });
        rayon::spawn(move || {
            let result = if let Some(root) = demo_root.as_ref() {
                file_actions::move_target_to_trash_guarded(
                    &path,
                    identity,
                    file_actions::TargetKind::File,
                    || root.allow_target(&path),
                )
            } else {
                file_actions::move_to_trash(&path, identity)
            }
            .map(|_| ());
            let _ = tx.send(TrashOutput {
                source,
                id: source_id,
                file_change,
                folder_hidden: None,
                result,
            });
        });
    }
    fn trash_folder(
        &mut self,
        source: Arc<scan::ScanResult>,
        path: std::path::PathBuf,
        identity: scan::FileIdentity,
    ) {
        if self.trash_rx.is_some()
            || (self.demo && self.demo_root.is_none())
            || self.access != permission::Access::Granted
            || !self
                .data
                .as_ref()
                .is_some_and(|data| Arc::ptr_eq(data, &source))
        {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.trash_rx = Some(rx);
        self.action_notice = None;
        self.set_notice(Notice {
            text: format!("正在移到{}…", platform::TRASH_NAME),
            kind: NoticeKind::Info,
            expires: None,
        });
        let hidden = self.hidden.clone();
        let demo_root = self.demo_root.clone();
        rayon::spawn(move || {
            // The visible directory may have appeared in a later reconciled
            // scene and therefore need not exist in the initial scan. Capture
            // its real parent's identity before the operation and re-enumerate
            // that same parent afterward; the scene actor checks it again.
            let parent_before = path.parent().and_then(|parent| {
                let metadata = std::fs::symlink_metadata(parent).ok()?;
                if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                    return None;
                }
                scan::FileIdentity::from_path(parent, &metadata).ok()
            });
            let result = if let Some(root) = demo_root.as_ref() {
                file_actions::move_target_to_trash_guarded(
                    &path,
                    identity,
                    file_actions::TargetKind::Directory,
                    || root.allow_target(&path),
                )
            } else {
                file_actions::move_target_to_trash(
                    &path,
                    identity,
                    file_actions::TargetKind::Directory,
                )
            }
            .map(|_| ());
            let folder_hidden = result.is_ok().then(|| {
                let mut updated = (*hidden).clone();
                updated.extend(
                    source
                        .files
                        .iter()
                        .enumerate()
                        .filter_map(|(id, f)| f.path.starts_with(&path).then_some(id)),
                );
                Arc::new(updated)
            });
            // A successful Trash operation confirms the old object moved,
            // even when its former path was immediately reused or the parent
            // is no longer readable. Retire only its captured scene identity;
            // a fresh scan reconciles any replacement and parent metadata.
            let file_change = folder_hidden.as_ref().map(|_| {
                let verified_parent = path.parent().and_then(|parent_path| {
                    if !matches!(std::fs::symlink_metadata(&path), Err(ref e) if e.kind() == std::io::ErrorKind::NotFound) { return None; }
                    scan::directory_fact_after_change(parent_path, parent_before?).ok()
                });
                match verified_parent {
                    Some(parent) => scene_model::Change::RemoveSubtreeDirectory { path: path.clone(), identity, parent },
                    None => scene_model::Change::RemoveConfirmedSubtree { path: path.clone(), identity },
                }
            });
            let _ = tx.send(TrashOutput {
                source,
                id: None,
                file_change,
                folder_hidden,
                result,
            });
        });
    }
    fn hide_folder(&mut self, hidden: Arc<BTreeSet<usize>>) {
        self.scene_mutation_epoch = self.scene_mutation_epoch.wrapping_add(1);
        defer_drop(std::mem::replace(&mut self.hidden, hidden));
        self.abandon_coordinator();
        self.begin_projection_update();
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
                    if let Some(data) = &self.data {
                        let errors = self
                            .scan_summary
                            .as_ref()
                            .map_or(data.errors.as_slice(), |s| s.errors.as_slice());
                        egui::ScrollArea::vertical()
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                            )
                            .max_height(360.0)
                            .show(ui, |ui| {
                                for error in errors {
                                    ui.add(egui::Label::new(error).wrap());
                                }
                            });
                    }
                });
            self.show_errors = open;
        }
    }
    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.demo_rx {
            match rx.try_recv() {
                Ok(Ok(root)) => {
                    self.demo_rx = None;
                    self.demo_root = Some(root);
                    self.access = permission::Access::Granted;
                    self.launch_scan();
                }
                Ok(Err(error)) => {
                    self.demo_rx = None;
                    self.message = Some(error);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.demo_rx = None;
                    self.message = Some("演示数据准备失败，请重置演示后重试。".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        self.poll_coordinator(ctx);
        // Coordinator may publish a new animation during this poll. Evaluate
        // after it runs so no later worker replaces that just-published frame.
        let animation_active = self.map_cache.as_ref().is_some_and(|cache| {
            let now = ctx.input(|input| input.time);
            cache
                .transition
                .as_ref()
                .is_some_and(|items| items.iter().any(|t| now - t.started < TRANSITION_SECONDS))
                || cache
                    .full_transition
                    .as_ref()
                    .is_some_and(|t| now - t.started < TRANSITION_SECONDS)
        });
        if animation_active {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
        if let Some(rx) = &self.folder_rx {
            match rx.try_recv() {
                Ok(output) => {
                    self.folder_rx = None;
                    if output.generation == self.folder_generation.load(Ordering::Relaxed) {
                        self.folder_data = Some(Arc::new(output.data));
                        self.projection_hidden[ViewMode::Folders.index()] = Arc::default();
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.folder_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &self.hybrid_rx {
            match rx.try_recv() {
                Ok((generation, data)) => {
                    self.hybrid_rx = None;
                    if generation == self.folder_generation.load(Ordering::Relaxed) {
                        self.hybrid_data = Some(data);
                        self.projection_hidden[ViewMode::Hybrid.index()] = Arc::default();
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.hybrid_rx = None,
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
        if !animation_active
            && self.trash_rx.is_none()
            && !self.projection_dirty
            && let Some(job) = &self.job
        {
            match job.rx.try_recv() {
                Ok(result) => {
                    let cancelled = job.cancel.load(Ordering::Relaxed);
                    let stale = job.mutation_epoch != self.scene_mutation_epoch;
                    self.job = None;
                    if cancelled {
                        defer_drop(result);
                        self.set_notice(Notice::new(
                            "已取消扫描，保留上次扫描结果。".into(),
                            NoticeKind::Info,
                        ));
                    } else if stale {
                        // The scan may have observed pre-mutation paths. Never
                        // publish it over a newer scene; take a fresh snapshot.
                        defer_drop(result);
                        self.launch_scan();
                    } else {
                        match result {
                            Ok(data) => {
                                if self
                                    .data
                                    .as_ref()
                                    .is_some_and(|current| current.root == data.root)
                                    && (self.map_cache.is_some()
                                        || self.mode_caches.iter().any(Option::is_some))
                                {
                                    let checkpoint = self.hidden.clone();
                                    self.pending_scene_changes.push_back(QueuedSceneChanges {
                                        payload: QueuedScenePayload::Reconcile(Arc::new(data)),
                                        hidden: Some((checkpoint.clone(), checkpoint)),
                                    });
                                    self.begin_projection_update();
                                } else {
                                    self.replace_snapshot(data);
                                }
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
                                let deleted_folder = output.folder_hidden.is_some();
                                let needs_reconcile = matches!(
                                    &output.file_change,
                                    Some(scene_model::Change::RemoveConfirmedSubtree { .. })
                                );
                                let folder_incremental =
                                    deleted_folder && output.file_change.is_some();
                                let scan_was_running = self.job.is_some();
                                if let Some(hidden) = output.folder_hidden {
                                    if let Some(change) = output.file_change {
                                        let before =
                                            std::mem::replace(&mut self.hidden, hidden.clone());
                                        self.queue_scene_changes_with_hidden(
                                            vec![change],
                                            before,
                                            hidden,
                                        );
                                    } else {
                                        self.hide_folder(hidden);
                                        self.launch_scan();
                                    }
                                } else if let Some(id) = output.id {
                                    self.hide_file(id);
                                } else if let Some(change) = output.file_change {
                                    self.queue_scene_changes(vec![change]);
                                }
                                // A running scan may have read the old path already.
                                if needs_reconcile
                                    || (scan_was_running && (!deleted_folder || folder_incremental))
                                {
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
                        if let Some(summary) = &mut self.scan_summary {
                            summary.incomplete = Some(output.incomplete);
                        }
                        self.scan_totals_source = None;
                    } else {
                        defer_drop(output);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.totals_rx = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if !animation_active && let Some(rx) = &self.layout_rx {
            match rx.try_recv() {
                Ok(mut output) => {
                    self.layout_rx = None;
                    if output.generation == self.layout_generation.load(Ordering::Relaxed)
                        && self.layout_key == Some(output.key)
                    {
                        if let Some(image) = output.map.image.take() {
                            // The old displayed raster stays alive until the new
                            // authoritative geometry, ownership and pixels arrive.
                            self.retain_visible_frame();
                            let mut previous =
                                self.retained_frame.take().filter(|f| f.mode == self.mode);
                            if let Some(frame) = &mut previous
                                && !frame.holes.is_empty()
                            {
                                if let Some(baked) = output.previous.take().filter(|b| {
                                    b.texture == frame.texture.id()
                                        && b.revision == frame.revision
                                        && Arc::ptr_eq(&b.holes, &frame.holes)
                                }) {
                                    frame.texture = ctx.load_texture(
                                        "transition-full-previous",
                                        baked.image,
                                        egui::TextureOptions::NEAREST,
                                    );
                                    frame.holes = Arc::default();
                                } else {
                                    // Rebuild from the latest displayed frame rather
                                    // than publishing a stale bake or dropping motion.
                                    self.retained_frame = previous;
                                    self.layout_generation.fetch_add(1, Ordering::Relaxed);
                                    defer_drop(image);
                                    defer_drop(output);
                                    ctx.request_repaint();
                                    return;
                                }
                            }
                            if previous.is_some() {
                                self.selected = None;
                            }
                            let cache = MapCache {
                                generation: output.generation,
                                revision: output.generation,
                                frame_revision: 0,
                                needs_rebuild: false,
                                key: output.key,
                                map: output.map,
                                texture: ctx.load_texture(
                                    "disk-map",
                                    image,
                                    egui::TextureOptions::NEAREST,
                                ),
                                holes: Arc::new(output.holes),
                                partitions: output.partitions.map(Arc::new),
                                projection: None,
                                geometry: None,
                                recovery_projection: None,
                                transition: None,
                                full_transition: previous.map(|previous| FullFrameTransition {
                                    previous,
                                    started: ctx.input(|input| input.time),
                                }),
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
    }
    fn poll_coordinator(&mut self, ctx: &egui::Context) {
        // A second prepared update is based on the current target geometry.
        // Let the previous animation reach that geometry before publishing it,
        // so a rapid second deletion cannot jump back to a later rectangle.
        if self.coordinator_update_pending
            && self.map_cache.as_ref().is_some_and(|cache| {
                let now = ctx.input(|input| input.time);
                cache
                    .transition
                    .as_ref()
                    .is_some_and(|items| items.iter().any(|t| now - t.started < TRANSITION_SECONDS))
                    || cache
                        .full_transition
                        .as_ref()
                        .is_some_and(|t| now - t.started < TRANSITION_SECONDS)
            })
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
            return;
        }
        loop {
            let event = match self.coordinator.as_ref().map(|c| c.try_recv()) {
                Some(Ok(event)) => event,
                Some(Err(mpsc::TryRecvError::Disconnected)) => {
                    self.abandon_coordinator();
                    return;
                }
                _ => return,
            };
            match event {
                scene_coordinator::Event::Ready(version) => {
                    if version.source_generation != self.coordinator_epoch {
                        self.abandon_coordinator();
                    } else {
                        self.coordinator_version = Some(version);
                    }
                }
                scene_coordinator::Event::ModeReady(ready) => {
                    let mode = ViewMode::from_scene(ready.mode);
                    self.coordinator_init_pending[mode.index()] = false;
                    if Some(ready.version) != self.coordinator_version {
                        continue;
                    }
                    let cache = if self.mode == mode {
                        self.map_cache.as_mut()
                    } else {
                        self.mode_caches[mode.index()].as_mut()
                    };
                    if let Some(cache) = cache.filter(|c| {
                        c.texture.id() == ready.texture && c.revision == ready.layout_generation
                    }) {
                        if self.mode == mode
                            && self
                                .selected
                                .is_some_and(|slot| ready.projection.get(slot).is_none())
                        {
                            self.selected = None;
                        }
                        cache.projection = Some(ready.projection);
                        cache.geometry = Some(ready.geometry);
                        self.coordinator_bound[mode.index()] = true;
                        cache.recovery_projection = None;
                        if let Some(parts) = cache.partitions.take() {
                            defer_drop(parts);
                        }
                    }
                }
                scene_coordinator::Event::ModeSnapshot {
                    base,
                    mode,
                    data,
                    source_ids,
                } => {
                    let mode = ViewMode::from_scene(mode);
                    self.coordinator_export_pending[mode.index()] = false;
                    if Some(base) != self.coordinator_version || self.projection_dirty {
                        continue;
                    }
                    match mode {
                        ViewMode::Files => self.files_view_data = Some(data),
                        ViewMode::Folders => self.folder_data = Some(data),
                        ViewMode::Hybrid => {
                            if let Some(source_ids) = source_ids {
                                self.hybrid_data =
                                    Some(hybrid::HybridSnapshot { data, source_ids });
                            }
                        }
                    }
                    self.projection_hidden[mode.index()] = Arc::default();
                    ctx.request_repaint();
                }
                scene_coordinator::Event::Checkpoint { base, checkpoint } => {
                    self.journal_checkpoint_pending = false;
                    // Retain the old shared history until a background drop;
                    // adopting a checkpoint must not free thousands of paths
                    // on the UI thread.
                    let previous = self.coordinator_journal.clone();
                    let accepted = self.coordinator_version == Some(base)
                        && self
                            .coordinator_journal
                            .as_mut()
                            .is_some_and(|journal| journal.adopt(checkpoint).is_ok());
                    if let Some(previous) = previous {
                        defer_drop(previous);
                    }
                    if !accepted {
                        self.abandon_coordinator();
                    } else {
                        ctx.request_repaint();
                    }
                }
                scene_coordinator::Event::Prepared(prepared) => {
                    self.coordinator_update_pending = false;
                    // Large hidden sets are reconciled in bounded commits. Do
                    // not serialize a full animation for every intermediate
                    // chunk; the final chunk animates the converged scene.
                    let animate = prepared.hidden_progress.as_ref().is_none_or(|p| p.done);
                    let expected_reconcile =
                        self.inflight_scene_changes.as_ref().is_some_and(|batch| {
                            matches!(batch.payload, QueuedScenePayload::Reconcile(_))
                        });
                    let valid = self.projection_dirty
                        && Some(prepared.base) == self.coordinator_version
                        && prepared.reconciled == expected_reconcile
                        && prepared.rebuild.is_empty()
                        && prepared.views.iter().all(|view| {
                            let mode = ViewMode::from_scene(view.mode);
                            let cache = if self.mode == mode {
                                self.map_cache.as_ref()
                            } else {
                                self.mode_caches[mode.index()].as_ref()
                            };
                            cache.is_some_and(|c| {
                                c.texture.id() == view.texture
                                    && c.revision == view.layout_generation
                            })
                        });
                    if !valid
                        || self
                            .coordinator_journal
                            .as_ref()
                            .is_none_or(|journal| journal.version() != prepared.base)
                        || self.coordinator.as_ref().is_none_or(|c| {
                            c.try_send(scene_coordinator::Command::Commit(prepared.version))
                                .is_err()
                        })
                    {
                        self.abandon_coordinator();
                        defer_drop(prepared);
                        continue;
                    }
                    if self.coordinator_journal.as_mut().is_none_or(|journal| {
                        journal
                            .append(prepared.base, prepared.version, prepared.journal.clone())
                            .is_err()
                    }) {
                        self.abandon_coordinator();
                        defer_drop(prepared);
                        continue;
                    }
                    for view in prepared.views {
                        let mode = ViewMode::from_scene(view.mode);
                        let cache = if self.mode == mode {
                            self.map_cache.as_mut()
                        } else {
                            self.mode_caches[mode.index()].as_mut()
                        };
                        if let Some(cache) = cache {
                            cache.transition = if animate
                                && self.mode == mode
                                && !view.transitions.is_empty()
                            {
                                Some(
                                    view.transitions
                                        .into_iter()
                                        .map(|transition| {
                                            let size = transition.previous.size;
                                            MapTransition {
                                                atlas: transition.atlas.map(|image| {
                                                    ctx.load_texture(
                                                        "transition-tiles",
                                                        image,
                                                        egui::TextureOptions::NEAREST,
                                                    )
                                                }),
                                                background: transition.background.map(|image| {
                                                    ctx.load_texture(
                                                        "transition-background",
                                                        image,
                                                        egui::TextureOptions::NEAREST,
                                                    )
                                                }),
                                                previous: ctx.load_texture(
                                                    "transition-previous",
                                                    transition.previous,
                                                    egui::TextureOptions::NEAREST,
                                                ),
                                                origin: transition.origin,
                                                size,
                                                started: ctx.input(|input| input.time),
                                                tiles: transition.tiles,
                                            }
                                        })
                                        .collect(),
                                )
                            } else {
                                None
                            };
                            for patch in view.patches {
                                cache.texture.set_partial(
                                    patch.origin,
                                    patch.image,
                                    egui::TextureOptions::NEAREST,
                                );
                            }
                            defer_drop(std::mem::replace(&mut cache.map, view.map));
                            defer_drop(std::mem::replace(&mut cache.holes, view.holes));
                            cache.frame_revision = cache.frame_revision.wrapping_add(1);
                            cache.projection = Some(view.projection);
                            cache.geometry = Some(view.geometry);
                            self.coordinator_bound[mode.index()] = true;
                        }
                    }
                    if let Some(progress) = prepared.hidden_progress {
                        if let Some(batch) = &mut self.coordinator_hidden_batch {
                            batch.cursor = progress.cursor;
                            if progress.done {
                                self.coordinator_hidden = batch.after.clone();
                                self.coordinator_hidden_batch = None;
                            }
                        }
                    } else if let Some((_, after)) = self
                        .inflight_scene_changes
                        .as_ref()
                        .and_then(|batch| batch.hidden.as_ref())
                    {
                        self.coordinator_hidden = after.clone();
                    }
                    self.coordinator_version = Some(prepared.version);
                    self.coordinator_failures = 0;
                    if let Some(batch) = self.inflight_scene_changes.take() {
                        if let QueuedScenePayload::Reconcile(snapshot) = batch.payload {
                            let mut summary = ScanSummary::from(snapshot.as_ref());
                            summary.reconciled = true;
                            self.scan_summary = Some(summary);
                            self.scan_totals_source = Some(snapshot);
                            self.totals_generation.fetch_add(1, Ordering::Relaxed);
                            if let Some(old) = self.totals_rx.take() {
                                defer_drop(old);
                            }
                            self.totals_ready = false;
                        }
                        self.scene_source_dirty = true;
                    }
                    self.projection_dirty = self.coordinator_hidden_batch.is_some()
                        || !self.pending_scene_changes.is_empty()
                        || !Arc::ptr_eq(&self.coordinator_hidden, &self.hidden);
                    if self.projection_dirty {
                        ctx.request_repaint();
                    }
                }
                scene_coordinator::Event::Failed(reason) => {
                    #[cfg(test)]
                    eprintln!("scene coordinator failure: {reason:?}");
                    if let scene_coordinator::Failure::Reconcile(problem) = &reason {
                        let text = match problem {
                            snapshot_diff::Error::RootChanged => {
                                Some("扫描位置已被替换，已保留当前画面；请确认位置后重新扫描。")
                            }
                            snapshot_diff::Error::InvalidSnapshot => {
                                Some("扫描结果不完整或存在冲突，已保留当前画面；请重新扫描。")
                            }
                            snapshot_diff::Error::UncertainReplacement => {
                                Some("无法确认部分路径是否被替换；请检查访问权限后重新扫描。")
                            }
                            snapshot_diff::Error::Cancelled => None,
                        };
                        if let Some(text) = text {
                            // Planning rejected the input before actor mutation;
                            // retrying the identical snapshot cannot help.
                            self.coordinator_update_pending = false;
                            if let Some(batch) = self.inflight_scene_changes.take() {
                                defer_drop(batch);
                            }
                            self.projection_dirty = self.coordinator_hidden_batch.is_some()
                                || !self.pending_scene_changes.is_empty()
                                || !Arc::ptr_eq(&self.coordinator_hidden, &self.hidden);
                            self.message = Some(text.into());
                            continue;
                        }
                    }
                    if self.projection_dirty {
                        let text = match reason {
                            scene_coordinator::Failure::Stale
                            | scene_coordinator::Failure::Pending => {
                                "视图状态已变化，正在重新更新。"
                            }
                            scene_coordinator::Failure::Recovery(_) => {
                                "无法恢复视图状态，正在重试；已保留当前画面。"
                            }
                            _ => "局部更新未完成，正在重试；已保留当前画面。",
                        };
                        self.set_notice(Notice::new(text.into(), NoticeKind::Info));
                    }
                    self.abandon_coordinator();
                }
            }
        }
    }
    fn hide_file(&mut self, id: usize) {
        if self.data.as_ref().is_none_or(|data| id >= data.files.len())
            || !Arc::make_mut(&mut self.hidden).insert(id)
        {
            return;
        }
        self.scene_mutation_epoch = self.scene_mutation_epoch.wrapping_add(1);
        if self.mode == ViewMode::Files && self.selected == Some(id) {
            self.selected = None;
        }
        self.begin_projection_update();
    }
    fn begin_projection_update(&mut self) {
        self.folder_generation.fetch_add(1, Ordering::Relaxed);
        if let Some(old) = self.folder_rx.take() {
            defer_drop(old);
        }
        if let Some(old) = self.hybrid_rx.take() {
            defer_drop(old);
        }
        self.projection_dirty = true;
        self.selected = None;
        // Keep an in-flight transaction bound to its original snapshot. New
        // changes are queued and reconciled after that transaction commits.
        if self.layout_rx.take().is_some() {
            let generation = self.layout_generation.fetch_add(1, Ordering::Relaxed) + 1;
            if let Some(cache) = &mut self.map_cache {
                cache.generation = generation;
            }
        }
    }
    /// Queue authoritative changes from a future filesystem reconciler. The
    /// old frame remains bound to its old slot mapping until commit.
    fn queue_scene_changes(&mut self, changes: Vec<scene_model::Change>) {
        if changes.is_empty() {
            return;
        }
        self.scene_mutation_epoch = self.scene_mutation_epoch.wrapping_add(1);
        self.pending_scene_changes.push_back(QueuedSceneChanges {
            payload: QueuedScenePayload::Explicit(changes),
            hidden: None,
        });
        self.begin_projection_update();
    }
    fn queue_scene_changes_with_hidden(
        &mut self,
        changes: Vec<scene_model::Change>,
        before: Arc<BTreeSet<usize>>,
        after: Arc<BTreeSet<usize>>,
    ) {
        if changes.is_empty() {
            return;
        }
        self.scene_mutation_epoch = self.scene_mutation_epoch.wrapping_add(1);
        self.pending_scene_changes.push_back(QueuedSceneChanges {
            payload: QueuedScenePayload::Explicit(changes),
            hidden: Some((before, after)),
        });
        self.begin_projection_update();
    }
    fn abandon_coordinator(&mut self) {
        if let Some(coordinator) = self.coordinator.take() {
            defer_drop(coordinator);
        }
        self.coordinator_version = None;
        self.coordinator_source = None;
        self.coordinator_failures = self.coordinator_failures.saturating_add(1);
        self.coordinator_disabled = self.coordinator_failures >= 3;
        self.coordinator_bound = [false; 3];
        self.coordinator_init_pending = [false; 3];
        self.coordinator_export_pending = [false; 3];
        self.coordinator_update_pending = false;
        self.journal_checkpoint_pending = false;
        self.coordinator_hidden_batch = None;
        if let Some(changes) = self.inflight_scene_changes.take() {
            self.pending_scene_changes.push_front(changes);
        }
        if self.coordinator_disabled {
            self.message = Some("视图更新多次失败；原画面已保留，请点击重新扫描重试。".into());
        }
    }

    fn prepare_coordinator(&mut self, ctx: &egui::Context) {
        if self.coordinator_disabled {
            return;
        }
        let Some(source) = self.data.clone() else {
            return;
        };
        if self
            .coordinator_source
            .as_ref()
            .is_some_and(|prior| !Arc::ptr_eq(prior, &source))
        {
            self.abandon_coordinator();
            self.coordinator_disabled = false;
        }
        if self.coordinator.is_none() {
            self.coordinator_source = Some(source.clone());
            let journal = self
                .coordinator_journal
                .get_or_insert_with(|| {
                    self.coordinator_hidden = Arc::default();
                    scene_journal::Journal::new(
                        source.clone(),
                        self.coordinator_epoch,
                        self.coordinator_hidden.clone(),
                    )
                })
                .clone();
            self.coordinator = Some(scene_coordinator::Coordinator::recover(
                source,
                self.coordinator_epoch,
                journal,
                ctx.clone(),
            ));
            return;
        }
        let Some(version) = self.coordinator_version else {
            return;
        };
        if self.coordinator_update_pending {
            return;
        }
        if self.journal_checkpoint_pending {
            return;
        }
        if self
            .coordinator_journal
            .as_ref()
            .is_some_and(|j| j.needs_checkpoint())
        {
            if let (Some(coordinator), Some(base)) = (&self.coordinator, self.coordinator_version) {
                if coordinator
                    .try_send(scene_coordinator::Command::Checkpoint { base })
                    .is_ok()
                {
                    self.journal_checkpoint_pending = true;
                }
            }
            return;
        }
        for mode in [ViewMode::Files, ViewMode::Folders, ViewMode::Hybrid] {
            let index = mode.index();
            if self.coordinator_init_pending[index] {
                continue;
            }
            let cache = if self.mode == mode {
                self.map_cache.as_ref()
            } else {
                self.mode_caches[index].as_ref()
            };
            let Some(cache) = cache else {
                continue;
            };
            if self.coordinator_bound[index] || cache.needs_rebuild {
                continue;
            }
            let retained = cache
                .geometry
                .clone()
                .zip(
                    cache
                        .projection
                        .clone()
                        .or_else(|| cache.recovery_projection.clone()),
                )
                .map(|(geometry, projection)| scene_coordinator::Retained {
                    geometry,
                    projection,
                });
            let partitions = cache.partitions.clone();
            if retained.is_none() && partitions.is_none() {
                continue;
            }
            let data = match mode {
                ViewMode::Files if self.scene_source_dirty => self
                    .files_view_data
                    .clone()
                    .or_else(|| cache.projection.as_ref().map(|p| p.base().clone())),
                ViewMode::Files => self.data.clone(),
                ViewMode::Folders => self.folder_data.clone(),
                ViewMode::Hybrid => self.hybrid_data.as_ref().map(|h| h.data.clone()),
            };
            let Some(data) = data else {
                continue;
            };
            let source_ids = if mode == ViewMode::Hybrid {
                self.hybrid_data
                    .as_ref()
                    .map(|h| h.source_ids.clone().into())
            } else {
                None
            };
            let input = scene_coordinator::ModeInput {
                base: version,
                mode: mode.scene_view(),
                data,
                source_ids,
                partitions,
                retained,
                layout_generation: cache.revision,
                texture: cache.texture.id(),
                map: cache.map.snapshot_for_update(),
                tombstones: if mode == ViewMode::Files && self.files_view_data.is_some() {
                    Arc::default()
                } else if mode == ViewMode::Files {
                    self.coordinator_hidden.clone()
                } else {
                    self.projection_hidden[index].clone()
                },
            };
            if self.coordinator.as_ref().is_some_and(|c| {
                c.try_send(scene_coordinator::Command::InitMode(input))
                    .is_ok()
            }) {
                self.coordinator_init_pending[index] = true;
            }
        }
    }

    fn try_coordinator_update(&mut self) -> bool {
        if self.journal_checkpoint_pending {
            return true;
        }
        if self.coordinator_update_pending {
            return true;
        }
        let Some(version) = self.coordinator_version else {
            return false;
        };
        if self.coordinator_init_pending.iter().any(|pending| *pending) {
            return false;
        }
        let mut modes = Vec::new();
        for mode in [ViewMode::Files, ViewMode::Folders, ViewMode::Hybrid] {
            let cache = if self.mode == mode {
                self.map_cache.as_ref()
            } else {
                self.mode_caches[mode.index()].as_ref()
            };
            if let Some(cache) = cache {
                if cache.projection.is_none() {
                    return false;
                }
                let [width, _] = cache.map.physical_size();
                modes.push(scene_coordinator::ModeBase {
                    mode: mode.scene_view(),
                    layout_generation: cache.revision,
                    texture: cache.texture.id(),
                    map: cache.map.snapshot_for_update(),
                    holes: cache.holes.clone(),
                    pixels_per_point: width as f32 / cache.map.bounds.w.max(1.0),
                    area_tolerance: 8192.0,
                    max_patch_pixels: 1_048_576,
                });
            }
        }
        if modes.is_empty() {
            return false;
        }
        // Reconcile all pending hidden source entries before applying explicit
        // directory changes. This keeps the hidden checkpoint authoritative
        // even when unrelated removals arrive during a directory operation.
        let target_hidden = self
            .pending_scene_changes
            .front()
            .and_then(|batch| batch.hidden.as_ref().map(|(before, _)| before))
            .unwrap_or(&self.hidden)
            .clone();
        let is_explicit = self.coordinator_hidden_batch.is_none()
            && Arc::ptr_eq(&self.coordinator_hidden, &target_hidden)
            && !self.pending_scene_changes.is_empty();
        let changes = if is_explicit {
            match &self.pending_scene_changes.front().unwrap().payload {
                QueuedScenePayload::Explicit(changes) => {
                    scene_coordinator::Changes::Explicit(changes.clone())
                }
                QueuedScenePayload::Reconcile(snapshot) => {
                    scene_coordinator::Changes::Reconcile(snapshot.clone())
                }
            }
        } else {
            let batch = self
                .coordinator_hidden_batch
                .get_or_insert_with(|| HiddenBatch {
                    before: self.coordinator_hidden.clone(),
                    after: target_hidden,
                    cursor: None,
                });
            scene_coordinator::Changes::HiddenDiff {
                before: batch.before.clone(),
                after: batch.after.clone(),
                cursor: batch.cursor,
            }
        };
        let input = scene_coordinator::UpdateInput {
            base: version,
            changes,
            modes,
        };
        if self.coordinator.as_ref().is_some_and(|c| {
            c.try_send(scene_coordinator::Command::Update(input))
                .is_ok()
        }) {
            if is_explicit {
                self.inflight_scene_changes = self.pending_scene_changes.pop_front();
            }
            self.coordinator_update_pending = true;
            true
        } else {
            false
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
        if let Some(rx) = self.hybrid_rx.take() {
            defer_drop(rx);
        }
        if let Some(data) = self.hybrid_data.take() {
            defer_drop(data);
        }
        if self.mode != ViewMode::Files {
            self.reset_layout();
        }
        for mode in [ViewMode::Folders, ViewMode::Hybrid] {
            if let Some(cache) = self.mode_caches[mode.index()].take() {
                defer_drop(cache);
            }
        }
    }
    fn reset_layout(&mut self) {
        self.retain_visible_frame();
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
            self.retained_frame = None;
            self.retained_empty_since = None;
            let stale_target = self.scene_source_dirty
                && self.mode_caches[mode.index()]
                    .as_ref()
                    .is_none_or(|cache| cache.projection.is_none());
            self.mode_caches[self.mode.index()] = self.map_cache.take();
            self.mode = mode;
            self.layout_generation.fetch_add(1, Ordering::Relaxed);
            if let Some(rx) = self.layout_rx.take() {
                defer_drop(rx);
            }
            self.selected = None;
            self.map_cache = self.mode_caches[mode.index()].take();
            let generation = self.layout_generation.load(Ordering::Relaxed);
            self.layout_key = self.map_cache.as_mut().map(|cache| {
                cache.generation = generation;
                cache.key
            });
            if stale_target {
                if let Some(old) = self.map_cache.take() {
                    defer_drop(old);
                }
                self.layout_key = None;
                match mode {
                    ViewMode::Files => self.files_view_data = None,
                    ViewMode::Folders => self.folder_data = None,
                    ViewMode::Hybrid => self.hybrid_data = None,
                }
            }
        }
    }
    fn retain_visible_frame(&mut self) {
        if let Some(cache) = self.map_cache.as_ref() {
            self.retained_frame = Some(RetainedFrame {
                texture: cache.texture.clone(),
                revision: cache.frame_revision,
                holes: cache.holes.clone(),
                bounds: cache.map.bounds,
                map: cache.map.snapshot_for_update(),
                mode: self.mode,
            });
            self.retained_empty_since = None;
        }
    }
    fn view_data(&self) -> Option<&Arc<scan::ScanResult>> {
        match self.mode {
            ViewMode::Files if self.scene_source_dirty => {
                self.files_view_data.as_ref().or_else(|| {
                    self.map_cache
                        .as_ref()
                        .and_then(|cache| cache.projection.as_ref())
                        .and(self.data.as_ref())
                })
            }
            ViewMode::Files => self.data.as_ref(),
            ViewMode::Folders => self.folder_data.as_ref(),
            ViewMode::Hybrid => self.hybrid_data.as_ref().map(|data| &data.data),
        }
    }
    fn view_file(&self, slot: usize) -> Option<&scan::FileEntry> {
        // A reconciled scene no longer shares the original scan's slot map.
        // Until this view has its authoritative sparse projection, never use
        // the baseline vector to resolve an action or tooltip.
        if self.scene_source_dirty
            && self
                .map_cache
                .as_ref()
                .is_none_or(|cache| cache.projection.is_none())
        {
            return None;
        }
        if let Some(projection) = self
            .map_cache
            .as_ref()
            .and_then(|cache| cache.projection.as_ref())
        {
            return projection.get(slot);
        }
        self.view_data()?.files.get(slot)
    }
    fn is_folder_tile(&self, id: usize) -> bool {
        if let Some(projection) = self
            .map_cache
            .as_ref()
            .and_then(|cache| cache.projection.as_ref())
        {
            return matches!(projection.id(id), Some(scene_model::TileId::Directory(_)));
        }
        match self.mode {
            ViewMode::Files => false,
            ViewMode::Folders => true,
            ViewMode::Hybrid => self
                .hybrid_data
                .as_ref()
                .and_then(|data| data.source_ids.get(id))
                .is_some_and(Option::is_none),
        }
    }
    fn prepare_folders(&mut self, ctx: &egui::Context) {
        if self.coordinator_disabled && self.projection_dirty {
            return;
        }
        if self.scene_source_dirty && self.coordinator_disabled && !self.projection_dirty {
            return;
        }
        if self.projection_dirty {
            // A coordinator can be starting, rebinding cached modes or working
            // through queued changes. Keep the published scene and wait for
            // its wake-up; never discard it for a full projection rebuild.
            self.try_coordinator_update();
            if !self.coordinator_disabled {
                // Worker completion wakes the UI. This bounded timer covers a
                // saturated command queue without a continuous busy redraw.
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            return;
        }
        if self.scene_source_dirty && self.view_data().is_none() {
            let index = self.mode.index();
            if !self.coordinator_export_pending[index] && !self.coordinator_update_pending {
                if let (Some(coordinator), Some(base)) =
                    (&self.coordinator, self.coordinator_version)
                {
                    if coordinator
                        .try_send(scene_coordinator::Command::ExportMode {
                            base,
                            mode: self.mode.scene_view(),
                        })
                        .is_ok()
                    {
                        self.coordinator_export_pending[index] = true;
                    }
                }
            }
            return;
        }
        if self.mode == ViewMode::Hybrid {
            if self.hybrid_data.is_some() || self.hybrid_rx.is_some() {
                return;
            }
            let Some(data) = self.data.clone() else {
                return;
            };
            let hidden = self.hidden.clone();
            let token = self.folder_generation.clone();
            let generation = token.load(Ordering::Relaxed);
            let (tx, rx) = mpsc::channel();
            self.hybrid_rx = Some(rx);
            let ctx = ctx.clone();
            rayon::spawn(move || {
                if let Some(data) = hybrid::build(&data, &hidden, &token, generation) {
                    let _ = tx.send((generation, data));
                }
                ctx.request_repaint();
            });
            return;
        }
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
        let Some(data) = self
            .scan_totals_source
            .clone()
            .or_else(|| self.data.clone())
        else {
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
    /// Keep the existing world-space layout visible while rebuilding only its
    /// physical pixel grid in the background after the viewport settles.
    fn update_resize(&mut self, key: LayoutKey, ctx: &egui::Context) {
        let Some(cache) = self.map_cache.as_ref() else {
            return;
        };
        let size = render::capped_size(key.physical_size);
        if cache.map.physical_size() == size {
            if self.resize_target.take().is_some() || self.resize_rx.is_some() {
                self.resize_generation.fetch_add(1, Ordering::Relaxed);
                self.resize_rx = None;
            }
            self.resize_failures = 0;
            self.resize_failed = None;
            return;
        }
        let stamp = ResizeStamp {
            mode: self.mode,
            texture: cache.texture.id(),
            frame_revision: cache.frame_revision,
            holes: cache.holes.clone(),
            size,
        };
        if self
            .resize_failed
            .as_ref()
            .is_some_and(|failed| failed.matches(&stamp))
        {
            return;
        }
        if self
            .resize_target
            .as_ref()
            .is_none_or(|(old, _)| !old.matches(&stamp))
        {
            self.resize_generation.fetch_add(1, Ordering::Relaxed);
            self.resize_rx = None;
            self.resize_target = Some((stamp.clone(), Instant::now()));
            self.resize_failures = 0;
            self.resize_failed = None;
        }
        let now = ctx.input(|input| input.time);
        let animation_active = cache.transition.as_ref().is_some_and(|items| {
            items
                .iter()
                .any(|transition| now - transition.started < TRANSITION_SECONDS)
        }) || cache
            .full_transition
            .as_ref()
            .is_some_and(|transition| now - transition.started < TRANSITION_SECONDS);
        // A coordinator result may still reference the current physical grid.
        // Wait for it to publish, then reject/restart if its frame has changed.
        let busy = animation_active
            || self.coordinator_update_pending
            || self.coordinator_init_pending.iter().any(|pending| *pending)
            || self.projection_dirty
            || self.job.is_some();
        if !busy && let Some(rx) = &self.resize_rx {
            match rx.try_recv() {
                Ok(mut output) => {
                    self.resize_rx = None;
                    if output.stamp.matches(&stamp)
                        && let Some(image) = output.map.image.take()
                        && let Some(cache) = self.map_cache.as_mut()
                    {
                        // Texture identity remains stable for the coordinator;
                        // pixels and owner map become visible in the same frame.
                        cache.texture.set(image, egui::TextureOptions::NEAREST);
                        defer_drop(std::mem::replace(&mut cache.map, output.map));
                        cache.frame_revision = cache.frame_revision.wrapping_add(1);
                        cache.key.physical_size = key.physical_size;
                        self.layout_key = Some(cache.key);
                        self.resize_target = None;
                        self.resize_failures = 0;
                        self.resize_failed = None;
                        ctx.request_repaint();
                    }
                    return;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.resize_rx = None;
                    self.resize_failures += 1;
                    if self.resize_failures < 2 {
                        self.resize_target = Some((stamp, Instant::now()));
                        ctx.request_repaint_after(Duration::from_millis(120));
                    } else {
                        self.resize_target = None;
                        self.resize_failed = Some(stamp);
                        self.message = Some("视图分辨率更新失败，请调整窗口大小后重试。".into());
                    }
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => return,
            }
        }
        if busy {
            ctx.request_repaint_after(Duration::from_millis(32));
            return;
        }
        let Some((_, since)) = self.resize_target.as_ref() else {
            return;
        };
        let delay = Duration::from_millis(120);
        if since.elapsed() < delay {
            ctx.request_repaint_after(delay.saturating_sub(since.elapsed()));
            return;
        }
        let snapshot = cache.map.snapshot_for_update();
        let generation = self.resize_generation.load(Ordering::Relaxed);
        let token = self.resize_generation.clone();
        let (tx, rx) = mpsc::channel();
        self.resize_rx = Some(rx);
        let ctx = ctx.clone();
        rayon::spawn(move || {
            if let Some(map) = snapshot.resample_tiled(size, &stamp.holes, &token, generation) {
                let _ = tx.send(ResizeOutput { stamp, map });
            }
            drop(tx);
            ctx.request_repaint();
        });
    }

    fn layout(&mut self, size: Vec2, pixels_per_point: f32, ctx: &egui::Context) {
        let key = LayoutKey::new(size, pixels_per_point);
        // Geometry and owner pixels use a stable world coordinate system. The
        // canvas maps that world to its current rectangle for both drawing and
        // hit testing, so resizing can reuse the same coherent scene instead of
        // sorting and laying out the entire snapshot again. Pixel resampling is
        // done separately in the background after the viewport settles.
        // `cache.key` describes the retained texture, not the current viewport.
        if self.layout_rx.is_none()
            && let Some(cache) = self.map_cache.as_ref()
            && !cache.needs_rebuild
            && cache.generation == self.layout_generation.load(Ordering::Relaxed)
        {
            self.layout_key = Some(cache.key);
            self.update_resize(key, ctx);
            return;
        }
        if self.layout_key != Some(key) {
            // A sparse projection may contain appended/tombstoned slots not in
            // the original scan vector. Reconcile it before a full resize
            // until sparse-layout rebuilding is available.
            if self
                .map_cache
                .as_ref()
                .is_some_and(|cache| cache.projection.is_some())
            {
                self.abandon_coordinator();
                self.begin_projection_update();
            }
            self.layout_key = Some(key);
            self.layout_generation.fetch_add(1, Ordering::Relaxed);
        }
        if self.projection_dirty {
            return;
        }
        if self.scene_source_dirty && self.coordinator_disabled {
            return;
        }
        let generation = self.layout_generation.load(Ordering::Relaxed);
        if self.layout_rx.is_some()
            || self.map_cache.as_ref().is_some_and(|cache| {
                cache.generation == generation && cache.key == key && !cache.needs_rebuild
            })
        {
            return;
        }
        let Some(data) = self.view_data().cloned() else {
            return;
        };
        let token = self.layout_generation.clone();
        let excluded = self.projection_hidden[self.mode.index()].clone();
        // Every generated view needs its layout partitions so subsequent
        // updates can be reconciled without discarding the current image.
        let keep = true;
        let hidden = if self.mode == ViewMode::Files && self.files_view_data.is_none() {
            Some(self.hidden.clone())
        } else {
            None
        };
        let old_frame = self
            .map_cache
            .as_ref()
            .map(|cache| {
                (
                    cache.texture.id(),
                    cache.frame_revision,
                    cache.holes.clone(),
                    cache.map.snapshot_for_update(),
                )
            })
            .or_else(|| {
                self.retained_frame
                    .as_ref()
                    .filter(|f| f.mode == self.mode)
                    .map(|f| {
                        (
                            f.texture.id(),
                            f.revision,
                            f.holes.clone(),
                            f.map.snapshot_for_update(),
                        )
                    })
            })
            .filter(|(_, _, holes, _)| !holes.is_empty());
        let (tx, rx) = mpsc::channel();
        self.layout_rx = Some(rx);
        let ctx = ctx.clone();
        rayon::spawn(move || {
            if let Some((mut map, partitions)) =
                layout_snapshot_excluding(&data, key, &token, generation, &excluded, keep)
            {
                let holes = match hidden {
                    Some(hidden) if !hidden.is_empty() => {
                        let ids: Vec<_> = hidden.iter().copied().collect();
                        let Some(holes) = map.erase_many(&ids, &token, generation) else {
                            ctx.request_repaint();
                            return;
                        };
                        holes
                    }
                    _ => Vec::new(),
                };
                let previous = old_frame.and_then(|(texture, revision, holes, map)| {
                    prepare_previous_frame(texture, revision, holes, map, &token, generation)
                });
                if token.load(Ordering::Relaxed) != generation {
                    drop(tx);
                    ctx.request_repaint();
                    return;
                }
                let _ = tx.send(LayoutOutput {
                    generation,
                    key,
                    map,
                    holes,
                    partitions,
                    previous,
                });
            }
            drop(tx);
            ctx.request_repaint();
        });
    }
    fn reveal(&mut self, i: usize) {
        if self.reveal_rx.is_some() || (self.demo && self.demo_root.is_none()) {
            return;
        }
        let Some(file) = self.view_file(i) else {
            return;
        };
        let source_id = if let Some(projection) = self
            .map_cache
            .as_ref()
            .and_then(|cache| cache.projection.as_ref())
        {
            match projection.id(i) {
                Some(scene_model::TileId::File(id)) if id.generation == 0 => Some(id.slot as usize),
                _ => None,
            }
        } else if self.mode == ViewMode::Files {
            Some(i)
        } else if self.mode == ViewMode::Hybrid {
            self.hybrid_data
                .as_ref()
                .and_then(|data| data.source_ids.get(i))
                .copied()
                .flatten()
        } else {
            None
        };
        if source_id.is_some_and(|id| self.hidden.contains(&id)) {
            self.message = Some("文件已移到废纸篓，请等待视图更新。".into());
            return;
        }
        let path = file.path.clone();
        let identity = file.identity;
        let demo_root = self.demo_root.clone();
        let kind = if self.is_folder_tile(i) {
            file_actions::TargetKind::Directory
        } else {
            file_actions::TargetKind::File
        };
        let (tx, rx) = mpsc::channel();
        self.reveal_rx = Some(rx);
        std::thread::spawn(move || {
            let result = demo_root
                .as_ref()
                .map_or(Ok(()), |root| root.allow_target(&path))
                .and_then(|_| file_actions::verify_target(&path, identity, kind))
                .and_then(|_| platform::reveal(&path));
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
                    let status = if self.demo_rx.is_some() {
                        if self.data.is_some() {
                            "正在准备演示… · 当前为上次结果".into()
                        } else {
                            "正在准备演示…".into()
                        }
                    } else if let Some(job) = &self.job {
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
                    } else if self.projection_dirty && self.coordinator_disabled {
                        "视图尚未更新 · 请重新扫描".into()
                    } else if let Some(data) = &self.data {
                        let elapsed = self
                            .scan_summary
                            .as_ref()
                            .map_or(data.elapsed, |s| s.elapsed);
                        let skipped = self
                            .scan_summary
                            .as_ref()
                            .map_or(data.skipped, |s| s.skipped);
                        format!(
                            "{} · {:.1} 秒 · 跳过 {} 项{}",
                            if self.demo {
                                "演示目录"
                            } else {
                                "启动磁盘"
                            },
                            elapsed.as_secs_f64(),
                            skipped,
                            if self.scan_summary.as_ref().is_some_and(|s| s.reconciled
                                && (s.incomplete == Some(true) || !s.errors.is_empty()))
                            {
                                " · 部分路径无法确认，保留上次结果"
                            } else {
                                ""
                            }
                        )
                    } else {
                        format!(
                            "{} · 扫描已停止",
                            if self.demo {
                                "演示目录"
                            } else {
                                "启动磁盘"
                            }
                        )
                    };
                    ui.add(egui::Label::new(RichText::new(status).small().color(MUTED)).truncate());
                    if self.projection_dirty && !self.coordinator_disabled {
                        ui.label(RichText::new("正在更新视图…").small().color(MUTED));
                    }
                    if self.layout_rx.is_some() {
                        ui.spinner();
                    }
                    if let Some(data) = &self.data {
                        let count = self
                            .scan_summary
                            .as_ref()
                            .map_or(data.errors.len(), |s| s.errors.len());
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
            || self.demo_rx.is_some()
            || self.reveal_rx.is_some()
            || self.trash_rx.is_some()
            || self.permission_rx.is_some()
            || self.totals_rx.is_some()
            || self.layout_rx.is_some()
            || self.folder_rx.is_some()
            || self.hybrid_rx.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
    fn ui_contents(&mut self, ctx: &egui::Context) {
        self.canvas_rect = None;
        self.poll(ctx);
        self.prepare_coordinator(ctx);
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
                    if self.demo {
                        ui.label(RichText::new("演示模式").strong().color(ACCENT))
                            .on_hover_text("演示文件使用稀疏空间，显示的是逻辑大小。");
                        if ui
                            .add_enabled(
                                self.demo_rx.is_none() && self.trash_rx.is_none(),
                                egui::Button::new("重置演示"),
                            )
                            .on_hover_text("创建新的演示数据；原有数据和废纸篓内容不会自动删除。")
                            .clicked()
                        {
                            self.prepare_demo();
                        }
                    }
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
                        .on_hover_text("按文件夹汇总")
                        .clicked()
                    {
                        self.set_mode(ViewMode::Folders);
                    }
                    if ui
                        .selectable_label(self.mode == ViewMode::Hybrid, "折中")
                        .clicked()
                    {
                        self.set_mode(ViewMode::Hybrid);
                    }
                    let selected = self
                        .selected
                        .and_then(|i| self.view_file(i).map(|f| (i, f)));
                    if ui
                        .add_enabled(
                            selected.is_some() && self.reveal_rx.is_none(),
                            egui::Button::new(platform::REVEAL_LABEL),
                        )
                        .clicked()
                    {
                        reveal = selected.map(|(i, _)| i);
                    }
                    if selected.is_none_or(|(id, f)| {
                        f.can_trash
                            && file_actions::can_move_target(
                                &f.path,
                                if self.is_folder_tile(id) {
                                    file_actions::TargetKind::Directory
                                } else {
                                    file_actions::TargetKind::File
                                },
                            )
                    }) && ui
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
                            let count = self
                                .scan_summary
                                .as_ref()
                                .map_or(data.files.len(), |s| s.files);
                            ui.label(format!("{} 个文件", count));
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
                        let heading = if self.demo_rx.is_some() {
                            "正在准备演示…"
                        } else if self.job.is_some() {
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
                .frame(egui::Frame::new().fill(BG).inner_margin(12.0))
                .show(ctx, |ui| {
                    if let Some(frame) =
                        self.retained_frame.as_ref().filter(|f| f.mode == self.mode)
                    {
                        let canvas = ui.max_rect();
                        self.canvas_rect = Some(canvas);
                        paint_retained_frame(ui.painter(), canvas, frame, 1.0);
                        return;
                    }
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            if self.scene_source_dirty
                                && self.coordinator_disabled
                                && self.job.is_none()
                            {
                                "视图尚未更新，请重新扫描。"
                            } else {
                                "正在生成文件夹视图…"
                            },
                        );
                    });
                });
            self.overlays(ctx);
            return;
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(BG).inner_margin(12.0))
            .show(ctx, |ui| {
                // Even an empty scene needs a stable texture, geometry and
                // slot mapping. Later insertions can then patch this same
                // canvas instead of replacing its snapshot and rebuilding it.
                let viewport = ui.available_size().max(Vec2::splat(1.0));
                self.layout(viewport, ctx.pixels_per_point(), ctx);
                let empty = self
                    .map_cache
                    .as_ref()
                    .and_then(|cache| cache.projection.as_ref())
                    .map_or_else(
                        || self.view_data().unwrap().files.is_empty(),
                        |p| p.is_empty(),
                    );
                let now = ctx.input(|input| input.time);
                let animating = self.map_cache.as_ref().is_some_and(|cache| {
                    cache.transition.as_ref().is_some_and(|items| {
                        items.iter().any(|t| now - t.started < TRANSITION_SECONDS)
                    }) || cache
                        .full_transition
                        .as_ref()
                        .is_some_and(|t| now - t.started < TRANSITION_SECONDS)
                });
                if empty && !animating {
                    if let Some(frame) =
                        self.retained_frame.as_ref().filter(|f| f.mode == self.mode)
                    {
                        let started = *self.retained_empty_since.get_or_insert(now);
                        let t = ((now - started) / TRANSITION_SECONDS).clamp(0.0, 1.0) as f32;
                        if t < 1.0 {
                            let canvas = ui.max_rect();
                            self.canvas_rect = Some(canvas);
                            let eased = t * t * (3.0 - 2.0 * t);
                            paint_retained_frame(ui.painter(), canvas, frame, 1.0 - eased);
                            ctx.request_repaint_after(std::time::Duration::from_millis(16));
                            return;
                        }
                        self.retained_frame = None;
                        self.retained_empty_since = None;
                    }
                    ui.centered_and_justified(|ui| {
                        ui.label(if self.projection_dirty && self.coordinator_disabled {
                            "视图尚未更新，请重新扫描。"
                        } else if self.projection_dirty || self.job.is_some() {
                            "正在更新视图…"
                        } else {
                            "没有可显示的文件"
                        });
                    });
                    return;
                }
                let (canvas, response) = ui.allocate_exact_size(viewport, Sense::click());
                self.canvas_rect = Some(canvas);
                let painter = ui.painter_at(canvas);
                let pointer = tooltip_pointer(&response);
                let mut hovered = None;
                if let Some(cache) = &mut self.map_cache {
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
                    for hole in cache.holes.iter() {
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
                    let now = ctx.input(|input| input.time);
                    if cache.transition.as_ref().is_some_and(|items| {
                        items.iter().all(|a| now - a.started >= TRANSITION_SECONDS)
                    }) {
                        cache.transition = None;
                    }
                    let mut transition_areas = Vec::new();
                    for transition in cache.transition.iter().flatten() {
                        let [pw, ph] = map.physical_size();
                        let x = transition.origin[0] as f32 / pw as f32;
                        let y = transition.origin[1] as f32 / ph as f32;
                        let w = transition.size[0] as f32 / pw as f32;
                        let h = transition.size[1] as f32 / ph as f32;
                        let area = Rect::from_min_size(
                            canvas.min + Vec2::new(x * canvas.width(), y * canvas.height()),
                            Vec2::new(w * canvas.width(), h * canvas.height()),
                        );
                        if let Some(background) = &transition.background {
                            painter.image(
                                background.id(),
                                area,
                                Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }
                        let raw = ((now - transition.started) / TRANSITION_SECONDS).clamp(0.0, 1.0)
                            as f32;
                        let t = raw * raw * (3.0 - 2.0 * raw);
                        painter.image(
                            transition.previous.id(),
                            area,
                            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::from_white_alpha(((1.0 - t) * 255.0).round() as u8),
                        );
                        transition_areas.push(area);
                        ctx.request_repaint_after(std::time::Duration::from_millis(16));
                    }
                    // Paint every region's backing before any moving sprite.
                    // Otherwise a later region can cover a tile crossing into
                    // it, producing a visible discontinuity.
                    for transition in cache.transition.iter().flatten() {
                        let raw = ((now - transition.started) / TRANSITION_SECONDS).clamp(0.0, 1.0)
                            as f32;
                        let t = raw * raw * (3.0 - 2.0 * raw);
                        for tile in &transition.tiles {
                            let lerp = |a: f32, b: f32| a + (b - a) * t;
                            let from = tile.from;
                            let to = tile.to;
                            let r = treemap::Rect {
                                x: lerp(from.x, to.x),
                                y: lerp(from.y, to.y),
                                w: lerp(from.w, to.w),
                                h: lerp(from.h, to.h),
                            };
                            let dst = Rect::from_min_size(
                                canvas.min
                                    + Vec2::new(
                                        (r.x - map.bounds.x) * scale.x,
                                        (r.y - map.bounds.y) * scale.y,
                                    ),
                                Vec2::new(r.w * scale.x, r.h * scale.y),
                            );
                            if let Some(atlas) = &transition.atlas {
                                painter.image(atlas.id(), dst, tile.uv, Color32::WHITE);
                            }
                            // The sprite may travel between disjoint patch
                            // rectangles. Suppress hits over its full swept
                            // area, not just over the patch endpoints.
                            let left = from.x.min(to.x);
                            let top = from.y.min(to.y);
                            let right = (from.x + from.w).max(to.x + to.w);
                            let bottom = (from.y + from.h).max(to.y + to.h);
                            transition_areas.push(Rect::from_min_max(
                                canvas.min
                                    + Vec2::new(
                                        (left - map.bounds.x) * scale.x,
                                        (top - map.bounds.y) * scale.y,
                                    ),
                                canvas.min
                                    + Vec2::new(
                                        (right - map.bounds.x) * scale.x,
                                        (bottom - map.bounds.y) * scale.y,
                                    ),
                            ));
                        }
                    }
                    if cache
                        .full_transition
                        .as_ref()
                        .is_some_and(|f| now - f.started >= TRANSITION_SECONDS)
                    {
                        cache.full_transition = None;
                    }
                    if let Some(fade) = &cache.full_transition {
                        let t = ((now - fade.started) / TRANSITION_SECONDS).clamp(0.0, 1.0) as f32;
                        let eased = t * t * (3.0 - 2.0 * t);
                        paint_retained_frame(&painter, canvas, &fade.previous, 1.0 - eased);
                        // The displayed old pixels no longer have a current
                        // identity mapping. Disable hit testing until complete.
                        transition_areas.push(canvas);
                        ctx.request_repaint_after(std::time::Duration::from_millis(16));
                    }
                    if let Some(p) = pointer {
                        if !transition_areas.iter().any(|area| area.contains(p)) {
                            hovered = map.hit_test(
                                (p.x - canvas.min.x) / scale.x,
                                (p.y - canvas.min.y) / scale.y,
                            );
                        }
                    }
                    for (id, width) in [(self.selected, 2.0_f32), (hovered, 1.5_f32)] {
                        if let Some(r) = id.and_then(|i| map.rect(i)) {
                            let rect = Rect::from_min_size(
                                canvas.min + Vec2::new(r.x * scale.x, r.y * scale.y),
                                Vec2::new(r.w * scale.x, r.h * scale.y),
                            );
                            if rect.width() > 1.0
                                && rect.height() > 1.0
                                && !transition_areas.iter().any(|area| area.intersects(rect))
                            {
                                painter.rect_stroke(
                                    rect.shrink(0.5),
                                    0.0,
                                    egui::Stroke::new(width, Color32::WHITE),
                                    egui::StrokeKind::Inside,
                                );
                            }
                        }
                    }
                } else if let Some(frame) =
                    self.retained_frame.as_ref().filter(|f| f.mode == self.mode)
                {
                    paint_retained_frame(&painter, canvas, frame, 1.0);
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
                    if response.clicked() {
                        self.selected = Some(i);
                    }
                    if let Some(f) = self.view_file(i) {
                        if self.is_folder_tile(i) {
                            show_entry_tooltip(&response, f, true);
                        } else {
                            show_file_tooltip(&response, f);
                        }
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
        self.resize_generation.fetch_add(1, Ordering::Relaxed);
        self.folder_generation.fetch_add(1, Ordering::Relaxed);
        if let Some(data) = self.data.take() {
            defer_drop(data);
        }
        if let Some(map) = self.map_cache.take() {
            defer_drop(map);
        }
        for cache in &mut self.mode_caches {
            if let Some(map) = cache.take() {
                defer_drop(map);
            }
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
            directories: vec![
                scan::DirectoryFact {
                    path: "/".into(),
                    identity: Default::default(),
                    enumeration_complete: true,
                    has_subdirectories: true,
                    excluded: false,
                },
                scan::DirectoryFact {
                    path: "/a".into(),
                    identity: Default::default(),
                    enumeration_complete: true,
                    has_subdirectories: true,
                    excluded: false,
                },
                scan::DirectoryFact {
                    path: "/b".into(),
                    identity: Default::default(),
                    enumeration_complete: true,
                    has_subdirectories: true,
                    excluded: false,
                },
            ]
            .into(),
            files: vec![
                scan::FileEntry {
                    parent_identity: scan::FileIdentity::default(),
                    parent_is_leaf: false,
                    can_trash: true,
                    path: "/a/empty".into(),
                    parent: Path::new("/a").into(),
                    size: 0,
                    identity: scan::FileIdentity::default(),
                },
                scan::FileEntry {
                    parent_identity: scan::FileIdentity::default(),
                    parent_is_leaf: false,
                    can_trash: true,
                    path: "/a/large".into(),
                    parent: Path::new("/a").into(),
                    size: 1_000_000,
                    identity: scan::FileIdentity::default(),
                },
                scan::FileEntry {
                    parent_identity: scan::FileIdentity::default(),
                    parent_is_leaf: false,
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
    fn painted_frame(app: &mut App, ctx: &egui::Context) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1180.0, 780.0),
                )),
                ..Default::default()
            },
            |ctx| app.ui(ctx),
        )
    }
    fn settle(app: &mut App, ctx: &egui::Context) {
        let start = std::time::Instant::now();
        loop {
            frame(app, ctx);
            if app.totals_rx.is_none()
                && app.layout_rx.is_none()
                && !app.projection_dirty
                && app.totals_ready
                && app.map_cache.as_ref().is_some_and(|cache| {
                    cache.generation == app.layout_generation.load(Ordering::Relaxed)
                        && !cache.needs_rebuild
                })
            {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "timed out: totals={} layout={} dirty={} cache={} disabled={} message={:?}",
                app.totals_ready,
                app.layout_rx.is_some(),
                app.projection_dirty,
                app.map_cache.is_some(),
                app.coordinator_disabled,
                app.message
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    #[test]
    fn default_view_is_hybrid_and_folder_size_uses_folder_label() {
        assert_eq!(App::default().mode, ViewMode::Hybrid);
        assert_eq!(tooltip_size(42, true), "文件夹 42 B");
        assert_eq!(tooltip_size(42, false), "42 B");
    }
    #[test]
    fn folder_projection_cannot_delete_incomplete_or_excluded_descendants() {
        let mut data = snapshot();
        data.root = "/".into();
        data.directories = vec![
            scan::DirectoryFact {
                path: "/a/unknown".into(),
                identity: Default::default(),
                enumeration_complete: false,
                has_subdirectories: false,
                excluded: false,
            },
            scan::DirectoryFact {
                path: "/b/.Trash".into(),
                identity: Default::default(),
                enumeration_complete: false,
                has_subdirectories: false,
                excluded: true,
            },
        ]
        .into();
        let result = folder_snapshot(&data, &BTreeSet::new(), &AtomicU64::new(0), 0).unwrap();
        assert!(
            result
                .files
                .iter()
                .any(|file| file.path == Path::new("/a") && !file.can_trash)
        );
        assert!(
            result
                .files
                .iter()
                .any(|file| file.path == Path::new("/b") && !file.can_trash)
        );
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
            app.mode = ViewMode::Files;
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
            parent_identity: scan::FileIdentity::default(),
            parent_is_leaf: false,
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
        assert!(folders.files.iter().all(|f| f.can_trash));
        assert!(folder_snapshot(&data, &BTreeSet::new(), &token, 0).is_none());
    }
    #[test]
    fn mode_switch_keeps_selection_and_cached_maps_safe_after_deletion() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.mode = ViewMode::Files;
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
        app.set_mode(ViewMode::Files);
        assert_eq!(app.selected, None);
        settle(&mut app, &ctx);
        assert!(app.hidden.contains(&1));
        assert!(app.map_cache.as_ref().unwrap().map.rect(1).is_none());
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), file_texture);
        app.set_mode(ViewMode::Folders);
        settle(&mut app, &ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), folder_texture);
        // A file deletion can complete after switching to folder mode.
        app.hide_file(2);
        settle(&mut app, &ctx);
        assert_eq!(app.folder_data.as_ref().unwrap().files.len(), 2);
        assert_eq!(app.map_cache.as_ref().unwrap().map.rect(1), None);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), folder_texture);
        app.set_mode(ViewMode::Files);
        settle(&mut app, &ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), file_texture);
        assert!(app.hidden.contains(&2));
    }
    #[test]
    fn folder_mode_delete_shortcut_starts_trash_without_confirmation() {
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
        assert!(app.trash_rx.is_some());
    }
    #[test]
    fn hybrid_mode_preserves_tile_slots_and_reuses_cache_after_deletion() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        let mut data = snapshot();
        data.files[2].parent_is_leaf = true;
        Arc::make_mut(&mut data.directories)
            .iter_mut()
            .find(|fact| fact.path == Path::new("/b"))
            .unwrap()
            .has_subdirectories = false;
        app.data = Some(Arc::new(data));
        app.set_mode(ViewMode::Hybrid);
        settle(&mut app, &ctx);
        let hybrid = app.hybrid_data.as_ref().unwrap();
        assert_eq!(&*hybrid.source_ids, &[Some(0), Some(1), None]);
        assert!(!app.is_folder_tile(0));
        assert!(app.is_folder_tile(2));
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        app.set_mode(ViewMode::Folders);
        settle(&mut app, &ctx);
        app.set_mode(ViewMode::Hybrid);
        settle(&mut app, &ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        app.selected = Some(1);
        app.hide_file(1);
        assert_eq!(app.selected, None);
        settle(&mut app, &ctx);
        assert_eq!(
            &*app.hybrid_data.as_ref().unwrap().source_ids,
            &[Some(0), Some(1), None]
        );
        assert_eq!(app.map_cache.as_ref().unwrap().map.rect(1), None);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
    }

    #[test]
    fn folder_deletion_updates_all_cached_modes_without_replacing_textures() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        let mut data = snapshot();
        data.files[2].parent_is_leaf = true;
        Arc::make_mut(&mut data.directories)
            .iter_mut()
            .find(|fact| fact.path == Path::new("/b"))
            .unwrap()
            .has_subdirectories = false;
        app.data = Some(Arc::new(data));
        let mut textures = Vec::new();
        for mode in [ViewMode::Files, ViewMode::Folders, ViewMode::Hybrid] {
            app.set_mode(mode);
            settle(&mut app, &ctx);
            textures.push(app.map_cache.as_ref().unwrap().texture.id());
        }
        app.hide_folder(Arc::new(BTreeSet::from([0, 1])));
        // The old view remains visible until the background update is ready.
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), textures[2]);
        assert!(app.projection_dirty);
        settle(&mut app, &ctx);
        for (index, mode) in [ViewMode::Files, ViewMode::Folders, ViewMode::Hybrid]
            .into_iter()
            .enumerate()
        {
            app.set_mode(mode);
            settle(&mut app, &ctx);
            let cache = app.map_cache.as_ref().unwrap();
            assert_eq!(cache.texture.id(), textures[index]);
            assert!(cache.map.rect(0).is_none());
            assert!(
                cache
                    .map
                    .rect(2.min(if mode == ViewMode::Folders { 1 } else { 2 }))
                    .is_some()
            );
        }
    }

    #[test]
    fn old_frame_cannot_reveal_file_already_moved_to_trash() {
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.data = Some(Arc::new(snapshot()));
        app.hidden = Arc::new(BTreeSet::from([1]));
        app.reveal(1);
        assert!(app.reveal_rx.is_none());
        assert!(app.message.as_deref().unwrap().contains("视图更新"));
    }

    #[test]
    fn file_deletion_uses_versioned_incremental_coordinator() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let start = Instant::now();
        while app
            .map_cache
            .as_ref()
            .is_none_or(|cache| cache.projection.is_none())
        {
            frame(&mut app, &ctx);
            assert!(
                !app.coordinator_disabled,
                "coordinator fell back during initialisation"
            );
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let before = app.coordinator_version.unwrap();
        app.hide_file(0);
        settle(&mut app, &ctx);
        assert!(
            !app.coordinator_disabled,
            "incremental deletion unexpectedly fell back"
        );
        assert!(app.coordinator_version.unwrap().scene_version > before.scene_version);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        assert!(app.view_file(0).is_none());
        assert!(app.view_file(1).is_some());
        let second_base = app.coordinator_version.unwrap();
        app.hide_file(2);
        settle(&mut app, &ctx);
        assert!(
            !app.coordinator_disabled,
            "second deletion must remain incremental"
        );
        assert!(app.coordinator_version.unwrap().scene_version > second_base.scene_version);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        assert!(app.view_file(2).is_none());
    }

    #[test]
    fn deletion_arriving_during_an_inflight_update_is_queued() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let started = Instant::now();
        while app
            .map_cache
            .as_ref()
            .is_none_or(|cache| cache.projection.is_none())
        {
            frame(&mut app, &ctx);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        app.hide_file(0);
        assert!(app.try_coordinator_update());
        assert!(app.coordinator_update_pending);
        app.hide_file(2);
        assert!(
            app.coordinator.is_some(),
            "new deletion must not abandon the transaction"
        );
        settle(&mut app, &ctx);
        assert!(!app.coordinator_disabled);
        assert!(!app.projection_dirty);
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.texture.id(), texture);
        assert!(cache.map.rect(0).is_none());
        assert!(cache.map.rect(2).is_none());
        assert!(cache.map.rect(1).is_some());
    }

    #[test]
    fn same_root_rescan_reconciles_without_replacing_texture() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let id = |n| scan::FileIdentity {
            device: n,
            ..Default::default()
        };
        let fact = scan::DirectoryFact {
            path: "/r".into(),
            identity: id(1),
            enumeration_complete: true,
            has_subdirectories: false,
            excluded: false,
        };
        let entry = |name: &str, n, size| scan::FileEntry {
            path: format!("/r/{name}").into(),
            parent: Path::new("/r").into(),
            size,
            identity: id(n),
            parent_identity: id(1),
            parent_is_leaf: true,
            can_trash: true,
        };
        let mut initial = snapshot();
        initial.root = "/r".into();
        initial.directories = vec![fact.clone()].into();
        initial.files = vec![entry("a", 2, 10), entry("b", 3, 10)];
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.replace_snapshot(initial);
        settle(&mut app, &ctx);
        let started = Instant::now();
        while app
            .map_cache
            .as_ref()
            .is_none_or(|cache| cache.projection.is_none())
        {
            frame(&mut app, &ctx);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let mut refreshed = snapshot();
        refreshed.root = "/r".into();
        refreshed.directories = vec![fact].into();
        refreshed.files = vec![entry("a", 2, 12), entry("b", 3, 10), entry("c", 4, 2)];
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(refreshed)).unwrap();
        app.job = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::default(),
            rx,
            mutation_epoch: app.scene_mutation_epoch,
        });
        app.poll(&ctx);
        settle(&mut app, &ctx);
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.texture.id(), texture);
        let projection = cache.projection.as_ref().unwrap();
        assert!(
            projection
                .iter()
                .any(|(_, _, entry)| entry.path == Path::new("/r/a") && entry.size == 12)
        );
        assert!(
            projection
                .iter()
                .any(|(_, _, entry)| entry.path == Path::new("/r/c"))
        );
        assert_eq!(app.scan_summary.as_ref().unwrap().files, 3);
    }

    #[test]
    fn rescan_after_switch_to_unbuilt_mode_keeps_previous_cached_view() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let id = |n| scan::FileIdentity {
            device: n,
            ..Default::default()
        };
        let fact = scan::DirectoryFact {
            path: "/r".into(),
            identity: id(1),
            enumeration_complete: true,
            has_subdirectories: false,
            excluded: false,
        };
        let entry = |name: &str, n, size| scan::FileEntry {
            path: format!("/r/{name}").into(),
            parent: Path::new("/r").into(),
            size,
            identity: id(n),
            parent_identity: id(1),
            parent_is_leaf: true,
            can_trash: true,
        };
        let make = |with_extra| scan::ScanResult {
            root: "/r".into(),
            directories: vec![fact.clone()].into(),
            files: if with_extra {
                vec![entry("a", 2, 10), entry("b", 3, 3)]
            } else {
                vec![entry("a", 2, 10)]
            },
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.replace_snapshot(make(false));
        settle(&mut app, &ctx);
        let started = Instant::now();
        while !app.coordinator_bound[ViewMode::Files.index()] {
            frame(&mut app, &ctx);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(make(true))).unwrap();
        app.job = Some(Job {
            cancel: Arc::default(),
            progress: Arc::default(),
            rx,
            mutation_epoch: app.scene_mutation_epoch,
        });
        app.set_mode(ViewMode::Folders);
        app.poll(&ctx);
        let started = Instant::now();
        while app.job.is_some()
            || app.projection_dirty
            || !app.coordinator_bound[ViewMode::Folders.index()]
        {
            frame(&mut app, &ctx);
            assert!(!app.coordinator_disabled);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            app.map_cache
                .as_ref()
                .unwrap()
                .projection
                .as_ref()
                .unwrap()
                .iter()
                .any(|(_, _, file)| file.path == Path::new("/r") && file.size == 13)
        );
        app.set_mode(ViewMode::Files);
        settle(&mut app, &ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        assert!(
            app.map_cache
                .as_ref()
                .unwrap()
                .projection
                .as_ref()
                .unwrap()
                .iter()
                .any(|(_, _, file)| file.path == Path::new("/r/b"))
        );
    }

    #[test]
    fn deletion_during_scan_finishes_and_rejects_stale_scan_result() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let started = Instant::now();
        while !app.coordinator_bound[ViewMode::Files.index()] {
            frame(&mut app, &ctx);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let (tx, rx) = mpsc::channel();
        tx.send(Ok(snapshot())).unwrap(); // captured before deletion
        app.job = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::default(),
            rx,
            mutation_epoch: app.scene_mutation_epoch,
        });
        app.hide_file(0);
        assert!(app.try_coordinator_update());
        // Do not launch a real platform scan when stale data is rejected.
        app.access = permission::Access::Unverified;
        let started = Instant::now();
        while app.projection_dirty || app.job.is_some() {
            frame(&mut app, &ctx);
            assert!(!app.coordinator_disabled);
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "scan and update must both finish"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.texture.id(), texture);
        assert!(
            cache.map.rect(0).is_none(),
            "stale scan must not restore the deleted file"
        );
    }

    #[test]
    fn empty_scene_rescans_to_nonempty_and_back_without_replacing_texture() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let id = |n| scan::FileIdentity {
            device: n,
            ..Default::default()
        };
        let fact = scan::DirectoryFact {
            path: "/r".into(),
            identity: id(1),
            enumeration_complete: true,
            has_subdirectories: false,
            excluded: false,
        };
        let make = |with_file: bool| scan::ScanResult {
            root: "/r".into(),
            directories: vec![fact.clone()].into(),
            files: if with_file {
                vec![scan::FileEntry {
                    path: "/r/a".into(),
                    parent: Path::new("/r").into(),
                    size: 12,
                    identity: id(2),
                    parent_identity: id(1),
                    parent_is_leaf: true,
                    can_trash: true,
                }]
            } else {
                vec![]
            },
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.replace_snapshot(make(false));
        settle(&mut app, &ctx);
        let started = Instant::now();
        while !app.coordinator_bound[ViewMode::Files.index()] {
            frame(&mut app, &ctx);
            assert!(!app.coordinator_disabled);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        for exists in [true, false] {
            let (tx, rx) = mpsc::channel();
            tx.send(Ok(make(exists))).unwrap();
            app.job = Some(Job {
                cancel: Arc::new(AtomicBool::new(false)),
                progress: Arc::default(),
                rx,
                mutation_epoch: app.scene_mutation_epoch,
            });
            app.poll(&ctx);
            let started = Instant::now();
            while app.job.is_some() || app.projection_dirty {
                frame(&mut app, &ctx);
                assert!(!app.coordinator_disabled);
                assert!(started.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(2));
            }
            settle(&mut app, &ctx);
            let cache = app.map_cache.as_ref().unwrap();
            assert_eq!(cache.texture.id(), texture);
            assert_eq!(!cache.projection.as_ref().unwrap().is_empty(), exists);
            assert!(app.job.is_none());
            if !exists {
                let cache = app.map_cache.as_mut().unwrap();
                cache.transition = None;
                cache.full_transition = None;
                app.retained_frame = None;
                assert!(app.scene_source_dirty);
                text_shape(&painted_frame(&mut app, &ctx), "没有可显示的文件");
            }
        }
    }

    #[test]
    fn deleting_last_file_shows_empty_state_after_incremental_commit() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut initial = snapshot();
        initial.files.truncate(1);
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.replace_snapshot(initial);
        settle(&mut app, &ctx);
        let started = Instant::now();
        while !app.coordinator_bound[ViewMode::Files.index()] {
            frame(&mut app, &ctx);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let file = app.data.as_ref().unwrap().files[0].clone();
        app.queue_scene_changes(vec![scene_model::Change::RemoveFile {
            path: file.path,
            identity: file.identity,
        }]);
        settle(&mut app, &ctx);
        let cache = app.map_cache.as_mut().unwrap();
        assert!(cache.projection.as_ref().unwrap().is_empty());
        assert_eq!(cache.texture.id(), texture);
        cache.transition = None;
        cache.full_transition = None;
        app.retained_frame = None;
        assert!(app.scene_source_dirty);
        text_shape(&painted_frame(&mut app, &ctx), "没有可显示的文件");
    }

    #[test]
    fn empty_scene_reports_pending_and_failed_updates() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut initial = snapshot();
        initial.files.clear();
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.replace_snapshot(initial);
        settle(&mut app, &ctx);

        let (_tx, rx) = mpsc::channel();
        app.job = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::default(),
            rx,
            mutation_epoch: app.scene_mutation_epoch,
        });
        text_shape(&painted_frame(&mut app, &ctx), "正在更新视图…");
        app.job = None;

        app.projection_dirty = true;
        app.coordinator_update_pending = true;
        text_shape(&painted_frame(&mut app, &ctx), "正在更新视图…");
        app.coordinator_update_pending = false;
        app.coordinator_disabled = true;
        text_shape(&painted_frame(&mut app, &ctx), "视图尚未更新，请重新扫描。");
    }

    #[test]
    fn coordinator_recovery_replays_published_changes_and_keeps_texture() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let started = Instant::now();
        while !app.coordinator_bound[ViewMode::Files.index()] {
            frame(&mut app, &ctx);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let mut added = app.data.as_ref().unwrap().files[2].clone();
        added.path = "/b/new-recovery".into();
        app.queue_scene_changes(vec![scene_model::Change::UpsertFile(added.clone())]);
        settle(&mut app, &ctx);
        let old_version = app.coordinator_version.unwrap();
        app.abandon_coordinator();
        let started = Instant::now();
        while !app.coordinator_bound[ViewMode::Files.index()] {
            frame(&mut app, &ctx);
            assert!(!app.coordinator_disabled, "journal recovery failed");
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(app.coordinator_version, Some(old_version));
        let projection = app.map_cache.as_ref().unwrap().projection.as_ref().unwrap();
        assert!(
            projection
                .iter()
                .any(|(_, _, file)| file.path == added.path)
        );
        app.queue_scene_changes(vec![scene_model::Change::RemoveFile {
            path: added.path.clone(),
            identity: added.identity,
        }]);
        settle(&mut app, &ctx);
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.texture.id(), texture);
        assert!(
            !cache
                .projection
                .as_ref()
                .unwrap()
                .iter()
                .any(|(_, _, file)| file.path == added.path)
        );
    }

    #[test]
    fn manual_retry_reenables_journal_recovery_after_repeated_failures() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let started = Instant::now();
        while !app.coordinator_bound[ViewMode::Files.index()] {
            frame(&mut app, &ctx);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let mut file = app.data.as_ref().unwrap().files[2].clone();
        file.path = "/b/retry".into();
        app.queue_scene_changes(vec![scene_model::Change::UpsertFile(file.clone())]);
        settle(&mut app, &ctx);
        for _ in 0..3 {
            app.abandon_coordinator();
        }
        assert!(app.coordinator_disabled);
        app.start();
        assert!(!app.coordinator_disabled);
        // The platform permission probe is independent of scene recovery.
        app.permission_rx = None;
        app.pending_scan = false;
        app.access = permission::Access::Granted;
        let started = Instant::now();
        while !app.coordinator_bound[ViewMode::Files.index()] {
            frame(&mut app, &ctx);
            assert!(!app.coordinator_disabled);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        file.size += 1;
        app.queue_scene_changes(vec![scene_model::Change::UpsertFile(file.clone())]);
        settle(&mut app, &ctx);
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.texture.id(), texture);
        assert!(
            cache
                .projection
                .as_ref()
                .unwrap()
                .iter()
                .any(|(_, _, entry)| entry.path == file.path && entry.size == file.size)
        );
    }

    #[test]
    fn file_deletion_updates_three_versioned_modes_without_replacing_textures() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut data = snapshot();
        data.files[2].parent_is_leaf = true;
        data.directories = vec![
            scan::DirectoryFact {
                path: "/a".into(),
                identity: scan::FileIdentity::default(),
                enumeration_complete: true,
                has_subdirectories: true,
                excluded: false,
            },
            scan::DirectoryFact {
                path: "/b".into(),
                identity: scan::FileIdentity::default(),
                enumeration_complete: true,
                has_subdirectories: false,
                excluded: false,
            },
        ]
        .into();
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(data));
        let mut textures = Vec::new();
        for mode in [ViewMode::Files, ViewMode::Folders, ViewMode::Hybrid] {
            app.set_mode(mode);
            settle(&mut app, &ctx);
            let start = Instant::now();
            while app
                .map_cache
                .as_ref()
                .is_none_or(|cache| cache.projection.is_none())
            {
                frame(&mut app, &ctx);
                assert!(
                    !app.coordinator_disabled,
                    "coordinator could not initialize {mode:?}"
                );
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(2));
            }
            textures.push(app.map_cache.as_ref().unwrap().texture.id());
        }
        let before = app.coordinator_version.unwrap();
        app.hide_file(2);
        settle(&mut app, &ctx);
        assert!(!app.coordinator_disabled, "three-mode update fell back");
        assert!(app.coordinator_version.unwrap().scene_version > before.scene_version);
        for (index, mode) in [ViewMode::Files, ViewMode::Folders, ViewMode::Hybrid]
            .into_iter()
            .enumerate()
        {
            app.set_mode(mode);
            settle(&mut app, &ctx);
            let cache = app.map_cache.as_ref().unwrap();
            assert_eq!(cache.texture.id(), textures[index]);
            let deleted = if mode == ViewMode::Folders { 1 } else { 2 };
            assert!(app.view_file(deleted).is_none());
            assert!(cache.map.rect(deleted).is_none());
        }
    }

    #[test]
    fn leaf_folder_receipt_animates_sibling_and_keeps_unaffected_texture() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let id = |device| scan::FileIdentity {
            device,
            ..Default::default()
        };
        let fact = |path: &str, device, has_subdirectories| scan::DirectoryFact {
            path: path.into(),
            identity: id(device),
            enumeration_complete: true,
            has_subdirectories,
            excluded: false,
        };
        let file = |name: &str, directory, device| scan::FileEntry {
            path: format!("/disk/{name}/f").into(),
            parent: Arc::from(PathBuf::from(format!("/disk/{name}"))),
            size: 50,
            identity: id(device),
            parent_identity: id(directory),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("a", 2, 4), file("b", 3, 5)],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, false),
                fact("/disk/b", 3, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(source.clone());
        settle(&mut app, &ctx);
        let started = Instant::now();
        while app
            .map_cache
            .as_ref()
            .is_none_or(|c| c.projection.is_none())
        {
            frame(&mut app, &ctx);
            assert!(!app.coordinator_disabled);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        tx.send(TrashOutput {
            source,
            id: None,
            file_change: Some(scene_model::Change::RemoveLeafDirectory {
                path: "/disk/a".into(),
                identity: id(2),
                parent: fact("/disk", 1, true),
            }),
            folder_hidden: Some(Arc::new(BTreeSet::from([0]))),
            result: Ok(()),
        })
        .unwrap();
        settle(&mut app, &ctx);
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.texture.id(), texture);
        assert!(app.view_file(0).is_none());
        assert!(
            cache.transition.is_some(),
            "surviving sibling should transition"
        );
        assert!(
            app.job.is_none(),
            "verified leaf deletion should avoid a full scan"
        );
    }

    #[test]
    fn confirmed_folder_receipt_queues_old_identity_without_abandoning_frame() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let start = Instant::now();
        while app
            .map_cache
            .as_ref()
            .is_none_or(|cache| cache.projection.is_none())
        {
            frame(&mut app, &ctx);
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let source = app.data.clone().unwrap();
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        // Avoid starting a real system-root scan from this UI-only test.
        app.access = permission::Access::Unverified;
        tx.send(TrashOutput {
            source,
            id: None,
            file_change: Some(scene_model::Change::RemoveConfirmedSubtree {
                path: "/b".into(),
                identity: scan::FileIdentity::default(),
            }),
            folder_hidden: Some(Arc::new(BTreeSet::from([2]))),
            result: Ok(()),
        })
        .unwrap();
        app.poll(&ctx);
        assert!(app.coordinator.is_some());
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        assert!(app.hidden.contains(&2));
        assert!(app.pending_scene_changes.iter().any(|batch| matches!(
            &batch.payload,
            QueuedScenePayload::Explicit(changes)
                if matches!(changes.as_slice(), [scene_model::Change::RemoveConfirmedSubtree { path, .. }] if path == Path::new("/b"))
        )));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn confirmed_folder_reconciles_replaced_parent_and_same_name_without_replacing_texture() {
        use std::fs;
        use std::sync::atomic::AtomicBool;

        // Only create and move our own fixture; retain it in the system Trash
        // instead of permanently deleting files during cleanup.
        struct Fixture(PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::process::Command::new("/usr/bin/trash")
                    .arg(&self.0)
                    .status();
            }
        }
        let unique = format!(
            "visualize-ssd-confirmed-reconcile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let base = fs::canonicalize(std::env::temp_dir()).unwrap().join(unique);
        fs::create_dir(&base).unwrap();
        let fixture = Fixture(base.clone());
        let root = base.join("root");
        fs::create_dir_all(root.join("parent/target")).unwrap();
        fs::write(root.join("parent/target/old"), b"old object").unwrap();
        fs::create_dir(root.join("other")).unwrap();
        fs::write(root.join("other/keep"), b"keep").unwrap();
        let scan_now = || {
            scan::scan(
                root.clone(),
                Arc::new(AtomicBool::new(false)),
                Arc::new(Mutex::new(scan::ScanProgress::default())),
            )
            .unwrap()
        };
        let initial = scan_now();
        let old_target = initial
            .directories
            .iter()
            .find(|d| d.path == root.join("parent/target"))
            .unwrap()
            .identity;
        let old_parent = initial
            .directories
            .iter()
            .find(|d| d.path == root.join("parent"))
            .unwrap()
            .identity;
        let old_file_index = initial
            .files
            .iter()
            .position(|f| f.path == root.join("parent/target/old"))
            .unwrap();

        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.replace_snapshot(initial);
        settle(&mut app, &ctx);
        let start = Instant::now();
        while app
            .map_cache
            .as_ref()
            .is_none_or(|c| c.projection.is_none())
        {
            frame(&mut app, &ctx);
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        // Simulate the successful move and a concurrent replacement of its
        // parent and same-name child; the old identity remains allocated.
        fs::rename(root.join("parent"), base.join("moved-parent")).unwrap();
        fs::create_dir_all(root.join("parent/target")).unwrap();
        fs::write(root.join("parent/target/new"), b"new object").unwrap();
        let refreshed = scan_now();
        let new_parent = refreshed
            .directories
            .iter()
            .find(|d| d.path == root.join("parent"))
            .unwrap()
            .identity;
        let new_target = refreshed
            .directories
            .iter()
            .find(|d| d.path == root.join("parent/target"))
            .unwrap()
            .identity;
        assert_ne!(old_parent, new_parent);
        assert_ne!(old_target, new_target);

        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        // Supply the fresh scan below, instead of scanning the real startup disk.
        app.access = permission::Access::Unverified;
        tx.send(TrashOutput {
            source: app.data.clone().unwrap(),
            id: None,
            file_change: Some(scene_model::Change::RemoveConfirmedSubtree {
                path: root.join("parent/target"),
                identity: old_target,
            }),
            folder_hidden: Some(Arc::new(BTreeSet::from([old_file_index]))),
            result: Ok(()),
        })
        .unwrap();
        app.poll(&ctx);
        app.access = permission::Access::Granted;
        settle(&mut app, &ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        assert!(
            app.map_cache
                .as_ref()
                .unwrap()
                .projection
                .as_ref()
                .unwrap()
                .iter()
                .all(|(_, _, f)| f.path != root.join("parent/target/old"))
        );

        let (tx, rx) = mpsc::channel();
        tx.send(Ok(refreshed)).unwrap();
        app.job = Some(Job {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::default(),
            rx,
            mutation_epoch: app.scene_mutation_epoch,
        });
        app.poll(&ctx);
        let started = Instant::now();
        while app.job.is_some() {
            frame(&mut app, &ctx);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        settle(&mut app, &ctx);
        assert!(!app.coordinator_disabled);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        let projection = app.map_cache.as_ref().unwrap().projection.as_ref().unwrap();
        assert!(
            projection
                .iter()
                .any(|(_, _, f)| f.path == root.join("parent/target/new")
                    && f.parent_identity == new_target)
        );
        assert_eq!(
            fs::read(root.join("parent/target/new")).unwrap(),
            b"new object"
        );
        assert_eq!(
            fs::read(base.join("moved-parent/target/old")).unwrap(),
            b"old object"
        );
        drop(fixture);
    }

    #[test]
    fn scene_change_path_adds_resizes_and_removes_a_new_file() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let start = Instant::now();
        while app
            .map_cache
            .as_ref()
            .is_none_or(|cache| cache.projection.is_none())
        {
            frame(&mut app, &ctx);
            assert!(!app.coordinator_disabled);
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let mut file = app.data.as_ref().unwrap().files[2].clone();
        file.path = "/b/new".into();
        file.size = 1;
        app.queue_scene_changes(vec![scene_model::Change::UpsertFile(file.clone())]);
        settle(&mut app, &ctx);
        assert!(!app.coordinator_disabled, "insert fell back");
        let projection = app.map_cache.as_ref().unwrap().projection.as_ref().unwrap();
        let (slot, _, _) = projection
            .iter()
            .find(|(_, _, f)| f.path == file.path)
            .unwrap();
        assert_eq!(app.view_file(slot).unwrap().size, 1);
        file.size = 2;
        app.queue_scene_changes(vec![scene_model::Change::UpsertFile(file.clone())]);
        settle(&mut app, &ctx);
        assert!(!app.coordinator_disabled, "resize fell back");
        assert_eq!(app.view_file(slot).unwrap().size, 2);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        // A not-yet-built mode must come from the current scene, not the
        // stale baseline scan, and must not trigger another full scan.
        app.set_mode(ViewMode::Folders);
        settle(&mut app, &ctx);
        let started = Instant::now();
        while app
            .map_cache
            .as_ref()
            .is_none_or(|cache| cache.projection.is_none())
        {
            frame(&mut app, &ctx);
            assert!(app.job.is_none());
            assert!(!app.coordinator_disabled);
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let projection = app.map_cache.as_ref().unwrap().projection.as_ref().unwrap();
        let expected = app
            .data
            .as_ref()
            .unwrap()
            .files
            .iter()
            .filter(|entry| entry.parent.as_ref() == Path::new("/b"))
            .map(|entry| entry.size)
            .sum::<u64>()
            + 2;
        assert!(
            projection
                .iter()
                .any(|(_, _, entry)| entry.path == Path::new("/b") && entry.size == expected)
        );
        app.set_mode(ViewMode::Files);
        settle(&mut app, &ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
    }

    #[test]
    fn newly_added_file_delete_uses_scene_identity_not_original_slot() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let start = Instant::now();
        while app
            .map_cache
            .as_ref()
            .is_none_or(|cache| cache.projection.is_none())
        {
            frame(&mut app, &ctx);
            assert!(!app.coordinator_disabled);
            assert!(start.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(2));
        }
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let mut file = app.data.as_ref().unwrap().files[2].clone();
        file.path = "/b/new".into();
        file.size = 1;
        app.queue_scene_changes(vec![scene_model::Change::UpsertFile(file.clone())]);
        settle(&mut app, &ctx);
        let projection = app.map_cache.as_ref().unwrap().projection.as_ref().unwrap();
        let (slot, _, _) = projection
            .iter()
            .find(|(_, _, f)| f.path == file.path)
            .unwrap();
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        tx.send(TrashOutput {
            source: app.data.clone().unwrap(),
            id: None,
            file_change: Some(scene_model::Change::RemoveFile {
                path: file.path,
                identity: file.identity,
            }),
            folder_hidden: None,
            result: Ok(()),
        })
        .unwrap();
        app.poll(&ctx);
        settle(&mut app, &ctx);
        assert!(!app.coordinator_disabled, "removal fell back");
        assert!(app.view_file(slot).is_none());
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
    }

    #[test]
    fn failed_or_cancelled_stale_rescan_offers_retry_instead_of_busy_state() {
        for cancelled in [false, true] {
            let ctx = egui::Context::default();
            configure(&ctx);
            let mut app = App::default();
            app.access = permission::Access::Granted;
            app.data = Some(Arc::new(snapshot()));
            app.mode = ViewMode::Folders;
            app.scene_source_dirty = true;
            app.coordinator_disabled = true;
            let (tx, rx) = mpsc::channel();
            tx.send(if cancelled {
                Ok(snapshot())
            } else {
                Err("扫描失败。".into())
            })
            .unwrap();
            app.job = Some(Job {
                cancel: Arc::new(AtomicBool::new(cancelled)),
                progress: Arc::default(),
                rx,
                mutation_epoch: 0,
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
            assert!(app.scene_source_dirty);
            text_shape(&output, "视图尚未更新，请重新扫描。");
        }
    }

    #[test]
    fn coordinator_recovers_after_fallback_with_existing_tombstone() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        settle(&mut app, &ctx);
        let wait = |app: &mut App| {
            let start = Instant::now();
            while app
                .map_cache
                .as_ref()
                .is_none_or(|cache| cache.projection.is_none())
            {
                frame(app, &ctx);
                assert!(!app.coordinator_disabled, "coordinator could not recover");
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        wait(&mut app);
        app.hide_file(0);
        settle(&mut app, &ctx);
        app.abandon_coordinator();
        app.begin_projection_update();
        settle(&mut app, &ctx);
        wait(&mut app);
        assert!(app.view_file(0).is_none());
        let before = app.coordinator_version.unwrap();
        app.hide_file(2);
        settle(&mut app, &ctx);
        assert!(!app.coordinator_disabled);
        assert!(app.coordinator_version.unwrap().scene_version > before.scene_version);
        assert!(app.view_file(2).is_none());
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
            mutation_epoch: 0,
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
        assert!(app.hybrid_rx.is_some() || app.layout_rx.is_some());
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
            app.mode = ViewMode::Files;
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
            app.mode = ViewMode::Files;
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
            holes: Vec::new(),
            partitions: None,
            previous: None,
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
            holes: Vec::new(),
            partitions: None,
            previous: None,
        })
        .unwrap();
        app.poll(ctx);
    }
    #[test]
    fn fallback_bakes_holes_and_retries_if_displayed_revision_changed() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.data = Some(Arc::new(snapshot()));
        let key = LayoutKey::new(Vec2::new(300.0, 180.0), 1.0);
        apply_test_map(&mut app, &ctx, key, 1);
        let cache = app.map_cache.as_mut().unwrap();
        let holes = cache.map.erase(1);
        assert!(!holes.is_empty());
        cache.holes = Arc::new(holes);
        let old_texture = cache.texture.id();
        let bake = prepare_previous_frame(
            old_texture,
            cache.frame_revision,
            cache.holes.clone(),
            cache.map.snapshot_for_update(),
            &app.layout_generation,
            1,
        )
        .unwrap();
        assert!(bake.image.pixels.iter().any(|pixel| *pixel == BG));
        // A patch can keep the same GPU texture and holes while changing its
        // pixels. A bake of the preceding frame must be discarded and retried.
        cache.frame_revision += 1;
        app.layout_generation.store(2, Ordering::Relaxed);
        app.layout_key = Some(key);
        let map =
            layout_snapshot(app.data.as_ref().unwrap(), key, &app.layout_generation, 2).unwrap();
        let (tx, rx) = mpsc::channel();
        app.layout_rx = Some(rx);
        tx.send(LayoutOutput {
            generation: 2,
            key,
            map,
            holes: Vec::new(),
            partitions: None,
            previous: Some(bake),
        })
        .unwrap();
        app.poll(&ctx);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), old_texture);
        assert_eq!(app.layout_generation.load(Ordering::Relaxed), 3);
        assert!(app.retained_frame.is_some());
        app.layout(key.logical_size, 1.0, &ctx);
        let start = Instant::now();
        while app.map_cache.as_ref().unwrap().texture.id() == old_texture {
            app.poll(&ctx);
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::yield_now();
        }
        let fade = app
            .map_cache
            .as_ref()
            .unwrap()
            .full_transition
            .as_ref()
            .unwrap();
        assert!(
            fade.previous.holes.is_empty(),
            "old holes must be baked before GPU fade"
        );
    }

    #[test]
    fn full_frame_fade_suppresses_stale_clicks_until_new_scene_is_visible() {
        let ctx = egui::Context::default();
        configure(&ctx);
        let mut app = App::default();
        app.mode = ViewMode::Files;
        app.access = permission::Access::Granted;
        app.data = Some(Arc::new(snapshot()));
        let key = LayoutKey::new(Vec2::new(300.0, 180.0), 1.0);
        apply_test_map(&mut app, &ctx, key, 1);
        apply_test_map(&mut app, &ctx, key, 2);
        assert!(app.map_cache.as_ref().unwrap().full_transition.is_some());
        let input = |time, events| egui::RawInput {
            time: Some(time),
            screen_rect: Some(Rect::from_min_size(
                egui::Pos2::ZERO,
                Vec2::new(1180.0, 780.0),
            )),
            events,
            ..Default::default()
        };
        let _ = ctx.run(input(0.01, vec![]), |ctx| app.ui(ctx));
        let canvas = app.canvas_rect.unwrap();
        let map = &app.map_cache.as_ref().unwrap().map;
        let rect = map.rect(1).unwrap();
        let pos = canvas.min
            + Vec2::new(
                (rect.x + rect.w * 0.5) * canvas.width() / map.bounds.w,
                (rect.y + rect.h * 0.5) * canvas.height() / map.bounds.h,
            );
        for (time, pressed) in [(0.05, true), (0.08, false)] {
            let events = vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Default::default(),
                },
            ];
            let _ = ctx.run(input(time, events), |ctx| app.ui(ctx));
        }
        assert!(
            app.selected.is_none(),
            "old visible pixels must not select a new scene entry"
        );
        let _ = ctx.run(input(0.35, vec![]), |ctx| app.ui(ctx));
        assert!(app.map_cache.as_ref().unwrap().full_transition.is_none());
    }

    #[test]
    fn resize_preserves_geometry_texture_and_hit_testing() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.data = Some(Arc::new(snapshot()));
        let initial = LayoutKey::new(Vec2::new(800.0, 400.0), 1.0);
        apply_test_map(&mut app, &ctx, initial, 1);
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        let rects: Vec<_> = (0..3)
            .map(|id| app.map_cache.as_ref().unwrap().map.rect(id))
            .collect();
        app.layout(Vec2::new(400.0, 900.0), 2.0, &ctx);
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.texture.id(), texture);
        assert_eq!(cache.key, initial);
        assert_eq!(app.layout_key, Some(initial));
        assert_eq!(app.layout_generation.load(Ordering::Relaxed), 1);
        assert!(app.layout_rx.is_none());
        for id in 0..3 {
            let r = cache.map.rect(id).unwrap();
            assert_eq!(Some(r), rects[id]);
            if r.w > 2.0 && r.h > 2.0 {
                // The canvas scales world coordinates in both directions.
                let displayed_x = (r.x + r.w * 0.5) * 400.0 / cache.map.bounds.w;
                let displayed_y = (r.y + r.h * 0.5) * 900.0 / cache.map.bounds.h;
                assert_eq!(
                    cache.map.hit_test(
                        displayed_x * cache.map.bounds.w / 400.0,
                        displayed_y * cache.map.bounds.h / 900.0,
                    ),
                    Some(id)
                );
            }
        }
    }
    #[test]
    fn dpi_change_reuses_existing_scene_without_rebuilding() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.data = Some(Arc::new(snapshot()));
        let size = Vec2::new(800.0, 600.0);
        apply_test_map(&mut app, &ctx, LayoutKey::new(size, 1.0), 1);
        let texture = app.map_cache.as_ref().unwrap().texture.id();
        app.layout(size, 2.0, &ctx);
        assert_eq!(app.layout_generation.load(Ordering::Relaxed), 1);
        assert_eq!(app.layout_key.unwrap().physical_size, [800, 600]);
        assert_eq!(app.map_cache.as_ref().unwrap().generation, 1);
        assert_eq!(app.map_cache.as_ref().unwrap().texture.id(), texture);
        assert!(app.layout_rx.is_none());
    }
    #[test]
    fn dpi_resize_resamples_tiles_without_replacing_geometry_or_texture() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.data = Some(Arc::new(snapshot()));
        let size = Vec2::new(180.0, 120.0);
        apply_test_map(&mut app, &ctx, LayoutKey::new(size, 1.0), 1);
        let cache = app.map_cache.as_ref().unwrap();
        let texture = cache.texture.id();
        let geometry = cache.map.rect(1);
        app.layout(size, 2.0, &ctx);
        // Elide the interaction debounce without sleeping for the UI test.
        app.resize_target.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(1);
        app.layout(size, 2.0, &ctx);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            app.layout(size, 2.0, &ctx);
            if app.map_cache.as_ref().unwrap().map.physical_size() == [360, 240] {
                break;
            }
            assert!(Instant::now() < deadline, "resize worker did not publish");
            std::thread::yield_now();
        }
        let cache = app.map_cache.as_ref().unwrap();
        assert_eq!(cache.texture.id(), texture);
        assert_eq!(cache.map.rect(1), geometry);
        assert_eq!(app.layout_generation.load(Ordering::Relaxed), 1);
        assert!(app.layout_rx.is_none());
    }
    #[test]
    fn resized_pixels_wait_for_scene_transaction_and_reject_stale_frame() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.data = Some(Arc::new(snapshot()));
        let size = Vec2::new(180.0, 120.0);
        apply_test_map(&mut app, &ctx, LayoutKey::new(size, 1.0), 1);
        let cache = app.map_cache.as_ref().unwrap();
        let stamp = ResizeStamp {
            mode: app.mode,
            texture: cache.texture.id(),
            frame_revision: cache.frame_revision,
            holes: cache.holes.clone(),
            size: [360, 240],
        };
        let map = cache
            .map
            .resample_tiled([360, 240], &stamp.holes, &app.resize_generation, 0)
            .unwrap();
        let (tx, rx) = mpsc::channel();
        tx.send(ResizeOutput {
            stamp: stamp.clone(),
            map,
        })
        .unwrap();
        app.resize_target = Some((stamp, Instant::now()));
        app.resize_rx = Some(rx);
        app.coordinator_update_pending = true;
        app.layout(size, 2.0, &ctx);
        assert_eq!(
            app.map_cache.as_ref().unwrap().map.physical_size(),
            [180, 120]
        );
        assert!(app.resize_rx.is_some());
        // The coordinator may publish a patch in the meantime; its frame must
        // never be overwritten by the older background resample.
        app.map_cache.as_mut().unwrap().frame_revision += 1;
        app.coordinator_update_pending = false;
        app.layout(size, 2.0, &ctx);
        assert_eq!(
            app.map_cache.as_ref().unwrap().map.physical_size(),
            [180, 120]
        );
        assert!(app.resize_rx.is_none());
    }
    #[test]
    fn failed_resize_wakes_for_one_retry_then_reports_without_hot_loop() {
        let ctx = egui::Context::default();
        let mut app = App::default();
        app.data = Some(Arc::new(snapshot()));
        let size = Vec2::new(180.0, 120.0);
        apply_test_map(&mut app, &ctx, LayoutKey::new(size, 1.0), 1);
        app.layout(size, 2.0, &ctx);
        for expected in 1..=2 {
            let (tx, rx) = mpsc::channel();
            drop(tx);
            app.resize_rx = Some(rx);
            app.layout(size, 2.0, &ctx);
            assert_eq!(app.resize_failures, expected);
        }
        assert!(app.message.as_deref().unwrap().contains("分辨率更新失败"));
        assert!(app.resize_rx.is_none());
        app.layout(size, 2.0, &ctx);
        assert!(app.resize_rx.is_none());
        // A new viewport is a fresh request, not suppressed by the failure.
        app.layout(Vec2::new(200.0, 120.0), 2.0, &ctx);
        assert_eq!(app.resize_failures, 0);
        assert!(app.resize_failed.is_none());
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
        let removed = cache.map.rect(1).unwrap();
        app.selected = Some(1);
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        tx.send(TrashOutput {
            source: source.clone(),
            folder_hidden: None,
            id: Some(1),
            file_change: None,
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
            folder_hidden: None,
            id: Some(1),
            file_change: None,
            result: Ok(()),
        })
        .unwrap();
        app.poll(&ctx);
        settle(&mut app, &ctx);
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
        assert!(cache.map.rect(2).is_some());
        assert_ne!(
            cache
                .map
                .hit_test(removed.x + removed.w * 0.5, removed.y + removed.h * 0.5),
            Some(1)
        );
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
            output
                .textures_delta
                .set
                .iter()
                .all(|(id, delta)| *id != texture || delta.pos.is_some())
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
        // The worker publishes a baked copy of the displayed deletion holes.
        // Finish the preceding transition before exercising the next resize.
        app.map_cache.as_mut().unwrap().transition = None;
        let key = LayoutKey::new(Vec2::new(700.0, 400.0), 2.0);
        let generation = app.layout_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let cache = app.map_cache.as_ref().unwrap();
        let previous = prepare_previous_frame(
            cache.texture.id(),
            cache.frame_revision,
            cache.holes.clone(),
            cache.map.snapshot_for_update(),
            &app.layout_generation,
            generation,
        );
        let mut map = layout_snapshot(
            app.data.as_ref().unwrap(),
            key,
            &app.layout_generation,
            generation,
        )
        .unwrap();
        let holes = map
            .erase_many(&[1], &app.layout_generation, generation)
            .unwrap();
        app.layout_key = Some(key);
        let (tx, rx) = mpsc::channel();
        app.layout_rx = Some(rx);
        tx.send(LayoutOutput {
            generation,
            key,
            map,
            holes,
            partitions: None,
            previous,
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
            mutation_epoch: 0,
        });
        let (tx, rx) = mpsc::channel();
        app.trash_rx = Some(rx);
        tx.send(TrashOutput {
            source,
            folder_hidden: None,
            id: Some(0),
            file_change: None,
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
            mutation_epoch: 0,
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
            mutation_epoch: 0,
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
                parent_identity: scan::FileIdentity::default(),
                parent_is_leaf: false,
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
            directories: Vec::new().into(),
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
