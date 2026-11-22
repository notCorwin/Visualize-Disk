//! Background coordinator for scene versions, per-mode lookup and view updates.
//! UI owns the published frame until it explicitly commits a prepared version.
use crate::{
    directory_map::DirectoryPartition,
    incremental_geometry::IncrementalGeometry,
    incremental_update,
    render::{RasterPatch, RenderMap},
    scan,
    scene_actor::{self, SceneActor, Version},
    scene_model::{Change, Scene, View},
    sparse_projection::SparseProjection,
    treemap::Rect,
};
use eframe::egui::{self, TextureId};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
};

// Conservative safety limit; larger batches use the background rebuild path.
const MAX_BATCH_CHANGES: usize = 4096;

pub struct ModeInput {
    pub base: Version,
    pub mode: View,
    pub data: Arc<scan::ScanResult>,
    pub source_ids: Option<Arc<[Option<usize>]>>,
    pub partitions: Option<Arc<Vec<DirectoryPartition>>>,
    pub retained: Option<Retained>,
    /// Projection slots already hidden in this retained render map.
    pub tombstones: Arc<BTreeSet<usize>>,
    pub map: RenderMap,
    pub layout_generation: u64,
    pub texture: TextureId,
}
pub struct Retained {
    pub geometry: Arc<IncrementalGeometry>,
    pub projection: Arc<SparseProjection>,
}
pub struct ModeBase {
    pub mode: View,
    pub layout_generation: u64,
    pub texture: TextureId,
    pub map: RenderMap,
    pub holes: Arc<Vec<Rect>>,
    pub pixels_per_point: f32,
    pub area_tolerance: f64,
    pub max_patch_pixels: usize,
}
pub enum Changes {
    Explicit(Vec<Change>),
    HiddenDiff {
        before: Arc<BTreeSet<usize>>,
        after: Arc<BTreeSet<usize>>,
    },
}
pub struct UpdateInput {
    pub base: Version,
    pub changes: Changes,
    pub modes: Vec<ModeBase>,
}
pub enum Command {
    InitMode(ModeInput),
    Update(UpdateInput),
    Commit(Version),
}
#[derive(Debug)]
pub enum Failure {
    Stale,
    Pending,
    InvalidMode,
    InvalidHidden,
    TooManyChanges,
    Lookup,
    Scene,
    Geometry,
    Disconnected,
    Cancelled,
}
pub struct ModeReady {
    pub mode: View,
    pub version: Version,
    pub layout_generation: u64,
    pub texture: TextureId,
    pub projection: Arc<SparseProjection>,
    pub geometry: Arc<IncrementalGeometry>,
}
pub struct ModeUpdate {
    pub mode: View,
    pub layout_generation: u64,
    pub texture: TextureId,
    pub projection: Arc<SparseProjection>,
    pub geometry: Arc<IncrementalGeometry>,
    pub map: RenderMap,
    pub patch: Option<RasterPatch>,
    pub holes: Arc<Vec<Rect>>,
}
pub struct Prepared {
    pub base: Version,
    pub version: Version,
    pub views: Vec<ModeUpdate>,
    pub rebuild: Vec<View>,
}
pub enum Event {
    Ready(Version),
    ModeReady(ModeReady),
    Prepared(Prepared),
    Failed(Failure),
}
struct ModeState {
    mode: View,
    layout_generation: u64,
    texture: TextureId,
    geometry: Arc<IncrementalGeometry>,
    projection: Arc<SparseProjection>,
}
struct Pending {
    version: Version,
    states: BTreeMap<u8, ModeState>,
}
fn actor_failure(error: scene_actor::Failure) -> Failure {
    match error {
        scene_actor::Failure::Stale { current } => {
            let _ = current;
            Failure::Stale
        }
        scene_actor::Failure::Pending => Failure::Pending,
        scene_actor::Failure::Invalid(reason) => {
            let _ = reason;
            Failure::Scene
        }
        scene_actor::Failure::InvalidCommit => Failure::Stale,
        scene_actor::Failure::InvalidProjection => Failure::Lookup,
    }
}
fn mode_key(mode: View) -> u8 {
    match mode {
        View::Files => 0,
        View::Folders => 1,
        View::Hybrid => 2,
    }
}

pub struct Coordinator {
    tx: SyncSender<Command>,
    rx: Receiver<Event>,
    cancelled: Arc<AtomicBool>,
    token: Arc<AtomicU64>,
}
impl Drop for Coordinator {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.token.fetch_add(1, Ordering::Relaxed);
    }
}
impl Coordinator {
    /// Starts O(1) on the caller; index construction and all updates are done in the worker.
    #[cfg(test)]
    pub fn start(
        source: Arc<scan::ScanResult>,
        source_generation: u64,
        ctx: egui::Context,
    ) -> Self {
        Self::start_with_hidden(source, source_generation, Arc::new(BTreeSet::new()), ctx)
    }
    pub fn start_with_hidden(
        source: Arc<scan::ScanResult>,
        source_generation: u64,
        hidden: Arc<BTreeSet<usize>>,
        ctx: egui::Context,
    ) -> Self {
        let (tx, commands) = mpsc::sync_channel(4);
        let (events, rx) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        let token = Arc::new(AtomicU64::new(0));
        let work_token = token.clone();
        thread::Builder::new()
            .name("scene-coordinator".into())
            .spawn(move || {
                let Some(mut scene) = Scene::from_scan_cancellable(source.clone(), &stop) else {
                    return;
                };
                for (i, &id) in hidden.iter().enumerate() {
                    if i % 4096 == 0 && stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let Some(file) = source.files.get(id) else {
                        let _ = events.send(Event::Failed(Failure::InvalidHidden));
                        ctx.request_repaint();
                        return;
                    };
                    if scene
                        .apply(Change::RemoveFile {
                            path: file.path.clone(),
                            identity: file.identity,
                        })
                        .is_err()
                    {
                        let _ = events.send(Event::Failed(Failure::InvalidHidden));
                        ctx.request_repaint();
                        return;
                    }
                }
                let actor = SceneActor::start_with_cancel(
                    scene,
                    source_generation,
                    ctx.clone(),
                    stop.clone(),
                );
                let mut version = Version {
                    source_generation,
                    scene_version: 0,
                };
                let mut states: BTreeMap<u8, ModeState> = BTreeMap::new();
                let mut pending: Option<Pending> = None;
                if events.send(Event::Ready(version)).is_err() {
                    return;
                }
                ctx.request_repaint();
                while let Ok(command) = commands.recv() {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let event = match command {
                        Command::Commit(requested) => {
                            if pending.as_ref().is_none_or(|p| p.version != requested) {
                                Some(Event::Failed(Failure::Stale))
                            } else if actor.try_commit(requested).is_err() {
                                Some(Event::Failed(Failure::Disconnected))
                            } else {
                                let next = pending.take().unwrap();
                                states = next.states;
                                version = requested;
                                None
                            }
                        }
                        Command::InitMode(input) => {
                            if pending.is_some() {
                                Some(Event::Failed(Failure::Pending))
                            } else if input.base != version {
                                Some(Event::Failed(Failure::Stale))
                            } else {
                                let mode = input.mode;
                                let request = scene_actor::ProjectionLookupRequest {
                                    base: version,
                                    data: input.data.clone(),
                                    mode,
                                    source_ids: input.source_ids,
                                };
                                let result = actor
                                    .try_lookup_projection(request)
                                    .map_err(|_| Failure::Disconnected)
                                    .and_then(|()| match actor.recv() {
                                        Ok(scene_actor::Event::ProjectionLookup(found))
                                            if found.base == version && found.mode == mode =>
                                        {
                                            Ok(found.entries)
                                        }
                                        Ok(scene_actor::Event::Failed(reason)) => {
                                            Err(actor_failure(reason))
                                        }
                                        _ => Err(Failure::Lookup),
                                    })
                                    .and_then(|items| {
                                        if input.tombstones.iter().any(|&slot| slot >= items.len())
                                            || items.iter().enumerate().any(|(slot, id)| {
                                                id.is_none() != input.tombstones.contains(&slot)
                                            })
                                        {
                                            return Err(Failure::Lookup);
                                        }
                                        let missing: std::collections::HashSet<usize> =
                                            input.tombstones.iter().copied().collect();
                                        match input.map.owners_exclude(&missing, &work_token, 0) {
                                            Some(true) => {}
                                            Some(false) => return Err(Failure::Lookup),
                                            None => return Err(Failure::Cancelled),
                                        }
                                        let projection = Arc::new(
                                            SparseProjection::new_optional(
                                                input.data,
                                                items.clone(),
                                            )
                                            .map_err(|_| Failure::Lookup)?,
                                        );
                                        let geometry = if let Some(retained) = input.retained {
                                            let mut geometry = (*retained.geometry).clone();
                                            geometry
                                                .reconcile_retained(
                                                    &retained.projection,
                                                    &projection,
                                                )
                                                .map_err(|_| Failure::Geometry)?;
                                            geometry
                                        } else {
                                            let partitions =
                                                input.partitions.ok_or(Failure::Geometry)?;
                                            let weights: Vec<_> = projection
                                                .base()
                                                .files
                                                .iter()
                                                .map(|f| f.size)
                                                .collect();
                                            IncrementalGeometry::from_layout_reconciled(
                                                &partitions,
                                                &items,
                                                &weights,
                                            )
                                            .map_err(|_| Failure::Geometry)?
                                        };
                                        let geometry = Arc::new(geometry);
                                        Ok((geometry, projection))
                                    });
                                match result {
                                    Ok((geometry, projection)) => {
                                        let state = ModeState {
                                            mode,
                                            layout_generation: input.layout_generation,
                                            texture: input.texture,
                                            geometry: geometry.clone(),
                                            projection: projection.clone(),
                                        };
                                        states.insert(mode_key(mode), state);
                                        Some(Event::ModeReady(ModeReady {
                                            mode,
                                            version,
                                            layout_generation: input.layout_generation,
                                            texture: input.texture,
                                            projection,
                                            geometry,
                                        }))
                                    }
                                    Err(error) => Some(Event::Failed(error)),
                                }
                            }
                        }
                        Command::Update(input) => {
                            if pending.is_some() {
                                Some(Event::Failed(Failure::Pending))
                            } else if input.base != version {
                                Some(Event::Failed(Failure::Stale))
                            } else {
                                let mut keys = BTreeSet::new();
                                let valid = input.modes.iter().all(|m| {
                                    keys.insert(mode_key(m.mode))
                                        && states.get(&mode_key(m.mode)).is_some_and(|s| {
                                            s.layout_generation == m.layout_generation
                                                && s.texture == m.texture
                                        })
                                });
                                if !valid {
                                    Some(Event::Failed(Failure::InvalidMode))
                                } else {
                                    let changes = match input.changes {
                                        Changes::Explicit(changes) => {
                                            if changes.len() > MAX_BATCH_CHANGES {
                                                Err(Failure::TooManyChanges)
                                            } else {
                                                Ok(changes)
                                            }
                                        }
                                        Changes::HiddenDiff { before, after } => {
                                            if !before.is_subset(&after) {
                                                Err(Failure::InvalidHidden)
                                            } else if after.len() - before.len() > MAX_BATCH_CHANGES
                                            {
                                                Err(Failure::TooManyChanges)
                                            } else {
                                                after
                                                    .difference(&before)
                                                    .map(|&id| {
                                                        source
                                                            .files
                                                            .get(id)
                                                            .map(|file| Change::RemoveFile {
                                                                path: file.path.clone(),
                                                                identity: file.identity,
                                                            })
                                                            .ok_or(Failure::InvalidHidden)
                                                    })
                                                    .collect()
                                            }
                                        }
                                    };
                                    match changes {
                                        Err(e) => Some(Event::Failed(e)),
                                        Ok(changes) => {
                                            let request = scene_actor::UpdateRequest {
                                                base: version,
                                                changes,
                                            };
                                            match actor
                                                .try_submit(request)
                                                .map_err(|_| Failure::Disconnected)
                                                .and_then(|()| match actor.recv() {
                                                    Ok(scene_actor::Event::Prepared(prepared))
                                                        if prepared.base == version =>
                                                    {
                                                        Ok(prepared)
                                                    }
                                                    Ok(scene_actor::Event::Failed(reason)) => {
                                                        Err(actor_failure(reason))
                                                    }
                                                    _ => Err(Failure::Scene),
                                                }) {
                                                Err(e) => Some(Event::Failed(e)),
                                                Ok(prepared) => {
                                                    let mut next = BTreeMap::new();
                                                    let mut views = Vec::new();
                                                    let mut rebuild = Vec::new();
                                                    let mut old = std::mem::take(&mut states);
                                                    for mode in input.modes {
                                                        let key = mode_key(mode.mode);
                                                        let state = old
                                                            .remove(&key)
                                                            .expect("validated mode");
                                                        let deltas = match mode.mode {
                                                            View::Files => {
                                                                &prepared.projections.files
                                                            }
                                                            View::Folders => {
                                                                &prepared.projections.folders
                                                            }
                                                            View::Hybrid => {
                                                                &prepared.projections.hybrid
                                                            }
                                                        };
                                                        match incremental_update::prepare(
                                                            (*state.geometry).clone(),
                                                            mode.map,
                                                            &state.projection,
                                                            &mode.holes,
                                                            deltas,
                                                            mode.pixels_per_point,
                                                            mode.area_tolerance,
                                                            mode.max_patch_pixels,
                                                            &work_token,
                                                            0,
                                                        ) {
                                                            Ok(updated) => {
                                                                let holes = Arc::new(updated.holes);
                                                                let geometry =
                                                                    Arc::new(updated.geometry);
                                                                let projection =
                                                                    Arc::new(updated.projection);
                                                                next.insert(
                                                                    key,
                                                                    ModeState {
                                                                        mode: state.mode,
                                                                        layout_generation: state
                                                                            .layout_generation,
                                                                        texture: state.texture,
                                                                        geometry: geometry.clone(),
                                                                        projection: projection
                                                                            .clone(),
                                                                    },
                                                                );
                                                                views.push(ModeUpdate {
                                                                    mode: mode.mode,
                                                                    layout_generation: mode
                                                                        .layout_generation,
                                                                    texture: mode.texture,
                                                                    projection,
                                                                    geometry,
                                                                    map: updated.map,
                                                                    patch: updated.patch,
                                                                    holes,
                                                                });
                                                            }
                                                            Err(_) => rebuild.push(mode.mode),
                                                        }
                                                    }
                                                    rebuild
                                                        .extend(old.into_values().map(|s| s.mode));
                                                    pending = Some(Pending {
                                                        version: prepared.version,
                                                        states: next,
                                                    });
                                                    Some(Event::Prepared(Prepared {
                                                        base: version,
                                                        version: prepared.version,
                                                        views,
                                                        rebuild,
                                                    }))
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    };
                    if let Some(event) = event {
                        if events.send(event).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                }
            })
            .expect("spawn scene coordinator");
        Self {
            tx,
            rx,
            cancelled,
            token,
        }
    }
    pub fn try_send(&self, command: Command) -> Result<(), TrySendError<Command>> {
        self.tx.try_send(command)
    }
    pub fn try_recv(&self) -> Result<Event, TryRecvError> {
        self.rx.try_recv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        directory_map,
        scene_model::{self, EntryId},
    };
    use std::{
        path::{Path, PathBuf},
        time::Duration,
    };
    fn identity(n: u64) -> scan::FileIdentity {
        scan::FileIdentity {
            device: n,
            ..Default::default()
        }
    }
    fn fixture() -> Arc<scan::ScanResult> {
        let file = |name: &str, n: u64| scan::FileEntry {
            path: PathBuf::from(format!("/disk/a/{name}")),
            size: 50,
            parent: Arc::from(Path::new("/disk/a")),
            identity: identity(n),
            parent_identity: identity(2),
            parent_is_leaf: true,
            can_trash: true,
        };
        let dir = |path: &str, n: u64, has_subdirectories| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: true,
            has_subdirectories,
        };
        Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("f", 3), file("g", 4)],
            directories: vec![dir("/disk", 1, true), dir("/disk/a", 2, false)].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        })
    }
    fn event(coordinator: &Coordinator) -> Event {
        coordinator
            .rx
            .recv_timeout(Duration::from_secs(10))
            .expect("coordinator response")
    }
    #[test]
    fn hidden_diff_updates_published_view_and_requires_matching_commit() {
        let source = fixture();
        let coordinator = Coordinator::start(source.clone(), 10, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let token = AtomicU64::new(1);
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk/a"), vec![(0, 50), (1, 50)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        let r = map.rect(0).unwrap();
        let at = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let texture = TextureId::Managed(7);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode: View::Files,
                data: source,
                source_ids: None,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::new(BTreeSet::new()),
                map: map.snapshot_for_update(),
                layout_generation: 44,
                texture,
            }))
            .ok()
            .unwrap();
        let Event::ModeReady(ready) = event(&coordinator) else {
            panic!("mode ready")
        };
        assert_eq!(
            ready.projection.id(0),
            Some(scene_model::TileId::File(EntryId {
                slot: 0,
                generation: 0
            }))
        );
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: Arc::new(BTreeSet::new()),
                    after: Arc::new(BTreeSet::from([0])),
                },
                modes: vec![ModeBase {
                    mode: View::Files,
                    layout_generation: 44,
                    texture,
                    map: map.snapshot_for_update(),
                    holes: Arc::new(vec![]),
                    pixels_per_point: 1.0,
                    area_tolerance: 10000.0,
                    max_patch_pixels: 10000,
                }],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = event(&coordinator) else {
            panic!("prepared")
        };
        assert_eq!(prepared.version.scene_version, 1);
        assert!(prepared.rebuild.is_empty());
        let updated = &prepared.views[0];
        assert_eq!(map.hit_test(at.0, at.1), Some(0));
        assert_eq!(updated.map.hit_test(at.0, at.1), None);
        assert!(updated.projection.get(0).is_none());
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![]),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        assert!(matches!(
            event(&coordinator),
            Event::Failed(Failure::Pending)
        ));
        coordinator
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![]),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::Failed(Failure::Stale)));
    }
    #[test]
    fn three_modes_publish_together_and_stale_texture_does_not_advance_scene() {
        let source = fixture();
        let coordinator = Coordinator::start(source.clone(), 13, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let folder = Arc::new(scan::ScanResult {
            root: source.root.clone(),
            files: vec![scan::FileEntry {
                path: "/disk/a".into(),
                size: 100,
                parent: Arc::from(Path::new("/disk")),
                identity: identity(2),
                parent_identity: Default::default(),
                parent_is_leaf: false,
                can_trash: true,
            }],
            directories: source.directories.clone(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let mut inputs = Vec::new();
        for (i, mode) in [View::Files, View::Folders, View::Hybrid]
            .into_iter()
            .enumerate()
        {
            let (data, parent, items, ids) = match mode {
                View::Files => (
                    source.clone(),
                    Path::new("/disk/a"),
                    vec![(0, 50), (1, 50)],
                    None,
                ),
                View::Folders => (folder.clone(), Path::new("/disk"), vec![(0, 100)], None),
                View::Hybrid => (
                    folder.clone(),
                    Path::new("/disk"),
                    vec![(0, 100)],
                    Some(Arc::from([None])),
                ),
            };
            let layout = directory_map::layout_with_partitions(
                [(parent, items)],
                Path::new("/disk"),
                bounds,
                1.0,
                &token,
                1,
            )
            .unwrap();
            let map = RenderMap::build_with_platforms(
                layout.entries,
                layout.platforms,
                bounds,
                [100, 100],
                &token,
                1,
            )
            .unwrap();
            let texture = TextureId::Managed(i as u64);
            coordinator
                .try_send(Command::InitMode(ModeInput {
                    base,
                    mode,
                    data,
                    source_ids: ids,
                    partitions: Some(Arc::new(layout.partitions)),
                    retained: None,
                    tombstones: Arc::new(BTreeSet::new()),
                    map: map.snapshot_for_update(),
                    layout_generation: 10 + i as u64,
                    texture,
                }))
                .ok()
                .unwrap();
            assert!(matches!(event(&coordinator), Event::ModeReady(_)));
            inputs.push(ModeBase {
                mode,
                layout_generation: 10 + i as u64,
                texture,
                map: map.snapshot_for_update(),
                holes: Arc::new(vec![]),
                pixels_per_point: 1.0,
                area_tolerance: 10000.0,
                max_patch_pixels: 10000,
            });
        }
        let stale = ModeBase {
            mode: View::Files,
            layout_generation: 10,
            texture: TextureId::Managed(999),
            map: inputs[0].map.snapshot_for_update(),
            holes: Arc::new(vec![]),
            pixels_per_point: 1.0,
            area_tolerance: 10000.0,
            max_patch_pixels: 10000,
        };
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![]),
                modes: vec![stale],
            }))
            .ok()
            .unwrap();
        assert!(matches!(
            event(&coordinator),
            Event::Failed(Failure::InvalidMode)
        ));
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: Arc::new(BTreeSet::new()),
                    after: Arc::new(BTreeSet::from([0])),
                },
                modes: inputs,
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = event(&coordinator) else {
            panic!("prepared")
        };
        assert_eq!(prepared.views.len(), 3);
        assert!(prepared.rebuild.is_empty());
        for view in prepared.views {
            match view.mode {
                View::Files => assert!(view.projection.get(0).is_none()),
                _ => assert_eq!(view.projection.get(0).unwrap().size, 50),
            }
        }
    }
    #[test]
    fn hidden_restart_requires_erased_owners_and_reuses_vacant_layout() {
        let source = fixture();
        let coordinator = Coordinator::start_with_hidden(
            source.clone(),
            14,
            Arc::new(BTreeSet::from([0])),
            egui::Context::default(),
        );
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let token = AtomicU64::new(1);
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk/a"), vec![(0, 50), (1, 50)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let mut map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        let texture = TextureId::Managed(8);
        let input = |map: RenderMap| ModeInput {
            base,
            mode: View::Files,
            data: source.clone(),
            source_ids: None,
            partitions: Some(Arc::new(layout.partitions.clone())),
            retained: None,
            tombstones: Arc::new(BTreeSet::from([0])),
            map,
            layout_generation: 3,
            texture,
        };
        coordinator
            .try_send(Command::InitMode(input(map.snapshot_for_update())))
            .ok()
            .unwrap();
        assert!(matches!(
            event(&coordinator),
            Event::Failed(Failure::Lookup)
        )); // visible deleted owner
        map.erase(0);
        coordinator
            .try_send(Command::InitMode(input(map.snapshot_for_update())))
            .ok()
            .unwrap();
        let Event::ModeReady(ready) = event(&coordinator) else {
            panic!("mode ready")
        };
        assert!(ready.projection.get(0).is_none());
        assert!(ready.projection.get(1).is_some());
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: Arc::new(BTreeSet::from([0])),
                    after: Arc::new(BTreeSet::from([0, 1])),
                },
                modes: vec![ModeBase {
                    mode: View::Files,
                    layout_generation: 3,
                    texture,
                    map: map.snapshot_for_update(),
                    holes: Arc::new(vec![]),
                    pixels_per_point: 1.0,
                    area_tolerance: 10000.0,
                    max_patch_pixels: 10000,
                }],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = event(&coordinator) else {
            panic!("prepared after restart")
        };
        assert!(prepared.rebuild.is_empty());
        assert!(prepared.views[0].projection.get(1).is_none());
    }
    #[test]
    fn retained_geometry_recovers_after_interrupted_delete_without_new_texture() {
        let source = fixture();
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let token = AtomicU64::new(1);
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk/a"), vec![(0, 50), (1, 50)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let mut map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        let texture = TextureId::Managed(10);
        let first = Coordinator::start(source.clone(), 16, egui::Context::default());
        let Event::Ready(base) = event(&first) else {
            panic!("ready")
        };
        first
            .try_send(Command::InitMode(ModeInput {
                base,
                mode: View::Files,
                data: source.clone(),
                source_ids: None,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::new(BTreeSet::new()),
                map: map.snapshot_for_update(),
                layout_generation: 5,
                texture,
            }))
            .ok()
            .unwrap();
        let Event::ModeReady(ready) = event(&first) else {
            panic!("initial mode")
        };
        drop(first);
        map.erase(0);
        let second = Coordinator::start_with_hidden(
            source.clone(),
            17,
            Arc::new(BTreeSet::from([0])),
            egui::Context::default(),
        );
        let Event::Ready(base) = event(&second) else {
            panic!("ready after restart")
        };
        second
            .try_send(Command::InitMode(ModeInput {
                base,
                mode: View::Files,
                data: source,
                source_ids: None,
                partitions: None,
                retained: Some(Retained {
                    geometry: ready.geometry,
                    projection: ready.projection,
                }),
                tombstones: Arc::new(BTreeSet::from([0])),
                map: map.snapshot_for_update(),
                layout_generation: 5,
                texture,
            }))
            .ok()
            .unwrap();
        let Event::ModeReady(recovered) = event(&second) else {
            panic!("recovered mode")
        };
        assert_eq!(recovered.texture, texture);
        assert!(recovered.projection.get(0).is_none());
        second
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: Arc::new(BTreeSet::from([0])),
                    after: Arc::new(BTreeSet::from([0, 1])),
                },
                modes: vec![ModeBase {
                    mode: View::Files,
                    layout_generation: 5,
                    texture,
                    map: map.snapshot_for_update(),
                    holes: Arc::new(vec![]),
                    pixels_per_point: 1.0,
                    area_tolerance: 10000.0,
                    max_patch_pixels: 10000,
                }],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(result) = event(&second) else {
            panic!("prepared")
        };
        assert!(result.rebuild.is_empty());
        assert_eq!(result.views[0].texture, texture);
        assert!(result.views[0].projection.get(1).is_none());
    }
    #[test]
    fn retained_folder_geometry_reconciles_current_aggregate_weight() {
        let source = fixture();
        let folder = |size| {
            Arc::new(scan::ScanResult {
                root: source.root.clone(),
                files: vec![scan::FileEntry {
                    path: "/disk/a".into(),
                    parent: Arc::from(Path::new("/disk")),
                    size,
                    identity: identity(2),
                    parent_identity: Default::default(),
                    parent_is_leaf: false,
                    can_trash: true,
                }],
                directories: source.directories.clone(),
                skipped: 0,
                errors: vec![],
                elapsed: Duration::ZERO,
            })
        };
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        let token = AtomicU64::new(1);
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk"), vec![(0, 100)])],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        let texture = TextureId::Managed(13);
        let first = Coordinator::start(source.clone(), 18, egui::Context::default());
        let Event::Ready(base) = event(&first) else {
            panic!("ready")
        };
        first
            .try_send(Command::InitMode(ModeInput {
                base,
                mode: View::Folders,
                data: folder(100),
                source_ids: None,
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::new(BTreeSet::new()),
                map: map.snapshot_for_update(),
                layout_generation: 8,
                texture,
            }))
            .ok()
            .unwrap();
        let Event::ModeReady(ready) = event(&first) else {
            panic!("mode")
        };
        drop(first);
        let second = Coordinator::start_with_hidden(
            source.clone(),
            19,
            Arc::new(BTreeSet::from([0])),
            egui::Context::default(),
        );
        let Event::Ready(base) = event(&second) else {
            panic!("restarted")
        };
        second
            .try_send(Command::InitMode(ModeInput {
                base,
                mode: View::Folders,
                data: folder(50),
                source_ids: None,
                partitions: None,
                retained: Some(Retained {
                    geometry: ready.geometry,
                    projection: ready.projection,
                }),
                tombstones: Arc::new(BTreeSet::new()),
                map: map.snapshot_for_update(),
                layout_generation: 8,
                texture,
            }))
            .ok()
            .unwrap();
        let Event::ModeReady(recovered) = event(&second) else {
            panic!("recovered")
        };
        assert_eq!(recovered.projection.get(0).unwrap().size, 50);
        second
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: Arc::new(BTreeSet::from([0])),
                    after: Arc::new(BTreeSet::from([0, 1])),
                },
                modes: vec![ModeBase {
                    mode: View::Folders,
                    layout_generation: 8,
                    texture,
                    map: map.snapshot_for_update(),
                    holes: Arc::new(vec![]),
                    pixels_per_point: 1.0,
                    area_tolerance: 10000.0,
                    max_patch_pixels: 10000,
                }],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(result) = event(&second) else {
            panic!("prepared")
        };
        assert!(result.rebuild.is_empty());
        assert!(result.views[0].projection.get(0).is_none());
    }
    #[test]
    fn hidden_restart_rejects_unknown_source_indices() {
        let coordinator = Coordinator::start_with_hidden(
            fixture(),
            15,
            Arc::new(BTreeSet::from([999])),
            egui::Context::default(),
        );
        assert!(matches!(
            event(&coordinator),
            Event::Failed(Failure::InvalidHidden)
        ));
    }
    #[test]
    fn oversized_batch_falls_back_without_advancing_scene() {
        let coordinator = Coordinator::start(fixture(), 12, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: Arc::new(BTreeSet::new()),
                    after: Arc::new((0..=MAX_BATCH_CHANGES).collect()),
                },
                modes: vec![],
            }))
            .ok()
            .unwrap();
        assert!(matches!(
            event(&coordinator),
            Event::Failed(Failure::TooManyChanges)
        ));
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![]),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::Prepared(_)));
    }
    #[test]
    fn invalid_hidden_diff_does_not_advance_scene() {
        let coordinator = Coordinator::start(fixture(), 11, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: Arc::new(BTreeSet::from([1])),
                    after: Arc::new(BTreeSet::new()),
                },
                modes: vec![],
            }))
            .ok()
            .unwrap();
        assert!(matches!(
            event(&coordinator),
            Event::Failed(Failure::InvalidHidden)
        ));
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![]),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::Prepared(_)));
    }
}
