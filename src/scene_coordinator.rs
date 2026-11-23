//! Background coordinator for scene versions, per-mode lookup and view updates.
//! UI owns the published frame until it explicitly commits a prepared version.
use crate::{
    directory_map::DirectoryPartition,
    incremental_geometry::{self, IncrementalGeometry},
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

/// Removing a leaf eliminates its own platform in Files mode. In Hybrid mode
/// a newly leaf parent changes its projection and platform topology. These
/// cases require a fresh layout rather than reusing the old platform raster.
fn requires_structural_rebuild(mode: View, changes: &[Change]) -> bool {
    changes.iter().any(|change| match change {
        Change::RemoveLeafDirectory { parent, .. } => {
            mode == View::Files || (mode == View::Hybrid && !parent.has_subdirectories)
        }
        // A non-leaf subtree has directory platforms in every view. Rebuild
        // only its fixed-ancestor region, including platform/light changes.
        Change::RemoveSubtreeDirectory { .. } | Change::RemoveConfirmedSubtree { .. } => true,
        _ => false,
    })
}

/// Choose a fixed ancestor containing both old and new locations. This also
/// handles an empty map gaining its first file (no prior parent partition).
fn delta_scope(deltas: &[scene_actor::TileDelta], root: &std::path::Path) -> std::path::PathBuf {
    let mut scope: Option<std::path::PathBuf> = None;
    for tile in deltas
        .iter()
        .flat_map(|delta| delta.before.iter().chain(delta.after.iter()))
    {
        let path = tile.parent.as_path();
        scope = Some(match scope {
            None => path.to_path_buf(),
            Some(mut ancestor) => {
                while !path.starts_with(&ancestor) && ancestor.pop() {}
                ancestor
            }
        });
    }
    scope.unwrap_or_else(|| root.to_path_buf())
}

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
    Reconcile(Arc<scan::ScanResult>),
    HiddenDiff {
        before: Arc<BTreeSet<usize>>,
        after: Arc<BTreeSet<usize>>,
        /// The final source index published in the preceding chunk, if any.
        cursor: Option<usize>,
    },
}
/// Progress is tied to the exact before/after snapshots supplied with the
/// first chunk. Callers must commit a prepared chunk before requesting the
/// next one and keep those Arc snapshots unchanged until `done`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HiddenProgress {
    pub cursor: Option<usize>,
    pub done: bool,
}
pub struct UpdateInput {
    pub base: Version,
    pub changes: Changes,
    pub modes: Vec<ModeBase>,
}
pub enum Command {
    InitMode(ModeInput),
    ExportMode { base: Version, mode: View },
    Checkpoint { base: Version },
    Update(UpdateInput),
    Commit(Version),
}
#[derive(Debug)]
pub enum Failure {
    Stale,
    Pending,
    InvalidMode,
    InvalidHidden,
    Lookup,
    Scene,
    Reconcile(crate::snapshot_diff::Error),
    Recovery(crate::scene_journal::Error),
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
    pub patches: Vec<RasterPatch>,
    pub holes: Arc<Vec<Rect>>,
    pub transitions: Vec<incremental_update::Transition>,
}
pub struct Prepared {
    pub base: Version,
    pub version: Version,
    pub views: Vec<ModeUpdate>,
    pub rebuild: Vec<View>,
    pub hidden_progress: Option<HiddenProgress>,
    pub reconciled: bool,
    /// Exact published transaction for recovering the stable scene identities.
    pub journal: Arc<crate::scene_journal::Record>,
}
pub enum Event {
    Ready(Version),
    ModeReady(ModeReady),
    Checkpoint {
        base: Version,
        checkpoint: crate::scene_journal::Checkpoint,
    },
    /// A mode not yet displayed may be created from the authoritative scene
    /// without replacing the baseline snapshot or rebuilding existing modes.
    ModeSnapshot {
        base: Version,
        mode: View,
        data: Arc<scan::ScanResult>,
        source_ids: Option<Arc<[Option<usize>]>>,
    },
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
    hidden_checkpoint: Option<HiddenCheckpoint>,
}
struct HiddenCheckpoint {
    before: Arc<BTreeSet<usize>>,
    after: Arc<BTreeSet<usize>>,
    cursor: usize,
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
        scene_actor::Failure::Reconcile(reason) => Failure::Reconcile(reason),
    }
}
fn mode_key(mode: View) -> u8 {
    match mode {
        View::Files => 0,
        View::Folders => 1,
        View::Hybrid => 2,
    }
}

/// Retired identities are needed while a file is merely hidden by Hybrid's
/// leaf aggregation, but must not accumulate after the underlying file was
/// actually removed. File-mode deltas are authoritative for that distinction.
fn deleted_file_ids(deltas: &[scene_actor::TileDelta]) -> Vec<crate::scene_model::TileId> {
    use crate::scene_model::TileId;
    let remaining: std::collections::HashSet<_> = deltas
        .iter()
        .filter_map(|delta| delta.after.as_ref().map(|tile| tile.id))
        .collect();
    deltas
        .iter()
        .filter_map(|delta| {
            let id = delta.before.as_ref()?.id;
            (matches!(id, TileId::File(_)) && !remaining.contains(&id)).then_some(id)
        })
        .collect()
}

fn verify_projection(
    actor: &SceneActor,
    base: Version,
    mode: View,
    projection: &Arc<SparseProjection>,
    map: &RenderMap,
    token: &AtomicU64,
) -> Result<(), Failure> {
    actor
        .try_verify_projection(scene_actor::ProjectionVerifyRequest {
            base,
            mode,
            projection: projection.clone(),
        })
        .map_err(|_| Failure::Disconnected)?;
    match actor.recv() {
        Ok(scene_actor::Event::ProjectionVerified {
            base: returned,
            mode: returned_mode,
            valid: true,
        }) if returned == base && returned_mode == mode => {}
        Ok(scene_actor::Event::Failed(reason)) => return Err(actor_failure(reason)),
        _ => return Err(Failure::Lookup),
    }
    match map.owners_satisfy(|slot| projection.get(slot).is_some(), token, 0) {
        Some(true) => Ok(()),
        Some(false) => Err(Failure::Lookup),
        None => Err(Failure::Cancelled),
    }
}

/// Apply a large hidden-set delta to private scene versions and merge the
/// resulting patches by physical tile. The published UI frame stays untouched
/// until the final prepared transaction is committed. Working memory for
/// pixels is bounded by the rendered texture rather than the number of chunks.
fn prepare_large_hidden(
    actor: &SceneActor,
    base: Version,
    source: &scan::ScanResult,
    before: &BTreeSet<usize>,
    after: &BTreeSet<usize>,
    modes: &[ModeBase],
    states: &mut BTreeMap<u8, ModeState>,
    token: &AtomicU64,
) -> Result<(Version, Vec<ModeUpdate>, Vec<View>), Failure> {
    if !before.is_subset(after) || after.last().is_some_and(|id| *id >= source.files.len()) {
        return Err(Failure::InvalidHidden);
    }
    let mut accumulators: Vec<_> = modes
        .iter()
        .map(|mode| {
            incremental_update::PatchAccumulator::new(&mode.map, &mode.holes, token, 0)
                .map_err(|_| Failure::Geometry)
        })
        .collect::<Result<_, _>>()?;
    let mut working: Vec<_> = modes
        .iter()
        .map(|mode| ModeBase {
            mode: mode.mode,
            layout_generation: mode.layout_generation,
            texture: mode.texture,
            map: mode.map.snapshot_for_update(),
            holes: mode.holes.clone(),
            pixels_per_point: mode.pixels_per_point,
            area_tolerance: mode.area_tolerance,
            max_patch_pixels: mode.max_patch_pixels,
        })
        .collect();
    let mut ids = after.difference(before).copied().peekable();
    let mut current = base;
    loop {
        if token.load(Ordering::Relaxed) != 0 {
            return Err(Failure::Cancelled);
        }
        let changes: Vec<_> = ids
            .by_ref()
            .take(MAX_BATCH_CHANGES)
            .map(|id| {
                let file = &source.files[id];
                Change::RemoveFile {
                    path: file.path.clone(),
                    identity: file.identity,
                }
            })
            .collect();
        let more = ids.peek().is_some();
        actor
            .try_submit(scene_actor::UpdateRequest {
                base: current,
                changes,
            })
            .map_err(|_| Failure::Disconnected)?;
        let prepared = match actor.recv() {
            Ok(scene_actor::Event::Prepared(p)) if p.base == current => p,
            Ok(scene_actor::Event::Failed(e)) => return Err(actor_failure(e)),
            _ => return Err(Failure::Scene),
        };
        let deleted_files = deleted_file_ids(&prepared.projections.files);
        for (index, mode) in working.iter_mut().enumerate() {
            let state = states
                .get_mut(&mode_key(mode.mode))
                .ok_or(Failure::InvalidMode)?;
            let deltas = match mode.mode {
                View::Files => &prepared.projections.files,
                View::Folders => &prepared.projections.folders,
                View::Hybrid => &prepared.projections.hybrid,
            };
            let updated = incremental_update::prepare_batched(
                (*state.geometry).clone(),
                mode.map.snapshot_for_update(),
                &state.projection,
                &mode.holes,
                deltas,
                mode.pixels_per_point,
                mode.area_tolerance,
                mode.max_patch_pixels,
                token,
                0,
            );
            let mut updated = match updated {
                Err(incremental_update::Error::Geometry(
                    incremental_geometry::Error::MissingPartition
                    | incremental_geometry::Error::Rebuild,
                ))
                | Err(incremental_update::Error::Rebuild) => {
                    // Ordinary updates can invalidate a partition when the
                    // final direct file disappears. Retry against the same
                    // private map and projection; never publish an intermediate
                    // chunk or advance the UI-visible scene version.
                    let scope = delta_scope(deltas, &state.projection.base().root);
                    incremental_update::prepare_structural_with_empty_root_proof(
                        &state.geometry,
                        mode.map.snapshot_for_update(),
                        &state.projection,
                        &mode.holes,
                        deltas,
                        &scope,
                        mode.pixels_per_point,
                        mode.area_tolerance,
                        mode.max_patch_pixels,
                        token,
                        0,
                        &prepared.structural_paths,
                        prepared.root_facts_touched,
                        prepared.root_identity_stable,
                        &prepared.visual_noop_directory_paths,
                        prepared.empty_root_replacement_safe,
                        false,
                    )
                }
                other => other,
            }
            .map_err(|_| Failure::Geometry)?;
            updated.projection.forget_deleted_files(&deleted_files);
            accumulators[index]
                .push(&updated.patches)
                .map_err(|_| Failure::Geometry)?;
            mode.map = updated.map;
            mode.holes = Arc::new(updated.holes);
            state.geometry = Arc::new(updated.geometry);
            state.projection = Arc::new(updated.projection);
        }
        current = prepared.version;
        if !more {
            break;
        }
        actor
            .try_commit(current)
            .map_err(|_| Failure::Disconnected)?;
    }
    let active: BTreeSet<_> = modes.iter().map(|m| mode_key(m.mode)).collect();
    let rebuild: Vec<_> = states
        .iter()
        .filter_map(|(k, s)| (!active.contains(k)).then_some(s.mode))
        .collect();
    states.retain(|k, _| active.contains(k));
    let mut views = Vec::with_capacity(working.len());
    for (mode, accumulator) in working.into_iter().zip(accumulators) {
        let state = states
            .get(&mode_key(mode.mode))
            .ok_or(Failure::InvalidMode)?;
        let (patches, transitions) = accumulator
            .finish(&mode.map)
            .map_err(|_| Failure::Geometry)?;
        let mut holes = (*mode.holes).clone();
        for patch in &patches {
            holes = incremental_update::clip_holes(
                &holes,
                mode.map.bounds,
                mode.map.physical_size(),
                patch.origin,
                patch.image.size,
            )
            .ok_or(Failure::Geometry)?;
        }
        views.push(ModeUpdate {
            mode: mode.mode,
            layout_generation: mode.layout_generation,
            texture: mode.texture,
            projection: state.projection.clone(),
            geometry: state.geometry.clone(),
            map: mode.map,
            patches,
            holes: Arc::new(holes),
            transitions,
        });
    }
    Ok((current, views, rebuild))
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
        Self::start_inner(source, source_generation, hidden, None, ctx)
    }
    /// Recover a previously published scene off-thread. Its exact journal is
    /// replayed before any mode is reattached to the still-displayed texture.
    pub fn recover(
        source: Arc<scan::ScanResult>,
        source_generation: u64,
        journal: crate::scene_journal::Journal,
        ctx: egui::Context,
    ) -> Self {
        Self::start_inner(
            source,
            source_generation,
            Arc::new(BTreeSet::new()),
            Some(journal),
            ctx,
        )
    }
    fn start_inner(
        source: Arc<scan::ScanResult>,
        source_generation: u64,
        hidden: Arc<BTreeSet<usize>>,
        journal: Option<crate::scene_journal::Journal>,
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
                let (mut scene, mut version) = if let Some(journal) = journal {
                    match journal.replay(&stop) {
                        Ok(restored) if restored.version.source_generation == source_generation => (restored.scene, restored.version),
                        Ok(_) => { let _ = events.send(Event::Failed(Failure::Stale)); ctx.request_repaint(); return; }
                        Err(error) => { let _ = events.send(Event::Failed(Failure::Recovery(error))); ctx.request_repaint(); return; }
                    }
                } else {
                    let Some(scene) = Scene::from_scan_cancellable(source.clone(), &stop) else { return; };
                    (scene, Version { source_generation, scene_version: 0 })
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
                let actor = SceneActor::start_with_version(scene, version, ctx.clone(), stop.clone());
                let mut states: BTreeMap<u8, ModeState> = BTreeMap::new();
                let mut pending: Option<Pending> = None;
                let mut hidden_checkpoint: Option<HiddenCheckpoint> = None;
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
                                hidden_checkpoint = next.hidden_checkpoint;
                                version = requested;
                                None
                            }
                        }
                        Command::Checkpoint { base } => {
                            if pending.is_some() {
                                Some(Event::Failed(Failure::Pending))
                            } else if base != version {
                                Some(Event::Failed(Failure::Stale))
                            } else if actor.try_checkpoint(scene_actor::CheckpointRequest { base }).is_err() {
                                Some(Event::Failed(Failure::Disconnected))
                            } else {
                                match actor.recv() {
                                    Ok(scene_actor::Event::Checkpoint { base: returned, checkpoint }) if returned == base =>
                                        Some(Event::Checkpoint { base, checkpoint }),
                                    Ok(scene_actor::Event::Failed(reason)) => Some(Event::Failed(actor_failure(reason))),
                                    _ => Some(Event::Failed(Failure::Scene)),
                                }
                            }
                        }
                        Command::ExportMode { base, mode } => {
                            if pending.is_some() {
                                Some(Event::Failed(Failure::Pending))
                            } else if base != version {
                                Some(Event::Failed(Failure::Stale))
                            } else if actor.try_export_projection(scene_actor::ExportProjectionRequest { base, mode }).is_err() {
                                Some(Event::Failed(Failure::Disconnected))
                            } else {
                                match actor.recv() {
                                    Ok(scene_actor::Event::ExportProjection { base: returned, mode: returned_mode, root, tiles, directories })
                                        if returned == base && returned_mode == mode => {
                                            let source_ids = (mode == View::Hybrid).then(|| {
                                                tiles.iter().map(|tile| match tile.id {
                                                    crate::scene_model::TileId::File(_) => Some(0),
                                                    crate::scene_model::TileId::Directory(_) => None,
                                                }).collect::<Vec<_>>().into()
                                            });
                                            let files = tiles.into_iter().map(|tile| tile.to_entry()).collect();
                                            Some(Event::ModeSnapshot { base, mode, data: Arc::new(scan::ScanResult {
                                                root, files, directories, skipped: 0, errors: Vec::new(), elapsed: std::time::Duration::ZERO,
                                            }), source_ids })
                                        }
                                    Ok(scene_actor::Event::Failed(reason)) => Some(Event::Failed(actor_failure(reason))),
                                    _ => Some(Event::Failed(Failure::Scene)),
                                }
                            }
                        }
                        Command::InitMode(input) => {
                            if pending.is_some() {
                                Some(Event::Failed(Failure::Pending))
                            } else if input.base != version {
                                Some(Event::Failed(Failure::Stale))
                            } else {
                                let mode = input.mode;
                                let retained = input.retained;
                                let direct = retained.as_ref().map(|r| {
                                    verify_projection(&actor, version, mode, &r.projection, &input.map, &work_token)
                                });
                                let result = match direct {
                                Some(Ok(())) => {
                                    let retained = retained.expect("verified retained view");
                                    Ok((retained.geometry, retained.projection))
                                }
                                Some(Err(error)) if !matches!(error, Failure::Lookup) => Err(error),
                                _ => {
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
                                        // Lookup of individual slots is insufficient: verify
                                        // the candidate includes *every* tile in the current scene
                                        // before reusing retained geometry.
                                        verify_projection(&actor, version, mode, &projection, &input.map, &work_token)?;
                                        let geometry = if let Some(retained) = retained {
                                            let mut geometry = (*retained.geometry).clone();
                                            geometry.reconcile_retained(&retained.projection, &projection)
                                                .map_err(|_| Failure::Geometry)?;
                                            geometry
                                        } else {
                                            let partitions = input.partitions.ok_or(Failure::Geometry)?;
                                            let weights: Vec<_> = projection.base().files.iter().map(|f| f.size).collect();
                                            IncrementalGeometry::from_layout_reconciled(
                                                &partitions, &items, &weights,
                                            ).map_err(|_| Failure::Geometry)?
                                        };
                                        let geometry = Arc::new(geometry);
                                        Ok((geometry, projection))
                                    });
                                result
                                }};
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
                                    let large_hidden = match &input.changes {
                                        Changes::HiddenDiff { before, after, cursor: None }
                                            if hidden_checkpoint.is_none()
                                                && after.len().saturating_sub(before.len()) > MAX_BATCH_CHANGES =>
                                            Some((before.clone(), after.clone())),
                                        _ => None,
                                    };
                                    if let Some((before, after)) = large_hidden {
                                        let journal = Arc::new(crate::scene_journal::Record::Hidden {
                                            before: before.clone(), after: after.clone(),
                                        });
                                        let mut next = std::mem::take(&mut states);
                                        match prepare_large_hidden(&actor, version, &source, &before,
                                            &after, &input.modes, &mut next, &work_token) {
                                            Err(error) => Some(Event::Failed(error)),
                                            Ok((prepared_version, views, rebuild)) => {
                                                pending = Some(Pending { version: prepared_version,
                                                    states: next, hidden_checkpoint: None });
                                                Some(Event::Prepared(Prepared { base: version,
                                                    version: prepared_version, views, rebuild,
                                                    hidden_progress: Some(HiddenProgress { cursor: None, done: true }),
                                                    reconciled: false, journal }))
                                            }
                                        }
                                    } else {
                                    let mut reconcile = None;
                                    let hidden_record = match &input.changes {
                                        Changes::HiddenDiff { before, after, .. } => Some(Arc::new(
                                            crate::scene_journal::Record::Hidden { before: before.clone(), after: after.clone() }
                                        )),
                                        _ => None,
                                    };
                                    let changes = match input.changes {
                                        Changes::Reconcile(data) => {
                                            if hidden_checkpoint.is_some() {
                                                Err(Failure::Pending)
                                            } else {
                                                reconcile = Some(data);
                                                Ok((Vec::new(), None, None))
                                            }
                                        }
                                        Changes::Explicit(changes) => {
                                            if hidden_checkpoint.is_some() {
                                                Err(Failure::Pending)
                                            } else {
                                                // Explicit batches may contain dependencies between
                                                // directory and file changes. Keep the complete batch
                                                // together: Scene::apply_batch validates it before any
                                                // mutation, then the resulting views are published once.
                                                Ok((changes, None, None))
                                            }
                                        }
                                        Changes::HiddenDiff {
                                            before,
                                            after,
                                            cursor,
                                        } => {
                                            let valid = match (cursor, &hidden_checkpoint) {
                                                (None, None) => {
                                                    before.is_subset(&after)
                                                        && after.last().is_none_or(|id| {
                                                            *id < source.files.len()
                                                        })
                                                }
                                                (Some(cursor), Some(checkpoint)) => {
                                                    cursor == checkpoint.cursor
                                                        && Arc::ptr_eq(&before, &checkpoint.before)
                                                        && Arc::ptr_eq(&after, &checkpoint.after)
                                                }
                                                _ => false,
                                            };
                                            if !valid {
                                                Err(Failure::InvalidHidden)
                                            } else {
                                                use std::ops::Bound::{Excluded, Unbounded};
                                                let lower = cursor.map_or(Unbounded, Excluded);
                                                let mut ids = after
                                                    .range((lower, Unbounded))
                                                    .copied()
                                                    .filter(|id| !before.contains(id));
                                                let chunk: Vec<_> =
                                                    ids.by_ref().take(MAX_BATCH_CHANGES).collect();
                                                let done = ids.next().is_none();
                                                let next_cursor = chunk.last().copied().or(cursor);
                                                let changes = chunk
                                                    .into_iter()
                                                    .map(|id| {
                                                        let file = &source.files[id];
                                                        Change::RemoveFile {
                                                            path: file.path.clone(),
                                                            identity: file.identity,
                                                        }
                                                    })
                                                    .collect();
                                                let next_checkpoint = if done {
                                                    None
                                                } else {
                                                    Some(HiddenCheckpoint {
                                                        before,
                                                        after,
                                                        cursor: next_cursor
                                                            .expect("nonempty chunk"),
                                                    })
                                                };
                                                Ok((
                                                    changes,
                                                    Some(HiddenProgress {
                                                        cursor: next_cursor,
                                                        done,
                                                    }),
                                                    next_checkpoint,
                                                ))
                                            }
                                        }
                                    };
                                    match changes {
                                        Err(e) => Some(Event::Failed(e)),
                                        Ok((changes, hidden_progress, next_checkpoint)) => {
                                            // Explicit directory facts can create, remove or change
                                            // leaf topology just as a reconciled scan can. Use the
                                            // actor's common scope rather than retaining platforms
                                            // from the previous directory structure.
                                            let explicit_directory_changes = changes.iter().any(|change| matches!(
                                                change,
                                                Change::UpsertDirectory(_) | Change::RemoveDirectory { .. }
                                            ));
                                            let structural_rebuild =
                                                [View::Files, View::Folders, View::Hybrid].map(
                                                    |mode| {
                                                        requires_structural_rebuild(mode, &changes)
                                                    },
                                                );
                                            let removed_leaf_parents: Vec<_> = changes
                                                .iter()
                                                .filter_map(|change| match change {
                                                    Change::RemoveLeafDirectory { parent, .. }
                                                    | Change::RemoveSubtreeDirectory { parent, .. } => Some(parent.path.clone()),
                                                    Change::RemoveConfirmedSubtree { path, .. } => path.parent().map(std::path::Path::to_path_buf),
                                                    _ => None,
                                                })
                                                .collect();
                                            let request = scene_actor::UpdateRequest {
                                                base: version,
                                                changes,
                                            };
                                            let reconciled = reconcile.is_some();
                                            let submitted = if let Some(data) = reconcile {
                                                actor.try_reconcile(scene_actor::ReconcileRequest { base: version, data })
                                                    .map_err(|_| Failure::Disconnected)
                                            } else {
                                                actor.try_submit(request).map_err(|_| Failure::Disconnected)
                                            };
                                            match submitted
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
                                                    let deleted_files = deleted_file_ids(&prepared.projections.files);
                                                    let journal = hidden_record.unwrap_or_else(|| Arc::new(
                                                        if prepared.sequential {
                                                            crate::scene_journal::Record::Ordered(prepared.applied.clone())
                                                        } else {
                                                            crate::scene_journal::Record::Batch(prepared.applied.clone())
                                                        }
                                                    ));
                                                    let mut next = BTreeMap::new();
                                                    let mut views = Vec::new();
                                                    let mut rebuild = Vec::new();
                                                    let mut old = std::mem::take(&mut states);
                                                    for mode in input.modes {
                                                        let key = mode_key(mode.mode);
                                                        let state = old
                                                            .remove(&key)
                                                            .expect("validated mode");
                                                        let structural = ((reconciled || explicit_directory_changes) && prepared.structural_scope.is_some()) || structural_rebuild
                                                            [key as usize]
                                                            || removed_leaf_parents.iter().any(
                                                                |path| {
                                                                    !state
                                                                        .geometry
                                                                        .can_compact_leaf_parent(
                                                                            path,
                                                                        )
                                                                },
                                                            );
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
                                                        // The actor has already reduced all directory
                                                        // changes in the batch to a common ancestor.
                                                        let scope = prepared.structural_scope.as_deref()
                                                            .or_else(|| removed_leaf_parents.first().map(|p| p.as_path()));
                                                        let retry_map = mode.map.snapshot_for_update();
                                                        let result = if structural && scope.is_some() {
                                                            incremental_update::prepare_structural_with_empty_root_proof(
                                                                &state.geometry,
                                                                mode.map,
                                                                &state.projection,
                                                                &mode.holes,
                                                                deltas,
                                                                scope.expect("structural scope"),
                                                                mode.pixels_per_point,
                                                                mode.area_tolerance,
                                                                mode.max_patch_pixels,
                                                                &work_token,
                                                                0,
                                                                &prepared.structural_paths,
                                                                prepared.root_facts_touched,
                                                                prepared.root_identity_stable,
                                                                &prepared.visual_noop_directory_paths,
                                                                prepared.empty_root_replacement_safe,
                                                                true,
                                                            )
                                                        } else {
                                                            incremental_update::prepare(
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
                                                            )
                                                        };
                                                        let result = match result {
                                                            Err(incremental_update::Error::Geometry(
                                                                incremental_geometry::Error::MissingPartition | incremental_geometry::Error::Rebuild
                                                            )) | Err(incremental_update::Error::Rebuild) if !structural => {
                                                                let scope = delta_scope(deltas, &state.projection.base().root);
                                                                incremental_update::prepare_structural_with_empty_root_proof(
                                                                    &state.geometry, retry_map, &state.projection,
                                                                    &mode.holes, deltas, &scope, mode.pixels_per_point,
                                                                    mode.area_tolerance, mode.max_patch_pixels, &work_token, 0,
                                                                    &prepared.structural_paths, prepared.root_facts_touched,
                                                                    prepared.root_identity_stable, &prepared.visual_noop_directory_paths,
                                                                    prepared.empty_root_replacement_safe, true,
                                                                )
                                                            }
                                                            other => other,
                                                        };
                                                        match result {
                                                            Ok(mut updated) => {
                                                                updated.projection.forget_deleted_files(&deleted_files);
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
                                                                    patches: updated.patches,
                                                                    holes,
                                                                    transitions: updated.transitions,
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
                                                        hidden_checkpoint: next_checkpoint,
                                                    });
                                                    Some(Event::Prepared(Prepared {
                                                        base: version,
                                                        version: prepared.version,
                                                        views,
                                                        rebuild,
                                                        hidden_progress,
                                                        reconciled,
                                                        journal,
                                                    }))
                                                }
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
    #[test]
    fn removed_leaf_rebuilds_modes_with_changed_platform_topology() {
        let make = |has_subdirectories| {
            vec![Change::RemoveLeafDirectory {
                path: "/disk/a".into(),
                identity: identity(2),
                parent: scan::DirectoryFact {
                    path: "/disk".into(),
                    identity: identity(1),
                    enumeration_complete: true,
                    excluded: false,
                    has_subdirectories,
                },
            }]
        };
        let sibling_remains = make(true);
        assert!(requires_structural_rebuild(View::Files, &sibling_remains));
        assert!(!requires_structural_rebuild(
            View::Folders,
            &sibling_remains
        ));
        assert!(!requires_structural_rebuild(View::Hybrid, &sibling_remains));
        let parent_became_leaf = make(false);
        assert!(requires_structural_rebuild(
            View::Files,
            &parent_became_leaf
        ));
        assert!(!requires_structural_rebuild(
            View::Folders,
            &parent_became_leaf
        ));
        assert!(requires_structural_rebuild(
            View::Hybrid,
            &parent_became_leaf
        ));
        let unknown_parent = [Change::RemoveConfirmedSubtree {
            path: "/disk/a".into(),
            identity: identity(2),
        }];
        for mode in [View::Files, View::Folders, View::Hybrid] {
            assert!(requires_structural_rebuild(mode, &unknown_parent));
        }
    }
    #[test]
    fn files_leaf_directory_deletion_keeps_outer_texture_and_updates_platform_locally() {
        let fact = |path: &str, n: u64, has_subdirectories| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories,
        };
        let file = |parent: &str, n: u64, size: u64| scan::FileEntry {
            path: format!("{parent}/f").into(),
            parent: Arc::from(Path::new(parent)),
            size,
            identity: identity(n + 10),
            parent_identity: identity(n),
            parent_is_leaf: true,
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/x", 3, 2),
                file("/disk/a/y", 4, 2),
                file("/disk/b", 5, 100),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, true),
                fact("/disk/a/x", 3, false),
                fact("/disk/a/y", 4, false),
                fact("/disk/b", 5, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 400.0,
            h: 200.0,
        };
        let layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk/a/x"), vec![(0, 2)]),
                (Path::new("/disk/a/y"), vec![(1, 2)]),
                (Path::new("/disk/b"), vec![(2, 100)]),
            ],
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
            [400, 200],
            &token,
            1,
        )
        .unwrap();
        let stable = map.rect(2);
        let coordinator = Coordinator::start(source.clone(), 71, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let texture = TextureId::Managed(71);
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
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::ModeReady(_)));
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![Change::RemoveLeafDirectory {
                    path: "/disk/a/x".into(),
                    identity: identity(3),
                    parent: fact("/disk/a", 2, true),
                }]),
                modes: vec![ModeBase {
                    mode: View::Files,
                    layout_generation: 1,
                    texture,
                    map,
                    holes: Arc::new(vec![]),
                    pixels_per_point: 1.0,
                    area_tolerance: 8192.0,
                    max_patch_pixels: 79999,
                }],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(result) = event(&coordinator) else {
            panic!("prepared")
        };
        assert!(result.rebuild.is_empty());
        let view = &result.views[0];
        assert_eq!(view.texture, texture);
        assert_eq!(view.map.rect(2), stable);
        assert!(view.projection.get(0).is_none());
        assert!(!view.transitions.is_empty());
        let patch = view.patches.first().unwrap();
        assert!(patch.image.size[0] * patch.image.size[1] < 400 * 200);
    }
    #[test]
    fn hybrid_leaf_folder_delete_transitions_sibling_without_replacing_texture() {
        let fact = |path: &str, n: u64, has_subdirectories| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories,
        };
        let file = |name: &str, dir_id, file_id| scan::FileEntry {
            path: format!("/disk/{name}/f").into(),
            parent: Arc::from(PathBuf::from(format!("/disk/{name}"))),
            size: 50,
            identity: identity(file_id),
            parent_identity: identity(dir_id),
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
        let projected = Arc::new(scan::ScanResult {
            root: source.root.clone(),
            files: ["a", "b"]
                .into_iter()
                .enumerate()
                .map(|(i, name)| scan::FileEntry {
                    path: format!("/disk/{name}").into(),
                    parent: Arc::from(Path::new("/disk")),
                    size: 50,
                    identity: identity(2 + i as u64),
                    parent_identity: Default::default(),
                    parent_is_leaf: false,
                    can_trash: true,
                })
                .collect(),
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
        let layout = directory_map::layout_with_partitions(
            [(Path::new("/disk"), vec![(0, 50), (1, 50)])],
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
        let files_layout = directory_map::layout_with_partitions(
            [
                (Path::new("/disk/a"), vec![(0, 50)]),
                (Path::new("/disk/b"), vec![(1, 50)]),
            ],
            Path::new("/disk"),
            bounds,
            1.0,
            &token,
            1,
        )
        .unwrap();
        let files_map = RenderMap::build_with_platforms(
            files_layout.entries,
            files_layout.platforms,
            bounds,
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        let coordinator = Coordinator::start(source.clone(), 13, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let texture = TextureId::Managed(41);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode: View::Hybrid,
                data: projected,
                source_ids: Some(Arc::from([None, None])),
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::new(BTreeSet::new()),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::ModeReady(_)));
        let files_texture = TextureId::Managed(42);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode: View::Files,
                data: source,
                source_ids: None,
                partitions: Some(Arc::new(files_layout.partitions)),
                retained: None,
                tombstones: Arc::new(BTreeSet::new()),
                map: files_map.snapshot_for_update(),
                layout_generation: 2,
                texture: files_texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::ModeReady(_)));
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![Change::RemoveLeafDirectory {
                    path: "/disk/a".into(),
                    identity: identity(2),
                    parent: fact("/disk", 1, true),
                }]),
                modes: vec![
                    ModeBase {
                        mode: View::Hybrid,
                        layout_generation: 1,
                        texture,
                        map,
                        holes: Arc::new(vec![]),
                        pixels_per_point: 1.0,
                        area_tolerance: 10000.0,
                        max_patch_pixels: 10000,
                    },
                    ModeBase {
                        mode: View::Files,
                        layout_generation: 2,
                        texture: files_texture,
                        map: files_map,
                        holes: Arc::new(vec![]),
                        pixels_per_point: 1.0,
                        area_tolerance: 10000.0,
                        max_patch_pixels: 10000,
                    },
                ],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(update) = event(&coordinator) else {
            panic!("prepared")
        };
        assert!(update.rebuild.is_empty());
        assert_eq!(update.views.len(), 2);
        let hybrid = update
            .views
            .iter()
            .find(|view| view.mode == View::Hybrid)
            .unwrap();
        assert_eq!(hybrid.texture, texture);
        assert!(!hybrid.transitions.is_empty());
        assert!(hybrid.projection.get(0).is_none());
        assert!(hybrid.projection.get(1).is_some());
        let files = update
            .views
            .iter()
            .find(|view| view.mode == View::Files)
            .unwrap();
        assert_eq!(files.texture, files_texture);
        assert!(files.projection.get(0).is_none());
        assert!(files.projection.get(1).is_some());
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
            excluded: false,
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
                    cursor: None,
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
        assert_eq!(updated.map.hit_test(at.0, at.1), Some(1));
        assert!(!updated.transitions.is_empty());
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
    fn explicit_empty_directory_changes_update_hybrid_topology() {
        let source = fixture();
        let coordinator = Coordinator::start(source.clone(), 71, egui::Context::default());
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
        let texture = TextureId::Managed(271);
        coordinator
            .try_send(Command::InitMode(ModeInput {
                base,
                mode: View::Hybrid,
                data: folder,
                source_ids: Some(Arc::from([None])),
                partitions: Some(Arc::new(layout.partitions)),
                retained: None,
                tombstones: Arc::new(BTreeSet::new()),
                map: map.snapshot_for_update(),
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::ModeReady(_)));
        let fact = |path: &str, id: u64, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: children,
        };
        let input = |base, changes, map: RenderMap, holes| {
            Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(changes),
                modes: vec![ModeBase {
                    mode: View::Hybrid,
                    layout_generation: 1,
                    texture,
                    map,
                    holes,
                    pixels_per_point: 1.0,
                    area_tolerance: 10000.0,
                    max_patch_pixels: 256,
                }],
            })
        };
        coordinator
            .try_send(input(
                base,
                vec![
                    Change::UpsertDirectory(fact("/disk/a", 2, true)),
                    Change::UpsertDirectory(fact("/disk/a/empty", 5, false)),
                ],
                map.snapshot_for_update(),
                Arc::new(vec![]),
            ))
            .ok()
            .unwrap();
        let Event::Prepared(mut expanded) = event(&coordinator) else {
            panic!("expanded")
        };
        assert!(expanded.rebuild.is_empty());
        let view = expanded.views.pop().expect("hybrid view");
        assert_eq!(view.texture, texture);
        let expanded_paths: BTreeSet<_> = (0..view.projection.len())
            .filter_map(|i| view.projection.get(i))
            .map(|entry| entry.path.to_path_buf())
            .collect();
        assert_eq!(
            expanded_paths,
            BTreeSet::from([PathBuf::from("/disk/a/f"), PathBuf::from("/disk/a/g")])
        );
        coordinator
            .try_send(Command::Commit(expanded.version))
            .ok()
            .unwrap();
        coordinator
            .try_send(input(
                expanded.version,
                vec![
                    Change::RemoveDirectory {
                        path: "/disk/a/empty".into(),
                        identity: identity(5),
                    },
                    Change::UpsertDirectory(fact("/disk/a", 2, false)),
                ],
                view.map,
                view.holes,
            ))
            .ok()
            .unwrap();
        let Event::Prepared(mut collapsed) = event(&coordinator) else {
            panic!("collapsed")
        };
        assert!(collapsed.rebuild.is_empty());
        let view = collapsed.views.pop().expect("hybrid view");
        assert_eq!(view.texture, texture);
        let paths: Vec<_> = (0..view.projection.len())
            .filter_map(|i| view.projection.get(i))
            .map(|entry| entry.path.to_path_buf())
            .collect();
        assert_eq!(paths, vec![PathBuf::from("/disk/a")]);
    }

    #[test]
    fn first_file_added_to_empty_scene_uses_existing_texture() {
        let mut data = Arc::try_unwrap(fixture()).unwrap();
        data.files.clear();
        data.directories = vec![scan::DirectoryFact {
            path: "/disk".into(),
            identity: identity(1),
            enumeration_complete: true,
            excluded: false,
            has_subdirectories: false,
        }]
        .into();
        let source = Arc::new(data);
        let coordinator = Coordinator::start(source.clone(), 72, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 48.0,
            h: 32.0,
        };
        let token = AtomicU64::new(1);
        let layout = directory_map::layout_with_partitions(
            std::iter::empty::<(&Path, Vec<(usize, u64)>)>(),
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
            [48, 32],
            &token,
            1,
        )
        .unwrap();
        let texture = TextureId::Managed(272);
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
                layout_generation: 1,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::ModeReady(_)));
        let file = scan::FileEntry {
            path: "/disk/new".into(),
            parent: Arc::from(Path::new("/disk")),
            size: 5,
            identity: identity(9),
            parent_identity: identity(1),
            parent_is_leaf: true,
            can_trash: true,
        };
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![Change::UpsertFile(file)]),
                modes: vec![ModeBase {
                    mode: View::Files,
                    layout_generation: 1,
                    texture,
                    map,
                    holes: Arc::new(vec![]),
                    pixels_per_point: 1.0,
                    area_tolerance: 10000.0,
                    max_patch_pixels: 100,
                }],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(mut prepared) = event(&coordinator) else {
            panic!("prepared")
        };
        assert!(prepared.rebuild.is_empty());
        let view = prepared.views.pop().expect("updated view");
        assert_eq!(view.texture, texture);
        assert_eq!(view.projection.get(0).unwrap().path, Path::new("/disk/new"));
        assert_eq!(view.map.hit_test(24.0, 16.0), Some(0));
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
                    cursor: None,
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
                    cursor: None,
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
                    cursor: None,
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
    fn journal_recovery_reattaches_retained_extra_slots_and_overrides() {
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
        let map = RenderMap::build_with_platforms(
            layout.entries,
            layout.platforms,
            bounds,
            [100, 100],
            &token,
            1,
        )
        .unwrap();
        let texture = TextureId::Managed(91);
        let first = Coordinator::start(source.clone(), 41, egui::Context::default());
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
                layout_generation: 9,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&first), Event::ModeReady(_)));
        let mut changed = source.files[0].clone();
        changed.size = 73;
        let mut added = source.files[0].clone();
        added.path = "/disk/a/new".into();
        added.identity = identity(99);
        first
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![
                    Change::UpsertFile(changed),
                    Change::UpsertFile(added),
                ]),
                modes: vec![ModeBase {
                    mode: View::Files,
                    layout_generation: 9,
                    texture,
                    map: map.snapshot_for_update(),
                    holes: Arc::default(),
                    pixels_per_point: 1.0,
                    area_tolerance: 10000.0,
                    max_patch_pixels: 10000,
                }],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(mut prepared) = event(&first) else {
            panic!("prepared")
        };
        assert!(prepared.rebuild.is_empty());
        let view = prepared.views.pop().unwrap();
        assert!(view.projection.len() > source.files.len());
        let mut journal = crate::scene_journal::Journal::new(source.clone(), 41, Arc::default());
        journal
            .append(base, prepared.version, prepared.journal.clone())
            .unwrap();
        first
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        drop(first);

        let recovered = Coordinator::recover(source.clone(), 41, journal, egui::Context::default());
        let Event::Ready(recovered_version) = event(&recovered) else {
            panic!("recovered")
        };
        assert_eq!(recovered_version, prepared.version);
        recovered
            .try_send(Command::InitMode(ModeInput {
                base: recovered_version,
                mode: View::Files,
                data: source,
                source_ids: None,
                partitions: None,
                retained: Some(Retained {
                    geometry: view.geometry,
                    projection: view.projection,
                }),
                tombstones: Arc::default(),
                map: view.map,
                layout_generation: 9,
                texture,
            }))
            .ok()
            .unwrap();
        let Event::ModeReady(ready) = event(&recovered) else {
            panic!("retained mode")
        };
        assert_eq!(ready.texture, texture);
        assert!((0..ready.projection.len()).any(|slot| {
            ready
                .projection
                .get(slot)
                .is_some_and(|f| f.path == Path::new("/disk/a/new"))
        }));
        assert!((0..ready.projection.len()).any(|slot| {
            ready
                .projection
                .get(slot)
                .is_some_and(|f| f.path == Path::new("/disk/a/f") && f.size == 73)
        }));
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
                    cursor: None,
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
    fn large_hidden_diff_prepares_chunks_but_publishes_once() {
        let mut data = Arc::try_unwrap(fixture()).unwrap();
        let template = data.files[0].clone();
        data.files = (0..=MAX_BATCH_CHANGES)
            .map(|i| {
                let mut file = template.clone();
                file.path = format!("/disk/a/{i}").into();
                file.identity = identity(i as u64 + 10);
                file
            })
            .collect();
        let coordinator = Coordinator::start(Arc::new(data), 12, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let before = Arc::new(BTreeSet::new());
        let after: Arc<BTreeSet<usize>> = Arc::new((0..=MAX_BATCH_CHANGES).collect());
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: before.clone(),
                    after: after.clone(),
                    cursor: None,
                },
                modes: vec![],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = event(&coordinator) else {
            panic!("complete update")
        };
        assert_eq!(
            prepared.hidden_progress,
            Some(HiddenProgress {
                cursor: None,
                done: true
            })
        );
        assert_eq!(prepared.base, base);
        assert_eq!(prepared.version.scene_version, base.scene_version + 2);
        assert!(matches!(coordinator.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn failed_later_hidden_chunk_recovers_only_published_journal() {
        let mut data = Arc::try_unwrap(fixture()).unwrap();
        let template = data.files[0].clone();
        data.files = (0..=MAX_BATCH_CHANGES)
            .map(|i| {
                let mut file = template.clone();
                file.path = format!("/disk/a/{i}").into();
                file.identity = identity(i as u64 + 10);
                file
            })
            .collect();
        let source = Arc::new(data);
        let mut added = template;
        added.path = "/disk/a/added".into();
        added.identity = identity(90_000);
        let already_removed = source.files.last().unwrap();
        let coordinator = Coordinator::start(source.clone(), 42, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![
                    Change::RemoveFile {
                        path: already_removed.path.clone(),
                        identity: already_removed.identity,
                    },
                    Change::UpsertFile(added),
                ]),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(published) = event(&coordinator) else {
            panic!("published")
        };
        let mut journal = crate::scene_journal::Journal::new(source.clone(), 42, Arc::default());
        journal
            .append(base, published.version, published.journal.clone())
            .unwrap();
        coordinator
            .try_send(Command::Commit(published.version))
            .ok()
            .unwrap();
        // The first 4096 removals can succeed privately; the final stale
        // source slot fails because its file was removed in the published
        // transaction above. No private chunk may enter the published journal.
        coordinator
            .try_send(Command::Update(UpdateInput {
                base: published.version,
                changes: Changes::HiddenDiff {
                    before: Arc::default(),
                    after: Arc::new((0..=MAX_BATCH_CHANGES).collect()),
                    cursor: None,
                },
                modes: vec![],
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::Failed(Failure::Scene)));
        drop(coordinator);
        let recovered = Coordinator::recover(source, 42, journal, egui::Context::default());
        let Event::Ready(version) = event(&recovered) else {
            panic!("recovered")
        };
        assert_eq!(version, published.version);
        recovered
            .try_send(Command::ExportMode {
                base: version,
                mode: View::Files,
            })
            .ok()
            .unwrap();
        let Event::ModeSnapshot { data, .. } = event(&recovered) else {
            panic!("snapshot")
        };
        assert_eq!(data.files.len(), MAX_BATCH_CHANGES + 1);
        assert!(
            data.files
                .iter()
                .any(|file| file.path == Path::new("/disk/a/0"))
        );
        assert!(
            data.files
                .iter()
                .any(|file| file.path == Path::new("/disk/a/added"))
        );
        assert!(
            !data
                .files
                .iter()
                .any(|file| file.path == Path::new("/disk/a/4096"))
        );
    }

    #[test]
    fn large_hidden_diff_keeps_render_map_atomic_and_reuses_texture() {
        let mut data = Arc::try_unwrap(fixture()).unwrap();
        let template = data.files[0].clone();
        let count = MAX_BATCH_CHANGES + 2;
        data.files = (0..count)
            .map(|i| {
                let mut file = template.clone();
                file.path = format!("/disk/a/{i}").into();
                file.identity = identity(i as u64 + 10);
                file
            })
            .collect();
        let source = Arc::new(data);
        let coordinator = Coordinator::start(source.clone(), 40, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let token = AtomicU64::new(1);
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 96.0,
            h: 64.0,
        };
        let layout = directory_map::layout_with_partitions(
            [(
                Path::new("/disk/a"),
                (0..count).map(|id| (id, 50)).collect(),
            )],
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
            [96, 64],
            &token,
            1,
        )
        .unwrap();
        let texture = TextureId::Managed(123);
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
                layout_generation: 8,
                texture,
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::ModeReady(_)));
        let old_owner = (0..64)
            .flat_map(|y| (0..96).map(move |x| (x, y)))
            .find_map(|(x, y)| {
                map.hit_test(x as f32 + 0.5, y as f32 + 0.5)
                    .map(|id| ((x, y), id))
            })
            .expect("the original map has a visible owner");
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: Arc::new(BTreeSet::new()),
                    after: Arc::new((0..count - 1).collect()),
                    cursor: None,
                },
                modes: vec![ModeBase {
                    mode: View::Files,
                    layout_generation: 8,
                    texture,
                    map: map.snapshot_for_update(),
                    holes: Arc::new(vec![]),
                    pixels_per_point: 1.0,
                    area_tolerance: 10000.0,
                    max_patch_pixels: 256,
                }],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = coordinator
            .rx
            .recv_timeout(Duration::from_secs(60))
            .expect("large update")
        else {
            panic!("prepared")
        };
        assert!(prepared.rebuild.is_empty());
        assert_eq!(
            prepared.hidden_progress,
            Some(HiddenProgress {
                cursor: None,
                done: true
            })
        );
        assert_eq!(prepared.version.scene_version, base.scene_version + 2);
        assert!(matches!(coordinator.try_recv(), Err(TryRecvError::Empty)));
        let view = &prepared.views[0];
        assert_eq!(view.texture, texture);
        assert!(view.projection.get(count - 2).is_none());
        assert!(view.projection.get(count - 1).is_some());
        assert_eq!(
            map.hit_test(old_owner.0.0 as f32 + 0.5, old_owner.0.1 as f32 + 0.5),
            Some(old_owner.1)
        );
        assert!(!view.transitions.is_empty());
        assert!(view.transitions.len() <= 64);
        for y in 0..64 {
            for x in 0..96 {
                if let Some(owner) = view.map.hit_test(x as f32 + 0.5, y as f32 + 0.5) {
                    assert_eq!(owner, count - 1);
                }
            }
        }
    }
    #[test]
    fn large_hidden_diff_updates_folder_and_hybrid_once_with_stable_textures() {
        let mut data = Arc::try_unwrap(fixture()).unwrap();
        let template = data.files[0].clone();
        let count = MAX_BATCH_CHANGES + 2;
        data.files = (0..count)
            .map(|i| {
                let mut file = template.clone();
                let (parent, parent_identity) = if i < MAX_BATCH_CHANGES {
                    ("/disk/a", identity(2))
                } else {
                    ("/disk/b", identity(5))
                };
                file.path = format!("{parent}/{i}").into();
                file.parent = Arc::from(Path::new(parent));
                file.parent_identity = parent_identity;
                file.identity = identity(i as u64 + 10);
                file
            })
            .collect();
        let mut facts = data.directories.to_vec();
        facts.push(scan::DirectoryFact {
            path: "/disk/b".into(),
            identity: identity(5),
            enumeration_complete: true,
            has_subdirectories: false,
            excluded: false,
        });
        data.directories = facts.into();
        let source = Arc::new(data);
        let token = AtomicU64::new(1);
        let empty = BTreeSet::new();
        let folders = Arc::new(crate::folder_snapshot(&source, &empty, &token, 1).unwrap());
        let hybrid = crate::hybrid::build(&source, &empty, &token, 1).unwrap();
        let coordinator = Coordinator::start(source.clone(), 40, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 96.0,
            h: 64.0,
        };
        let mut modes = Vec::new();
        for (index, (view, data, source_ids)) in [
            (View::Folders, folders, None),
            (View::Hybrid, hybrid.data, Some(hybrid.source_ids)),
        ]
        .into_iter()
        .enumerate()
        {
            let mut groups: std::collections::HashMap<&Path, Vec<(usize, u64)>> =
                std::collections::HashMap::new();
            for (slot, file) in data.files.iter().enumerate() {
                groups
                    .entry(file.parent.as_ref())
                    .or_default()
                    .push((slot, file.size.max(1)));
            }
            let layout =
                directory_map::layout_with_partitions(groups, &source.root, bounds, 1.0, &token, 1)
                    .unwrap();
            let map = RenderMap::build_with_platforms(
                layout.entries,
                layout.platforms,
                bounds,
                [96, 64],
                &token,
                1,
            )
            .unwrap();
            let texture = TextureId::Managed(210 + index as u64);
            coordinator
                .try_send(Command::InitMode(ModeInput {
                    base,
                    mode: view,
                    data,
                    source_ids,
                    partitions: Some(Arc::new(layout.partitions)),
                    retained: None,
                    tombstones: Arc::default(),
                    map: map.snapshot_for_update(),
                    layout_generation: 8,
                    texture,
                }))
                .ok()
                .unwrap();
            assert!(matches!(event(&coordinator), Event::ModeReady(_)));
            modes.push(ModeBase {
                mode: view,
                layout_generation: 8,
                texture,
                map,
                holes: Arc::default(),
                pixels_per_point: 1.0,
                area_tolerance: 10000.0,
                max_patch_pixels: 256,
            });
        }
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::HiddenDiff {
                    before: Arc::default(),
                    after: Arc::new((0..count - 1).collect()),
                    cursor: None,
                },
                modes,
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = coordinator
            .rx
            .recv_timeout(Duration::from_secs(60))
            .expect("large update")
        else {
            panic!("prepared")
        };
        assert!(prepared.rebuild.is_empty());
        assert_eq!(
            prepared.hidden_progress,
            Some(HiddenProgress {
                cursor: None,
                done: true
            })
        );
        assert_eq!(prepared.version.scene_version, base.scene_version + 2);
        assert_eq!(prepared.views.len(), 2);
        for view in prepared.views {
            assert!(
                view.texture == TextureId::Managed(210) || view.texture == TextureId::Managed(211)
            );
            let live: Vec<_> = (0..view.projection.len())
                .filter_map(|slot| view.projection.get(slot))
                .collect();
            assert_eq!(live.len(), 1);
            assert_eq!(live[0].path, Path::new("/disk/b"));
            assert_eq!(live[0].size, 50);
            for y in 0..64 {
                for x in 0..96 {
                    if let Some(owner) = view.map.hit_test(x as f32 + 0.5, y as f32 + 0.5) {
                        assert_eq!(
                            view.projection.get(owner).unwrap().path,
                            Path::new("/disk/b")
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn confirmed_subtree_without_parent_facts_updates_modes_and_protects_parent() {
        let fact = |path: &str, n, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let file = |parent: &str, n, dir_id| scan::FileEntry {
            path: format!("{parent}/f").into(),
            parent: Arc::from(Path::new(parent)),
            size: 50,
            identity: identity(n),
            parent_identity: identity(dir_id),
            parent_is_leaf: parent != "/disk/a",
            can_trash: true,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a", 20, 2),
                file("/disk/a/nested", 21, 3),
                file("/disk/b", 22, 4),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, true),
                fact("/disk/a/nested", 3, false),
                fact("/disk/b", 4, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let token = AtomicU64::new(1);
        let folders =
            Arc::new(crate::folder_snapshot(&source, &BTreeSet::new(), &token, 1).unwrap());
        let hybrid = crate::hybrid::build(&source, &BTreeSet::new(), &token, 1).unwrap();
        let coordinator = Coordinator::start(source.clone(), 64, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            w: 120.0,
            h: 80.0,
        };
        let mut modes = Vec::new();
        for (index, (mode, data, source_ids)) in [
            (View::Folders, folders, None),
            (View::Hybrid, hybrid.data, Some(hybrid.source_ids)),
        ]
        .into_iter()
        .enumerate()
        {
            let mut groups: std::collections::HashMap<&Path, Vec<(usize, u64)>> =
                std::collections::HashMap::new();
            for (slot, file) in data.files.iter().enumerate() {
                groups
                    .entry(file.parent.as_ref())
                    .or_default()
                    .push((slot, file.size.max(1)));
            }
            let layout =
                directory_map::layout_with_partitions(groups, &source.root, bounds, 1.0, &token, 1)
                    .unwrap();
            let map = RenderMap::build_with_platforms(
                layout.entries,
                layout.platforms,
                bounds,
                [120, 80],
                &token,
                1,
            )
            .unwrap();
            let texture = TextureId::Managed(220 + index as u64);
            coordinator
                .try_send(Command::InitMode(ModeInput {
                    base,
                    mode,
                    data,
                    source_ids,
                    partitions: Some(Arc::new(layout.partitions)),
                    retained: None,
                    tombstones: Arc::default(),
                    map: map.snapshot_for_update(),
                    layout_generation: 4,
                    texture,
                }))
                .ok()
                .unwrap();
            assert!(matches!(event(&coordinator), Event::ModeReady(_)));
            modes.push(ModeBase {
                mode,
                layout_generation: 4,
                texture,
                map,
                holes: Arc::default(),
                pixels_per_point: 1.0,
                area_tolerance: 10000.0,
                max_patch_pixels: 1024,
            });
        }
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![Change::RemoveConfirmedSubtree {
                    path: "/disk/a/nested".into(),
                    identity: identity(3),
                }]),
                modes,
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = event(&coordinator) else {
            panic!("prepared")
        };
        assert!(prepared.rebuild.is_empty());
        assert_eq!(prepared.views.len(), 2);
        for view in prepared.views {
            assert!((0..view.projection.len()).all(|slot| {
                view.projection
                    .get(slot)
                    .is_none_or(|entry| !entry.path.starts_with("/disk/a/nested"))
            }));
            if view.mode == View::Folders {
                let parent = (0..view.projection.len())
                    .filter_map(|slot| view.projection.get(slot))
                    .find(|entry| entry.path == Path::new("/disk/a"))
                    .expect("parent aggregate");
                assert!(!parent.can_trash, "unknown parent must not be deletable");
            }
        }
    }
    #[test]
    fn large_explicit_batch_is_prevalidated_and_published_once() {
        let mut data = Arc::try_unwrap(fixture()).unwrap();
        let template = data.files[0].clone();
        data.files = (0..=MAX_BATCH_CHANGES)
            .map(|i| {
                let mut file = template.clone();
                file.path = format!("/disk/a/{i}").into();
                file.identity = identity(i as u64 + 10);
                file
            })
            .collect();
        let changes: Vec<_> = data
            .files
            .iter()
            .map(|file| Change::RemoveFile {
                path: file.path.clone(),
                identity: file.identity,
            })
            .collect();
        let coordinator = Coordinator::start(Arc::new(data), 20, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let mut invalid = changes.clone();
        if let Change::RemoveFile { identity, .. } = invalid.last_mut().unwrap() {
            *identity = self::identity(u64::MAX);
        }
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(invalid),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::Failed(Failure::Scene)));
        // An invalid last operation must not remove the earlier 4096 entries.
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(changes),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = event(&coordinator) else {
            panic!("prepared")
        };
        assert_eq!(prepared.base, base);
        assert_eq!(prepared.version.scene_version, base.scene_version + 1);
        assert!(prepared.hidden_progress.is_none());
    }

    #[test]
    fn mode_export_uses_current_scene_after_committed_changes() {
        let source = fixture();
        let mut new_file = source.files[0].clone();
        new_file.path = "/disk/a/new".into();
        new_file.identity = identity(999);
        let coordinator = Coordinator::start(source, 31, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![Change::UpsertFile(new_file)]),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = event(&coordinator) else {
            panic!("prepared")
        };
        coordinator
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        coordinator
            .try_send(Command::ExportMode {
                base: prepared.version,
                mode: View::Files,
            })
            .ok()
            .unwrap();
        let Event::ModeSnapshot {
            base: exported,
            mode,
            data,
            source_ids,
        } = event(&coordinator)
        else {
            panic!("snapshot")
        };
        assert_eq!(exported, prepared.version);
        assert_eq!(mode, View::Files);
        assert!(source_ids.is_none());
        assert!(
            data.files
                .iter()
                .any(|file| file.path == Path::new("/disk/a/new"))
        );
        coordinator
            .try_send(Command::ExportMode {
                base,
                mode: View::Files,
            })
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::Failed(Failure::Stale)));
    }

    #[test]
    fn deleted_file_ids_exclude_moves_and_retain_only_true_removals() {
        let scene = Scene::from_scan(fixture());
        let first = scene
            .tile(crate::scene_model::TileId::File(
                scene.file_id(Path::new("/disk/a/f")).unwrap(),
            ))
            .unwrap();
        let removed = scene
            .tile(crate::scene_model::TileId::File(
                scene.file_id(Path::new("/disk/a/g")).unwrap(),
            ))
            .unwrap();
        let mut moved = first.clone();
        moved.path = "/disk/moved".into();
        moved.parent = "/disk".into();
        let deltas = vec![
            scene_actor::TileDelta {
                before: Some(first.clone()),
                after: Some(moved),
            },
            scene_actor::TileDelta {
                before: Some(removed.clone()),
                after: None,
            },
        ];
        assert_eq!(deleted_file_ids(&deltas), vec![removed.id]);
        // Even if a future producer separates the same stable identity into
        // distinct removal/addition deltas, it is still a move, not deletion.
        let separated = vec![
            scene_actor::TileDelta {
                before: Some(first.clone()),
                after: None,
            },
            scene_actor::TileDelta {
                before: None,
                after: Some(first),
            },
        ];
        assert!(deleted_file_ids(&separated).is_empty());
    }

    #[test]
    fn checkpoint_requires_committed_matching_version() {
        let source = fixture();
        let coordinator = Coordinator::start(source.clone(), 56, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        let mut changed = source.files[0].clone();
        changed.size = 73;
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Explicit(vec![Change::UpsertFile(changed)]),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = event(&coordinator) else {
            panic!("prepared")
        };
        coordinator
            .try_send(Command::Checkpoint { base })
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
            .try_send(Command::Checkpoint { base })
            .ok()
            .unwrap();
        assert!(matches!(event(&coordinator), Event::Failed(Failure::Stale)));
        coordinator
            .try_send(Command::Checkpoint {
                base: prepared.version,
            })
            .ok()
            .unwrap();
        let Event::Checkpoint {
            base: snapshot_version,
            checkpoint,
        } = event(&coordinator)
        else {
            panic!("checkpoint")
        };
        assert_eq!(snapshot_version, prepared.version);
        let mut journal = crate::scene_journal::Journal::new(source.clone(), 56, Arc::default());
        journal
            .append(base, prepared.version, prepared.journal)
            .unwrap();
        journal.adopt(checkpoint).unwrap();
        let recovered = Coordinator::recover(source, 56, journal, egui::Context::default());
        assert!(matches!(event(&recovered), Event::Ready(v) if v == prepared.version));
    }

    #[test]
    fn reconcile_is_published_as_one_version_and_exports_updated_scene() {
        let source = fixture();
        let mut files = source.files.clone();
        files[0].size = 73;
        let updated = Arc::new(scan::ScanResult {
            root: source.root.clone(),
            files,
            directories: source.directories.clone(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let coordinator = Coordinator::start(source, 32, egui::Context::default());
        let Event::Ready(base) = event(&coordinator) else {
            panic!("ready")
        };
        coordinator
            .try_send(Command::Update(UpdateInput {
                base,
                changes: Changes::Reconcile(updated),
                modes: vec![],
            }))
            .ok()
            .unwrap();
        let Event::Prepared(prepared) = event(&coordinator) else {
            panic!("prepared")
        };
        assert!(prepared.reconciled);
        assert_eq!(prepared.base, base);
        coordinator
            .try_send(Command::Commit(prepared.version))
            .ok()
            .unwrap();
        coordinator
            .try_send(Command::ExportMode {
                base: prepared.version,
                mode: View::Files,
            })
            .ok()
            .unwrap();
        let Event::ModeSnapshot { data, .. } = event(&coordinator) else {
            panic!("snapshot")
        };
        assert_eq!(
            data.files
                .iter()
                .find(|f| f.path == Path::new("/disk/a/f"))
                .unwrap()
                .size,
            73
        );
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
                    cursor: None,
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

#[cfg(test)]
#[path = "coordinator_regression_tests.rs"]
mod regression_tests;
