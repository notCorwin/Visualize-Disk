//! Single-writer scene actor. UI commands never wait for directory work and a
//! prepared transaction must be acknowledged before another can be applied.
use crate::scan::{self, FileIdentity};
use crate::scene_model::{Change, ChangeError, Scene, Tile, TileId, View};
use crate::sparse_projection::SparseProjection;
use eframe::egui;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    pub source_generation: u64,
    pub scene_version: u64,
}
#[derive(Debug)]
pub struct UpdateRequest {
    pub base: Version,
    pub changes: Vec<Change>,
}
#[derive(Debug)]
pub struct ReconcileRequest {
    pub base: Version,
    pub data: Arc<scan::ScanResult>,
}
#[derive(Debug)]
pub struct ExportProjectionRequest {
    pub base: Version,
    pub mode: View,
}
#[derive(Clone, Debug)]
pub struct TileDelta {
    pub before: Option<Tile>,
    pub after: Option<Tile>,
}
#[derive(Default, Debug)]
pub struct ProjectionDelta {
    pub files: Vec<TileDelta>,
    pub folders: Vec<TileDelta>,
    pub hybrid: Vec<TileDelta>,
}
#[derive(Debug)]
pub struct Prepared {
    pub base: Version,
    pub version: Version,
    pub projections: ProjectionDelta,
    /// Common ancestor whose directory topology may have changed.
    pub structural_scope: Option<PathBuf>,
    /// Directory facts touched by the transaction, including empty directories
    /// that have no projected tile. Used to prove independent components are
    /// safe to prepare separately.
    pub structural_paths: Vec<PathBuf>,
    /// The root's facts, direct-file aggregate, or direct-child topology may
    /// have changed. These are shared by root-child components and cannot be
    /// split independently.
    pub root_facts_touched: bool,
    /// The scanned root retained both its slot generation and filesystem
    /// identity throughout this transaction.
    pub root_identity_stable: bool,
    /// A replacement root had no known old files or descendants in the Scene,
    /// and all new known entries are rooted in the scan with known direct file parents.
    /// Unknown filesystem entries remain unknown; no emptiness is inferred.
    /// This is a separate proof from identity stability, bound to base/version.
    pub empty_root_replacement_safe: bool,
    /// Touched directories absent or fully empty leaves both before and after.
    /// Their own platform and projection are provably absent in every view.
    pub visual_noop_directory_paths: Vec<PathBuf>,
    /// Exact input order used for this transaction. Recovery must replay the
    /// applied order rather than recomputing a diff whose hash iteration may
    /// assign different stable IDs.
    pub applied: Arc<[Change]>,
    /// Reconciliation can require ordered operations that are invalid as one
    /// independent validated batch (for example directory replacement).
    pub sequential: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LookupKind {
    File,
    Directory,
}
#[derive(Debug)]
pub struct ProjectionLookupRequest {
    pub base: Version,
    pub data: Arc<scan::ScanResult>,
    pub mode: View,
    /// Required in hybrid mode: Some(source index) denotes a file tile.
    pub source_ids: Option<Arc<[Option<usize>]>>,
}
#[derive(Debug)]
pub struct ProjectionLookupResult {
    pub base: Version,
    pub mode: View,
    pub entries: Vec<Option<TileId>>,
}
pub struct ProjectionVerifyRequest {
    pub base: Version,
    pub mode: View,
    pub projection: Arc<SparseProjection>,
}
#[derive(Debug)]
pub struct CheckpointRequest {
    pub base: Version,
}
#[derive(Debug)]
pub enum Failure {
    Stale { current: Version },
    Pending,
    Invalid(ChangeError),
    InvalidCommit,
    InvalidProjection,
    Reconcile(crate::snapshot_diff::Error),
}
#[derive(Debug)]
pub enum Event {
    Prepared(Prepared),
    ProjectionLookup(ProjectionLookupResult),
    ProjectionVerified {
        base: Version,
        mode: View,
        valid: bool,
    },
    Checkpoint {
        base: Version,
        checkpoint: crate::scene_journal::Checkpoint,
    },
    ExportProjection {
        base: Version,
        mode: View,
        root: PathBuf,
        tiles: Vec<Tile>,
        directories: Arc<[scan::DirectoryFact]>,
    },
    Failed(Failure),
}

enum Command {
    Update(UpdateRequest),
    Reconcile(ReconcileRequest),
    ExportProjection(ExportProjectionRequest),
    ProjectionLookup(ProjectionLookupRequest),
    ProjectionVerify(ProjectionVerifyRequest),
    Checkpoint(CheckpointRequest),
    Commit(Version),
}
pub struct SceneActor {
    tx: SyncSender<Command>,
    rx: Receiver<Event>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for SceneActor {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}
impl SceneActor {
    /// The supplied scene must be built off the UI thread before starting.
    #[cfg(test)]
    pub fn start(scene: Scene, source_generation: u64, ctx: egui::Context) -> Self {
        Self::start_with_cancel(
            scene,
            source_generation,
            ctx,
            Arc::new(AtomicBool::new(false)),
        )
    }
    pub fn start_with_cancel(
        scene: Scene,
        source_generation: u64,
        ctx: egui::Context,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        Self::start_with_version(
            scene,
            Version {
                source_generation,
                scene_version: 0,
            },
            ctx,
            cancelled,
        )
    }

    /// Restart from an exactly replayed, published scene version.
    pub fn start_with_version(
        scene: Scene,
        initial: Version,
        ctx: egui::Context,
        cancelled: Arc<AtomicBool>,
    ) -> Self {
        let (tx, commands) = mpsc::sync_channel(4);
        let (events, rx) = mpsc::channel();
        let worker_cancelled = Arc::clone(&cancelled);
        thread::Builder::new()
            .name("scene-updates".into())
            .spawn(move || {
                let mut scene = scene;
                let mut committed = initial;
                let mut pending = None;
                while let Ok(command) = commands.recv() {
                    if worker_cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    let event = match command {
                        Command::Commit(version) => {
                            if pending == Some(version) {
                                committed = version;
                                pending = None;
                                continue;
                            }
                            Event::Failed(Failure::InvalidCommit)
                        }
                        Command::ProjectionLookup(request) => {
                            if pending.is_some() {
                                Event::Failed(Failure::Pending)
                            } else if request.base != committed {
                                Event::Failed(Failure::Stale { current: committed })
                            } else if request.mode == View::Hybrid
                                && request
                                    .source_ids
                                    .as_ref()
                                    .is_none_or(|ids| ids.len() != request.data.files.len())
                            {
                                Event::Failed(Failure::InvalidProjection)
                            } else {
                                let entries: Option<Vec<_>> = request
                                    .data
                                    .files
                                    .iter()
                                    .enumerate()
                                    .map(|(i, file)| {
                                        if i % 4096 == 0 && worker_cancelled.load(Ordering::Relaxed)
                                        {
                                            return None;
                                        }
                                        let kind = match request.mode {
                                            View::Files => LookupKind::File,
                                            View::Folders => LookupKind::Directory,
                                            View::Hybrid => {
                                                if request.source_ids.as_ref().unwrap()[i].is_some()
                                                {
                                                    LookupKind::File
                                                } else {
                                                    LookupKind::Directory
                                                }
                                            }
                                        };
                                        Some(
                                            lookup(&scene, &file.path, file.identity, kind)
                                                .filter(|id| scene.matches_entry(*id, file)),
                                        )
                                    })
                                    .collect();
                                let Some(entries) = entries else {
                                    break;
                                };
                                Event::ProjectionLookup(ProjectionLookupResult {
                                    base: committed,
                                    mode: request.mode,
                                    entries,
                                })
                            }
                        }
                        Command::ProjectionVerify(request) => {
                            if pending.is_some() {
                                Event::Failed(Failure::Pending)
                            } else if request.base != committed {
                                Event::Failed(Failure::Stale { current: committed })
                            } else {
                                let Some(valid) = verify_projection(
                                    &scene,
                                    request.mode,
                                    &request.projection,
                                    &worker_cancelled,
                                ) else {
                                    break;
                                };
                                Event::ProjectionVerified {
                                    base: committed,
                                    mode: request.mode,
                                    valid,
                                }
                            }
                        }
                        Command::Checkpoint(request) => {
                            if pending.is_some() {
                                Event::Failed(Failure::Pending)
                            } else if request.base != committed {
                                Event::Failed(Failure::Stale { current: committed })
                            } else {
                                // Scene pages and hash shards are COW; no path,
                                // file table or directory members are rebuilt.
                                Event::Checkpoint {
                                    base: committed,
                                    checkpoint: crate::scene_journal::Checkpoint::from_scene(
                                        scene.clone(),
                                        committed,
                                    ),
                                }
                            }
                        }
                        Command::ExportProjection(request) => {
                            if pending.is_some() {
                                Event::Failed(Failure::Pending)
                            } else if request.base != committed {
                                Event::Failed(Failure::Stale { current: committed })
                            } else {
                                let mut directories = Vec::new();
                                let mut tiles = Vec::new();
                                scene.for_each_directory(|fact| {
                                    if !worker_cancelled.load(Ordering::Relaxed) {
                                        directories.push(fact.clone());
                                        tiles.extend(
                                            scene.project_directory(request.mode, &fact.path),
                                        );
                                    }
                                });
                                if worker_cancelled.load(Ordering::Relaxed) {
                                    break;
                                }
                                tiles.sort_unstable_by(|a, b| {
                                    a.path.cmp(&b.path).then(a.id.cmp(&b.id))
                                });
                                Event::ExportProjection {
                                    base: committed,
                                    mode: request.mode,
                                    root: scene.root().to_path_buf(),
                                    tiles,
                                    directories: directories.into(),
                                }
                            }
                        }
                        Command::Update(request) => {
                            if pending.is_some() {
                                Event::Failed(Failure::Pending)
                            } else if request.base != committed {
                                Event::Failed(Failure::Stale { current: committed })
                            } else {
                                match prepare(&mut scene, committed, request.changes) {
                                    Ok(prepared) => {
                                        pending = Some(prepared.version);
                                        Event::Prepared(prepared)
                                    }
                                    Err(error) => Event::Failed(Failure::Invalid(error)),
                                }
                            }
                        }
                        Command::Reconcile(request) => {
                            if pending.is_some() {
                                Event::Failed(Failure::Pending)
                            } else if request.base != committed {
                                Event::Failed(Failure::Stale { current: committed })
                            } else {
                                match crate::snapshot_diff::plan(
                                    &scene,
                                    &request.data,
                                    &worker_cancelled,
                                ) {
                                    Err(error) => Event::Failed(Failure::Reconcile(error)),
                                    Ok(changes) => match prepare_reconcile(
                                        &mut scene,
                                        committed,
                                        changes,
                                        &worker_cancelled,
                                    ) {
                                        Ok(prepared) => {
                                            pending = Some(prepared.version);
                                            Event::Prepared(prepared)
                                        }
                                        Err(error) => Event::Failed(Failure::Invalid(error)),
                                    },
                                }
                            }
                        }
                    };
                    if events.send(event).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("spawn scene update thread");
        Self { tx, rx, cancelled }
    }
    pub fn try_submit(&self, request: UpdateRequest) -> Result<(), TrySendError<UpdateRequest>> {
        self.tx
            .try_send(Command::Update(request))
            .map_err(|e| match e {
                TrySendError::Full(Command::Update(r)) => TrySendError::Full(r),
                TrySendError::Disconnected(Command::Update(r)) => TrySendError::Disconnected(r),
                _ => unreachable!(),
            })
    }
    pub fn try_reconcile(
        &self,
        request: ReconcileRequest,
    ) -> Result<(), TrySendError<ReconcileRequest>> {
        self.tx
            .try_send(Command::Reconcile(request))
            .map_err(|e| match e {
                TrySendError::Full(Command::Reconcile(r)) => TrySendError::Full(r),
                TrySendError::Disconnected(Command::Reconcile(r)) => TrySendError::Disconnected(r),
                _ => unreachable!(),
            })
    }
    pub fn try_lookup_projection(
        &self,
        request: ProjectionLookupRequest,
    ) -> Result<(), TrySendError<ProjectionLookupRequest>> {
        self.tx
            .try_send(Command::ProjectionLookup(request))
            .map_err(|e| match e {
                TrySendError::Full(Command::ProjectionLookup(r)) => TrySendError::Full(r),
                TrySendError::Disconnected(Command::ProjectionLookup(r)) => {
                    TrySendError::Disconnected(r)
                }
                _ => unreachable!(),
            })
    }
    pub fn try_verify_projection(
        &self,
        request: ProjectionVerifyRequest,
    ) -> Result<(), TrySendError<ProjectionVerifyRequest>> {
        self.tx
            .try_send(Command::ProjectionVerify(request))
            .map_err(|e| match e {
                TrySendError::Full(Command::ProjectionVerify(r)) => TrySendError::Full(r),
                TrySendError::Disconnected(Command::ProjectionVerify(r)) => {
                    TrySendError::Disconnected(r)
                }
                _ => unreachable!(),
            })
    }
    pub fn try_checkpoint(
        &self,
        request: CheckpointRequest,
    ) -> Result<(), TrySendError<CheckpointRequest>> {
        self.tx
            .try_send(Command::Checkpoint(request))
            .map_err(|e| match e {
                TrySendError::Full(Command::Checkpoint(r)) => TrySendError::Full(r),
                TrySendError::Disconnected(Command::Checkpoint(r)) => TrySendError::Disconnected(r),
                _ => unreachable!(),
            })
    }
    pub fn try_export_projection(
        &self,
        request: ExportProjectionRequest,
    ) -> Result<(), TrySendError<ExportProjectionRequest>> {
        self.tx
            .try_send(Command::ExportProjection(request))
            .map_err(|e| match e {
                TrySendError::Full(Command::ExportProjection(r)) => TrySendError::Full(r),
                TrySendError::Disconnected(Command::ExportProjection(r)) => {
                    TrySendError::Disconnected(r)
                }
                _ => unreachable!(),
            })
    }
    pub fn try_commit(&self, version: Version) -> Result<(), TrySendError<Version>> {
        self.tx
            .try_send(Command::Commit(version))
            .map_err(|e| match e {
                TrySendError::Full(Command::Commit(v)) => TrySendError::Full(v),
                TrySendError::Disconnected(Command::Commit(v)) => TrySendError::Disconnected(v),
                _ => unreachable!(),
            })
    }
    /// Blocking receive, intended only for the background coordinator.
    pub fn recv(&self) -> Result<Event, mpsc::RecvError> {
        self.rx.recv()
    }
}

/// Validate a retained projection against the replayed scene before reusing
/// its texture and hit map. Besides checking each stable ID, compare the
/// expected mode cardinality so omitted new entries cannot remain invisible.
fn verify_projection(
    scene: &Scene,
    mode: View,
    projection: &SparseProjection,
    cancelled: &AtomicBool,
) -> Option<bool> {
    let mut live = 0usize;
    for slot in 0..projection.len() {
        if slot % 4096 == 0 && cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let Some(id) = projection.id(slot) else {
            continue;
        };
        let Some(entry) = projection.get(slot) else {
            return Some(false);
        };
        let in_mode = match (mode, id) {
            (View::Files, TileId::File(_)) | (View::Folders, TileId::Directory(_)) => true,
            (View::Hybrid, TileId::File(_)) => scene.directory_leaf(&entry.parent) == Some(false),
            (View::Hybrid, TileId::Directory(_)) => scene.directory_leaf(&entry.path) == Some(true),
            _ => false,
        };
        if !in_mode || !scene.matches_entry(id, entry) {
            return Some(false);
        }
        live += 1;
    }
    let mut expected = 0usize;
    let mut visited = 0usize;
    let mut stopped = false;
    if mode != View::Folders {
        scene.for_each_file(|file| {
            visited += 1;
            if stopped || (visited % 4096 == 0 && cancelled.load(Ordering::Relaxed)) {
                stopped = true;
                return;
            }
            if mode == View::Files || scene.directory_leaf(&file.parent) == Some(false) {
                expected += 1;
            }
        });
    }
    if stopped || cancelled.load(Ordering::Relaxed) {
        return None;
    }
    if mode != View::Files {
        scene.for_each_directory(|fact| {
            visited += 1;
            if stopped || (visited % 4096 == 0 && cancelled.load(Ordering::Relaxed)) {
                stopped = true;
                return;
            }
            if (mode == View::Folders || (fact.enumeration_complete && !fact.has_subdirectories))
                && scene
                    .directory_id(&fact.path)
                    .and_then(|id| scene.tile(TileId::Directory(id)))
                    .is_some()
            {
                expected += 1;
            }
        });
    }
    if stopped || cancelled.load(Ordering::Relaxed) {
        None
    } else {
        Some(live == expected)
    }
}

fn lookup(
    scene: &Scene,
    path: &std::path::Path,
    identity: FileIdentity,
    kind: LookupKind,
) -> Option<TileId> {
    match kind {
        LookupKind::File => {
            let id = scene.file_id(path)?;
            (scene.file(id)?.identity == identity).then_some(TileId::File(id))
        }
        LookupKind::Directory => {
            let id = scene.directory_id(path)?;
            (scene.directory_identity(id)? == identity).then_some(TileId::Directory(id))
        }
    }
}
fn affected(scene: &Scene, changes: &[Change]) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    // Directory completeness is aggregated over the entire subtree. A
    // descendant becoming unreadable, excluded, or available changes the
    // delete eligibility of every ancestor, not only its direct parent.
    let add_ancestors = |paths: &mut BTreeSet<PathBuf>, path: &std::path::Path| {
        paths.extend(
            path.ancestors()
                .take_while(|ancestor| ancestor.starts_with(scene.root()))
                .map(std::path::Path::to_path_buf),
        );
    };
    for change in changes {
        match change {
            Change::UpsertFile(file) => {
                paths.insert(file.parent.to_path_buf());
                if let Some(old) = scene.file_id(&file.path).and_then(|id| scene.file(id)) {
                    paths.insert(old.parent.to_path_buf());
                }
            }
            Change::RemoveFile { path, .. } => {
                if let Some(old) = scene.file_id(path).and_then(|id| scene.file(id)) {
                    paths.insert(old.parent.to_path_buf());
                }
            }
            Change::MoveFile { from, to, .. } => {
                paths.insert(to.parent.to_path_buf());
                if let Some(old) = scene.file_id(from).and_then(|id| scene.file(id)) {
                    paths.insert(old.parent.to_path_buf());
                }
            }
            Change::UpsertDirectory(fact) => {
                add_ancestors(&mut paths, &fact.path);
            }
            Change::RemoveDirectory { path, .. } => {
                add_ancestors(&mut paths, path);
            }
            Change::RemoveLeafDirectory { path, parent, .. } => {
                add_ancestors(&mut paths, path);
                add_ancestors(&mut paths, &parent.path);
            }
            Change::RemoveSubtreeDirectory { path, parent, .. } => {
                paths.extend(scene.subtree_directories(path));
                add_ancestors(&mut paths, &parent.path);
            }
            Change::RemoveConfirmedSubtree { path, .. } => {
                paths.extend(scene.subtree_directories(path));
                if let Some(parent) = path.parent() {
                    add_ancestors(&mut paths, parent);
                }
            }
        }
    }
    paths
}
fn file_paths(changes: &[Change]) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    for change in changes {
        match change {
            Change::UpsertFile(file) => {
                paths.insert(file.path.clone());
            }
            Change::RemoveFile { path, .. } => {
                paths.insert(path.clone());
            }
            Change::MoveFile { from, to, .. } => {
                paths.insert(from.clone());
                paths.insert(to.path.clone());
            }
            _ => {}
        }
    }
    paths
}
fn leaf_transitions(
    scene: &Scene,
    changes: &[Change],
    affected: &BTreeSet<PathBuf>,
) -> BTreeSet<PathBuf> {
    let mut changed = BTreeSet::new();
    for change in changes {
        let (path, new_leaf) = match change {
            Change::UpsertDirectory(fact) => (
                &fact.path,
                Some(fact.enumeration_complete && !fact.has_subdirectories),
            ),
            Change::RemoveDirectory { path, .. } => (path, None),
            Change::RemoveLeafDirectory { path, .. } => (path, None),
            Change::RemoveSubtreeDirectory { path, .. } => {
                changed.extend(affected.iter().filter(|p| p.starts_with(path)).cloned());
                (path, None)
            }
            Change::RemoveConfirmedSubtree { path, .. } => {
                changed.extend(affected.iter().filter(|p| p.starts_with(path)).cloned());
                if let Some(parent) = path.parent() {
                    changed.insert(parent.to_path_buf());
                }
                (path, None)
            }
            _ => continue,
        };
        if scene.directory_leaf(path) != new_leaf {
            changed.insert(path.clone());
        }
    }
    // A leaf removal may turn its parent into a hybrid aggregate. Its files
    // must be captured before and after even when no file path changed.
    for change in changes {
        if let Change::RemoveLeafDirectory { parent, .. }
        | Change::RemoveSubtreeDirectory { parent, .. } = change
        {
            let new_leaf = parent.enumeration_complete && !parent.has_subdirectories;
            if scene.directory_leaf(&parent.path) != Some(new_leaf) {
                changed.insert(parent.path.clone());
            }
        }
    }
    changed
}
fn capture(
    scene: &Scene,
    directories: &BTreeSet<PathBuf>,
    files: &BTreeSet<PathBuf>,
    transitions: &BTreeSet<PathBuf>,
    view: View,
) -> BTreeMap<TileId, Tile> {
    let mut out = BTreeMap::new();
    for path in directories {
        let whole = transitions.contains(path)
            || view == View::Folders
            || (view == View::Hybrid && scene.directory_leaf(path) == Some(true));
        if whole {
            for tile in scene.project_directory(view, path) {
                out.insert(tile.id, tile);
            }
        }
    }
    if view != View::Folders {
        for path in files {
            let Some(id) = scene.file_id(path) else {
                continue;
            };
            let Some(file) = scene.file(id) else {
                continue;
            };
            if view == View::Hybrid && scene.directory_leaf(&file.parent) == Some(true) {
                continue;
            }
            if let Some(tile) = scene.tile(TileId::File(id)) {
                out.insert(tile.id, tile);
            }
        }
    }
    out
}

fn difference(before: BTreeMap<TileId, Tile>, after: BTreeMap<TileId, Tile>) -> Vec<TileDelta> {
    let mut joined: BTreeMap<TileId, (Option<Tile>, Option<Tile>)> = BTreeMap::new();
    for (id, tile) in before {
        joined.entry(id).or_default().0 = Some(tile);
    }
    for (id, tile) in after {
        joined.entry(id).or_default().1 = Some(tile);
    }
    joined
        .into_values()
        .filter_map(|(before, after)| (before != after).then_some(TileDelta { before, after }))
        .collect()
}

fn prepare(
    scene: &mut Scene,
    base: Version,
    changes: Vec<Change>,
) -> Result<Prepared, ChangeError> {
    prepare_inner(scene, base, changes, false, None)
}

// Reconciliation can remove populated subtrees, replace a directory with a
// file, and create new descendants in one snapshot. Stage those ordered
// operations in a private scene so a later conflict cannot partially mutate
// the actor's committed state. The clone shares the immutable source data.
fn prepare_reconcile(
    scene: &mut Scene,
    base: Version,
    changes: Vec<Change>,
    cancelled: &AtomicBool,
) -> Result<Prepared, ChangeError> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(ChangeError::Cancelled);
    }
    // The normal validated batch path is already atomic and retains the
    // existing scene without copying it. Only directory replacement and
    // file-to-directory exchanges require the ordered shadow transaction.
    let needs_shadow = changes.iter().any(|change| match change {
        Change::RemoveSubtreeDirectory { .. } | Change::RemoveConfirmedSubtree { .. } => true,
        Change::UpsertDirectory(fact) => scene.file_id(&fact.path).is_some(),
        _ => false,
    });
    if !needs_shadow {
        return prepare_inner(scene, base, changes, false, None);
    }
    let mut staged = scene.clone();
    let prepared = prepare_inner(&mut staged, base, changes, true, Some(cancelled))?;
    if cancelled.load(Ordering::Relaxed) {
        return Err(ChangeError::Cancelled);
    }
    *scene = staged;
    Ok(prepared)
}

fn prepare_inner(
    scene: &mut Scene,
    base: Version,
    changes: Vec<Change>,
    sequential: bool,
    cancelled: Option<&AtomicBool>,
) -> Result<Prepared, ChangeError> {
    let applied: Arc<[Change]> = changes.clone().into();
    // Resolve subtree membership once; this set is shared by projection
    // capture, topology proof and leaf transitions below.
    let paths = affected(scene, &changes);
    let mut structural_paths = BTreeSet::new();
    let mut root_facts_touched = false;
    let root = scene.root().to_path_buf();
    let root_before = scene
        .directory_id(&root)
        .and_then(|id| scene.directory_identity(id).map(|identity| (id, identity)));
    // An empty view alone cannot prove the Scene has no known members: other
    // modes can hide files and an empty directory still has its own identity.
    // This check is only about recorded members, never filesystem emptiness.
    let root_identity_change_requested = root_before.is_some_and(|(_, identity)| applied.iter().any(|change| {
        matches!(change, Change::UpsertDirectory(fact) if fact.path == root && fact.identity != identity)
    }));
    let empty_root_before = root_identity_change_requested
        && scene.directory_has_no_known_files(&root)
        && scene.subtree_directories(&root).len() == 1;
    for change in &changes {
        // Root-level direct files have no independent enclosing root child:
        // their parent partition and (in folder mode) root aggregate are
        // shared with every root-child component. Include both ends of a move
        // and the old parent of an upsert, not just its new path.
        let old_file_parent_is_root = |path: &std::path::Path| {
            scene
                .file_id(path)
                .and_then(|id| scene.file(id))
                .is_some_and(|file| file.parent.as_ref() == root.as_path())
        };
        root_facts_touched |= match change {
            Change::UpsertFile(file) => {
                file.parent.as_ref() == root.as_path() || old_file_parent_is_root(&file.path)
            }
            Change::RemoveFile { path, .. } => old_file_parent_is_root(path),
            Change::MoveFile { from, to, .. } => {
                to.parent.as_ref() == root.as_path() || old_file_parent_is_root(from)
            }
            // Adding or removing an immediate child can change the root's
            // unary/platform topology even if no child has visible files.
            Change::UpsertDirectory(fact) => {
                fact.path.parent() == Some(root.as_path())
                    && scene.directory_id(&fact.path).is_none()
            }
            Change::RemoveDirectory { path, .. } => path.parent() == Some(root.as_path()),
            Change::RemoveConfirmedSubtree { path, .. } => path.parent() == Some(root.as_path()),
            _ => false,
        };
        let (path, parent) = match change {
            Change::UpsertDirectory(fact) => (Some(fact.path.as_path()), None),
            Change::RemoveDirectory { path, .. } => (Some(path.as_path()), None),
            Change::RemoveConfirmedSubtree { path, .. } => (Some(path.as_path()), None),
            Change::RemoveLeafDirectory { path, parent, .. }
            | Change::RemoveSubtreeDirectory { path, parent, .. } => {
                (Some(path.as_path()), Some(parent))
            }
            _ => (None, None),
        };
        if let Some(path) = path {
            root_facts_touched |= path == root;
            structural_paths.insert(path.to_path_buf());
        }
        if let Change::RemoveSubtreeDirectory { path, .. }
        | Change::RemoveConfirmedSubtree { path, .. } = change
        {
            // Descendant directory facts disappear too, including empty
            // directories with no visible tile. A splitter must not infer
            // independence solely from the root of the removed subtree.
            structural_paths.extend(paths.iter().filter(|p| p.starts_with(path)).cloned());
        }
        if let Change::RemoveConfirmedSubtree { path, .. } = change {
            if let Some(parent) = path.parent() {
                root_facts_touched |= parent == root;
                structural_paths.insert(parent.to_path_buf());
            }
        }
        if let Some(parent) = parent {
            // Even if leaf status is unchanged, completeness or exclusion can
            // alter an aggregate's deletion eligibility. Treat every supplied
            // parent fact as touched; never infer equality from leaf alone.
            root_facts_touched |= parent.path == root;
            structural_paths.insert(parent.path.clone());
        }
    }
    let empty_before: BTreeSet<_> = structural_paths
        .iter()
        .filter(|path| path.as_path() != root && scene.absent_or_empty_complete_leaf(path))
        .cloned()
        .collect();
    let mut structural_scope: Option<PathBuf> = None;
    for change in &changes {
        let path = match change {
            Change::UpsertDirectory(fact)
                if scene.directory_id(&fact.path).is_none()
                    || scene.directory_leaf(&fact.path)
                        != Some(fact.enumeration_complete && !fact.has_subdirectories) =>
            {
                Some(fact.path.parent().unwrap_or(&fact.path))
            }
            Change::RemoveDirectory { path, .. } => Some(path.parent().unwrap_or(path)),
            Change::RemoveConfirmedSubtree { path, .. } => Some(path.parent().unwrap_or(path)),
            Change::RemoveLeafDirectory { parent, .. }
            | Change::RemoveSubtreeDirectory { parent, .. } => Some(parent.path.as_path()),
            _ => None,
        };
        if let Some(path) = path {
            structural_scope = Some(match structural_scope {
                None => path.to_path_buf(),
                Some(mut current) => {
                    while !path.starts_with(&current) {
                        if !current.pop() {
                            break;
                        }
                    }
                    current
                }
            });
        }
    }
    let files_changed = file_paths(&changes);
    let transitions = leaf_transitions(scene, &changes, &paths);
    let files = capture(scene, &paths, &files_changed, &transitions, View::Files);
    let folders = capture(scene, &paths, &files_changed, &transitions, View::Folders);
    let hybrid = capture(scene, &paths, &files_changed, &transitions, View::Hybrid);
    let impact = if sequential {
        let mut total = crate::scene_model::Impact::default();
        for (index, change) in changes.into_iter().enumerate() {
            if index % 1024 == 0 && cancelled.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err(ChangeError::Cancelled);
            }
            let next = scene.apply(change)?;
            total.directories.extend(next.directories);
            total.files.extend(next.files);
            total.removed.extend(next.removed);
        }
        total
    } else {
        scene.apply_batch(changes)?
    };
    debug_assert!(impact.directories.is_subset(&paths));
    let projections = ProjectionDelta {
        files: difference(
            files,
            capture(scene, &paths, &files_changed, &transitions, View::Files),
        ),
        folders: difference(
            folders,
            capture(scene, &paths, &files_changed, &transitions, View::Folders),
        ),
        hybrid: difference(
            hybrid,
            capture(scene, &paths, &files_changed, &transitions, View::Hybrid),
        ),
    };
    let version = Version {
        source_generation: base.source_generation,
        scene_version: base
            .scene_version
            .checked_add(1)
            .expect("scene version exhausted"),
    };
    let root_after = scene
        .directory_id(&root)
        .and_then(|id| scene.directory_identity(id).map(|identity| (id, identity)));
    let empty_root_replacement_safe =
        if empty_root_before && root_after.is_some() && root_before != root_after {
            // Inspect final known facts rather than guessing from the transaction
            // sequence: ordered updates may create, move or remove the same entry
            // repeatedly. All model states remain private until this result commits.
            let mut rooted = true;
            scene.for_each_directory(|fact| {
                rooted &= fact.path.strip_prefix(&root).ok().is_some_and(|relative| {
                    !relative
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                });
            });
            scene.for_each_file(|file| {
                rooted &= file.path.strip_prefix(&root).ok().is_some_and(|relative| {
                    !relative
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                }) && file.path.parent() == Some(file.parent.as_ref())
                    && scene.directory_id(&file.parent).is_some();
            });
            rooted
        } else {
            false
        };
    let visual_noop_directory_paths = empty_before
        .into_iter()
        .filter(|path| scene.absent_or_empty_complete_leaf(path))
        .collect();
    Ok(Prepared {
        base,
        version,
        projections,
        structural_scope,
        structural_paths: structural_paths.into_iter().collect(),
        root_facts_touched,
        root_identity_stable: root_before.is_some() && root_before == root_after,
        empty_root_replacement_safe,
        visual_noop_directory_paths,
        applied,
        sequential,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan;
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
    fn identity(n: u64) -> FileIdentity {
        FileIdentity {
            device: n,
            ..Default::default()
        }
    }
    fn file(path: &str, size: u64, object: u64, parent: u64) -> scan::FileEntry {
        let path = PathBuf::from(path);
        scan::FileEntry {
            parent: Arc::from(path.parent().unwrap()),
            path,
            size,
            identity: identity(object),
            parent_identity: identity(parent),
            parent_is_leaf: true,
            can_trash: true,
        }
    }
    #[test]
    fn affected_projection_deltas_match_complete_snapshots() {
        let fact = |path: &str, id: u64, children: bool, complete: bool| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: complete,
            has_subdirectories: children,
            excluded: false,
        };
        let source = scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/a/nested/file", 10, 4, 3)],
            directories: vec![
                fact("/disk", 1, true, true),
                fact("/disk/a", 2, true, true),
                fact("/disk/a/nested", 3, false, true),
                fact("/disk/unreadable", 5, false, false),
            ]
            .into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        };
        let mut scene = Scene::from_scan(Arc::new(source));
        let before: Vec<BTreeMap<TileId, Tile>> = [View::Files, View::Folders, View::Hybrid]
            .into_iter()
            .map(|view| {
                scene
                    .project(view)
                    .into_iter()
                    .map(|tile| (tile.id, tile))
                    .collect()
            })
            .collect();
        let base = Version {
            source_generation: 17,
            scene_version: 4,
        };
        let prepared = prepare(
            &mut scene,
            base,
            vec![
                Change::UpsertDirectory(fact("/disk/a", 2, true, true)),
                Change::UpsertDirectory(fact("/disk/unreadable", 5, true, false)),
                Change::UpsertFile(file("/disk/a/nested/file", 20, 4, 3)),
            ],
        )
        .unwrap();
        assert_eq!(prepared.base, base);
        assert_eq!(
            prepared.version,
            Version {
                source_generation: 17,
                scene_version: 5
            }
        );
        for (index, (view, actual)) in [
            (View::Files, &prepared.projections.files),
            (View::Folders, &prepared.projections.folders),
            (View::Hybrid, &prepared.projections.hybrid),
        ]
        .into_iter()
        .enumerate()
        {
            let after = scene
                .project(view)
                .into_iter()
                .map(|tile| (tile.id, tile))
                .collect();
            let expected = difference(before[index].clone(), after);
            assert_eq!(actual.len(), expected.len());
            for (got, want) in actual.iter().zip(&expected) {
                assert_eq!(got.before, want.before);
                assert_eq!(got.after, want.after);
            }
        }
        assert!(
            !prepared
                .visual_noop_directory_paths
                .contains(&PathBuf::from("/disk/unreadable")),
            "incomplete enumeration is not a proof of filesystem emptiness"
        );
        assert!(scene.file_id(Path::new("/disk/a/nested/file")).is_some());
    }
    #[test]
    fn root_replacement_tracks_known_members_and_preserves_unknown_state() {
        let fact = |path: &str, n: u64, complete: bool, children: bool| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: complete,
            has_subdirectories: children,
            excluded: false,
        };
        let source = |complete| {
            Arc::new(scan::ScanResult {
                root: "/disk".into(),
                files: vec![],
                directories: vec![fact("/disk", 1, complete, false)].into(),
                skipped: 0,
                errors: vec![],
                elapsed: Duration::ZERO,
            })
        };
        let batch = |parent_id| {
            vec![
                Change::UpsertDirectory(fact("/disk", 2, true, true)),
                Change::UpsertDirectory(fact("/disk/sub", 3, true, false)),
                Change::UpsertFile(file("/disk/sub/f", 10, 4, parent_id)),
            ]
        };
        let base = Version {
            source_generation: 99,
            scene_version: 0,
        };
        let mut valid = Scene::from_scan(source(true));
        let prepared = prepare(&mut valid, base, batch(3)).unwrap();
        assert!(!prepared.root_identity_stable);
        assert!(prepared.empty_root_replacement_safe);
        assert_eq!(prepared.base, base);
        assert_eq!(prepared.version.scene_version, base.scene_version + 1);
        let mut incomplete = Scene::from_scan(source(false));
        assert!(
            prepare(&mut incomplete, base, batch(3))
                .unwrap()
                .empty_root_replacement_safe,
            "no known old members permits a new view without inferring filesystem emptiness"
        );
        let mut incomplete_new = Scene::from_scan(source(false));
        let mut unknown_batch = batch(3);
        if let Change::UpsertDirectory(root_fact) = &mut unknown_batch[0] {
            root_fact.enumeration_complete = false;
        }
        if let Change::UpsertDirectory(child_fact) = &mut unknown_batch[1] {
            child_fact.enumeration_complete = false;
        }
        assert!(
            prepare(&mut incomplete_new, base, unknown_batch)
                .unwrap()
                .empty_root_replacement_safe
        );
        assert!(
            !incomplete_new
                .directory_leaf(Path::new("/disk/sub"))
                .unwrap()
        );
        assert!(
            !incomplete_new.project_directory(View::Folders, Path::new("/disk/sub"))[0].can_trash,
            "unknown new descendants must retain deletion protection"
        );
        let mut excluded_root = fact("/disk", 1, false, false);
        excluded_root.excluded = true;
        let mut excluded = Scene::from_scan(Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![],
            directories: vec![excluded_root].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        }));
        let mut excluded_batch = batch(3);
        if let Change::UpsertDirectory(child_fact) = &mut excluded_batch[1] {
            child_fact.excluded = true;
        }
        assert!(
            prepare(&mut excluded, base, excluded_batch)
                .unwrap()
                .empty_root_replacement_safe
        );
        assert!(
            !excluded.project_directory(View::Folders, Path::new("/disk/sub"))[0].can_trash,
            "excluded new directories must retain deletion protection"
        );
        let mut wrong_parent = Scene::from_scan(source(true));
        assert!(
            prepare(&mut wrong_parent, base, batch(9))
                .unwrap()
                .empty_root_replacement_safe,
            "an identity mismatch does not invalidate the view transaction"
        );
        let id = TileId::File(wrong_parent.file_id(Path::new("/disk/sub/f")).unwrap());
        let tile = wrong_parent.tile(id).unwrap();
        assert!(
            !tile.can_trash,
            "a file with a stale parent identity must not be deletable"
        );
        assert!(
            !wrong_parent.project_directory(View::Folders, Path::new("/disk/sub"))[0].can_trash
        );
        assert!(
            wrong_parent.matches_entry(id, &tile.to_entry()),
            "the protected projection must remain reusable"
        );
        let mut missing_ancestor = Scene::from_scan(source(false));
        let result = prepare(
            &mut missing_ancestor,
            base,
            vec![
                Change::UpsertDirectory(fact("/disk", 2, false, true)),
                Change::UpsertDirectory(fact("/disk/unknown/deep", 3, true, false)),
                Change::UpsertFile(file("/disk/unknown/deep/f", 10, 4, 3)),
            ],
        )
        .unwrap();
        assert!(
            result.empty_root_replacement_safe,
            "missing intermediate directory metadata must not block an accurate view of known entries"
        );
        assert!(
            missing_ancestor
                .directory_id(Path::new("/disk/unknown"))
                .is_none()
        );
        for (view, deltas) in [
            (View::Files, &result.projections.files),
            (View::Folders, &result.projections.folders),
            (View::Hybrid, &result.projections.hybrid),
        ] {
            let projected: BTreeMap<_, _> = missing_ancestor
                .project(view)
                .into_iter()
                .map(|tile| (tile.id, tile))
                .collect();
            let actual: BTreeMap<_, _> = deltas
                .iter()
                .map(|delta| {
                    assert!(delta.before.is_none());
                    let tile = delta.after.clone().unwrap();
                    (tile.id, tile)
                })
                .collect();
            assert_eq!(actual, projected, "{view:?}");
        }
        let mut has_child = Scene::from_scan(Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![],
            directories: vec![
                fact("/disk", 1, true, true),
                fact("/disk/old-empty", 5, true, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        }));
        assert_eq!(
            prepare(&mut has_child, base, batch(3)).unwrap_err(),
            ChangeError::NonEmptyDirectory
        );
        assert!(
            has_child
                .directory_id(Path::new("/disk/old-empty"))
                .is_some()
        );
        assert_eq!(
            has_child.directory_identity(has_child.directory_id(Path::new("/disk")).unwrap()),
            Some(identity(1))
        );
        let mut has_file = Scene::from_scan(Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/known", 2, 6, 1)],
            directories: vec![fact("/disk", 1, false, false)].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        }));
        assert_eq!(
            prepare(&mut has_file, base, batch(3)).unwrap_err(),
            ChangeError::NonEmptyDirectory
        );
        assert!(has_file.file_id(Path::new("/disk/known")).is_some());
        let mut missing_direct_parent = Scene::from_scan(source(true));
        assert_eq!(
            prepare(
                &mut missing_direct_parent,
                base,
                vec![
                    Change::UpsertDirectory(fact("/disk", 2, true, true)),
                    Change::UpsertFile(file("/disk/missing/f", 8, 7, 6)),
                ]
            )
            .unwrap_err(),
            ChangeError::MissingParent
        );
        assert_eq!(
            missing_direct_parent.directory_identity(
                missing_direct_parent
                    .directory_id(Path::new("/disk"))
                    .unwrap()
            ),
            Some(identity(1)),
            "invalid direct parent must not partially replace the root"
        );
        let synthesized = Scene::from_scan(Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/missing/f", 8, 7, 6)],
            directories: vec![fact("/disk", 1, false, true)].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        }));
        assert!(
            synthesized
                .directory_id(Path::new("/disk/missing"))
                .is_some()
        );
        assert_eq!(
            synthesized.directory_leaf(Path::new("/disk/missing")),
            Some(false),
            "a direct parent synthesized from an incomplete scan is not treated as a leaf"
        );
    }
    #[test]
    fn ordered_empty_root_replacement_proves_only_final_scene() {
        let fact = |path: &str, n: u64, children: bool| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let source = || {
            Arc::new(scan::ScanResult {
                root: "/disk".into(),
                files: vec![],
                directories: vec![fact("/disk", 1, false)].into(),
                skipped: 0,
                errors: vec![],
                elapsed: Duration::ZERO,
            })
        };
        let base = Version {
            source_generation: 106,
            scene_version: 0,
        };
        for retain_final_file in [false, true] {
            let mut scene = Scene::from_scan(source());
            let mut changes = vec![
                Change::UpsertDirectory(fact("/disk", 2, false)),
                Change::UpsertDirectory(fact("/disk", 2, true)),
                Change::UpsertDirectory(fact("/disk/sub", 3, false)),
                Change::UpsertFile(file("/disk/sub/first", 10, 4, 3)),
                Change::MoveFile {
                    from: "/disk/sub/first".into(),
                    identity: identity(4),
                    to: file("/disk/sub/moved", 10, 4, 3),
                },
                Change::RemoveFile {
                    path: "/disk/sub/moved".into(),
                    identity: identity(4),
                },
            ];
            if retain_final_file {
                changes.push(Change::UpsertFile(file("/disk/sub/final", 20, 4, 3)));
            }
            let result = prepare_inner(&mut scene, base, changes, true, None).unwrap();
            assert!(!result.root_identity_stable);
            assert!(result.empty_root_replacement_safe);
            assert_eq!(result.base, base);
            assert_eq!(result.version.scene_version, base.scene_version + 1);
            for (view, deltas) in [
                (View::Files, &result.projections.files),
                (View::Folders, &result.projections.folders),
                (View::Hybrid, &result.projections.hybrid),
            ] {
                let expected: BTreeMap<_, _> = scene
                    .project(view)
                    .into_iter()
                    .map(|tile| (tile.id, tile))
                    .collect();
                let actual: BTreeMap<_, _> = deltas
                    .iter()
                    .map(|delta| {
                        assert!(delta.before.is_none(), "old scene had no projected items");
                        let tile = delta.after.clone().expect("final tile");
                        (tile.id, tile)
                    })
                    .collect();
                assert_eq!(actual, expected, "final {view:?} projection only");
            }
            assert!(scene.file_id(Path::new("/disk/sub/first")).is_none());
            assert!(scene.file_id(Path::new("/disk/sub/moved")).is_none());
            assert_eq!(
                scene.file_id(Path::new("/disk/sub/final")).is_some(),
                retain_final_file
            );
        }
    }
    #[test]
    fn confirmed_subtree_removal_captures_unknown_parent_and_ancestor_tiles() {
        let fact = |path: &str, n: u64, children: bool| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let mut scene = Scene::from_scan(Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/root", 3, 8, 1),
                file("/disk/a/own", 5, 4, 2),
                file("/disk/a/sub/f", 8, 5, 3),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, true),
                fact("/disk/a/sub", 3, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        }));
        let before: Vec<_> = [View::Files, View::Folders, View::Hybrid]
            .into_iter()
            .map(|view| {
                scene
                    .project(view)
                    .into_iter()
                    .map(|t| (t.id, t))
                    .collect::<BTreeMap<_, _>>()
            })
            .collect();
        let base = Version {
            source_generation: 20,
            scene_version: 0,
        };
        let result = prepare(
            &mut scene,
            base,
            vec![Change::RemoveConfirmedSubtree {
                path: "/disk/a/sub".into(),
                identity: identity(3),
            }],
        )
        .unwrap();
        assert!(result.structural_paths.contains(&PathBuf::from("/disk/a")));
        assert!(
            result
                .structural_paths
                .contains(&PathBuf::from("/disk/a/sub"))
        );
        for (index, (view, deltas)) in [
            (View::Files, &result.projections.files),
            (View::Folders, &result.projections.folders),
            (View::Hybrid, &result.projections.hybrid),
        ]
        .into_iter()
        .enumerate()
        {
            let after = scene.project(view).into_iter().map(|t| (t.id, t)).collect();
            let expected = difference(before[index].clone(), after);
            assert_eq!(deltas.len(), expected.len(), "{view:?}");
            for (actual, expected) in deltas.iter().zip(expected) {
                assert_eq!(actual.before, expected.before, "{view:?}");
                assert_eq!(actual.after, expected.after, "{view:?}");
            }
        }
        for path in ["/disk", "/disk/a"] {
            let tile = scene
                .project_directory(View::Folders, Path::new(path))
                .pop()
                .unwrap();
            assert!(
                !tile.can_trash,
                "unknown descendant must block deleting {path}"
            );
        }
        assert!(!scene.directory_leaf(Path::new("/disk/a")).unwrap());
    }
    #[test]
    fn hybrid_file_reappearance_has_prior_live_files_identity_in_capture() {
        let fact = |path: &str, n: u64, children: bool| scan::DirectoryFact {
            path: path.into(),
            identity: identity(n),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/a/f", 10, 7, 2)],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, false),
                fact("/disk/b", 3, true),
                fact("/disk/b/empty", 4, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let base = Version {
            source_generation: 21,
            scene_version: 0,
        };
        for changes in [
            vec![
                Change::UpsertDirectory(fact("/disk/a", 2, true)),
                Change::UpsertDirectory(fact("/disk/a/empty", 5, false)),
            ],
            vec![Change::MoveFile {
                from: "/disk/a/f".into(),
                identity: identity(7),
                to: file("/disk/b/f", 10, 7, 3),
            }],
        ] {
            let mut scene = Scene::from_scan(source.clone());
            let old_id = TileId::File(scene.file_id(Path::new("/disk/a/f")).unwrap());
            assert!(
                scene
                    .project(View::Hybrid)
                    .iter()
                    .all(|tile| tile.id != old_id)
            );
            let paths = affected(&scene, &changes);
            let changed_files = file_paths(&changes);
            let transitions = leaf_transitions(&scene, &changes, &paths);
            let captured = capture(&scene, &paths, &changed_files, &transitions, View::Files);
            let before = captured
                .get(&old_id)
                .expect("live file in pre-transaction capture");
            assert_eq!(before.identity, identity(7));
            let prepared = prepare(&mut scene, base, changes).unwrap();
            let reappeared = prepared
                .projections
                .hybrid
                .iter()
                .find(|delta| {
                    delta.before.is_none() && delta.after.as_ref().is_some_and(|t| t.id == old_id)
                })
                .and_then(|delta| delta.after.as_ref())
                .expect("hybrid file reappears");
            assert_eq!(reappeared.identity, before.identity);
            assert_eq!(
                scene
                    .file(match old_id {
                        TileId::File(id) => id,
                        _ => unreachable!(),
                    })
                    .unwrap()
                    .identity,
                before.identity
            );
        }
        // Removing a real entry and creating the same filesystem identity
        // produces a new generational ID, not permission to revive its slot.
        let mut scene = Scene::from_scan(source);
        let old_id = scene.file_id(Path::new("/disk/a/f")).unwrap();
        let removed = prepare(
            &mut scene,
            base,
            vec![Change::RemoveFile {
                path: "/disk/a/f".into(),
                identity: identity(7),
            }],
        )
        .unwrap();
        assert!(removed.projections.files.iter().any(|delta| {
            delta
                .before
                .as_ref()
                .is_some_and(|tile| tile.id == TileId::File(old_id))
                && delta.after.is_none()
        }));
        prepare(
            &mut scene,
            removed.version,
            vec![Change::UpsertFile(file("/disk/a/f", 10, 7, 2))],
        )
        .unwrap();
        assert_ne!(scene.file_id(Path::new("/disk/a/f")).unwrap(), old_id);
        assert!(scene.file(old_id).is_none());
    }
    #[test]
    fn directory_transactions_report_exact_projection_changes_for_all_modes() {
        let fact = |path: &str, id: u64, children: bool, complete: bool, excluded: bool| {
            scan::DirectoryFact {
                path: path.into(),
                identity: identity(id),
                enumeration_complete: complete,
                has_subdirectories: children,
                excluded,
            }
        };
        let mut scene = Scene::from_scan(Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/own", 7, 4, 2),
                file("/disk/a/nested/file", 10, 5, 3),
                file("/disk/b/file", 11, 7, 6),
            ],
            directories: vec![
                fact("/disk", 1, true, true, false),
                fact("/disk/a", 2, true, true, false),
                fact("/disk/a/nested", 3, false, true, false),
                fact("/disk/b", 6, false, true, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        }));
        let transactions = vec![
            // A descendant becoming unreadable changes deletion eligibility of
            // ancestor folder tiles, even though their files did not change.
            vec![Change::UpsertDirectory(fact(
                "/disk/a/nested",
                3,
                false,
                false,
                false,
            ))],
            vec![Change::UpsertDirectory(fact(
                "/disk/a/nested",
                3,
                false,
                true,
                true,
            ))],
            vec![Change::UpsertDirectory(fact(
                "/disk/a/nested",
                3,
                false,
                true,
                false,
            ))],
            // Removing the last child both removes its tiles and folds its
            // parent in Hybrid mode; a different subtree changes in the batch.
            vec![
                Change::RemoveSubtreeDirectory {
                    path: "/disk/a/nested".into(),
                    identity: identity(3),
                    parent: fact("/disk/a", 2, false, true, false),
                },
                Change::UpsertFile(file("/disk/b/file", 19, 7, 6)),
            ],
            // Expanding that parent while adding a child with an unknown scan
            // state must preserve its files and change only known facts.
            vec![
                Change::UpsertDirectory(fact("/disk/a", 2, true, true, false)),
                Change::UpsertDirectory(fact("/disk/a/new", 8, false, false, false)),
            ],
        ];
        for (revision, changes) in transactions.into_iter().enumerate() {
            let before: Vec<_> = [View::Files, View::Folders, View::Hybrid]
                .into_iter()
                .map(|view| {
                    scene
                        .project(view)
                        .into_iter()
                        .map(|tile| (tile.id, tile))
                        .collect::<BTreeMap<_, _>>()
                })
                .collect();
            let base = Version {
                source_generation: 11,
                scene_version: revision as u64,
            };
            let prepared =
                prepare_reconcile(&mut scene, base, changes, &AtomicBool::new(false)).unwrap();
            assert_eq!(prepared.base, base);
            assert_eq!(prepared.version.scene_version, base.scene_version + 1);
            for (index, (view, actual)) in [
                (View::Files, &prepared.projections.files),
                (View::Folders, &prepared.projections.folders),
                (View::Hybrid, &prepared.projections.hybrid),
            ]
            .into_iter()
            .enumerate()
            {
                let after = scene
                    .project(view)
                    .into_iter()
                    .map(|tile| (tile.id, tile))
                    .collect();
                let expected = difference(before[index].clone(), after);
                assert_eq!(
                    actual.len(),
                    expected.len(),
                    "revision {revision}, mode {view:?}"
                );
                for (got, want) in actual.iter().zip(expected) {
                    assert_eq!(
                        got.before, want.before,
                        "revision {revision}, mode {view:?}"
                    );
                    assert_eq!(got.after, want.after, "revision {revision}, mode {view:?}");
                }
            }
        }
    }
    fn actor() -> SceneActor {
        let fact = |path: &str, id: u64, has_subdirectories: bool| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories,
            excluded: false,
        };
        let data = scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/a/f", 10, 3, 2), file("/disk/a/g", 5, 4, 2)],
            directories: vec![fact("/disk", 1, true), fact("/disk/a", 2, false)].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        SceneActor::start(
            Scene::from_scan(Arc::new(data)),
            9,
            egui::Context::default(),
        )
    }
    fn get(actor: &SceneActor) -> Event {
        actor
            .rx
            .recv_timeout(Duration::from_secs(5))
            .expect("actor response")
    }
    #[test]
    fn structural_proof_includes_empty_directory_and_root_fact_changes() {
        let actor = actor();
        let base = Version {
            source_generation: 9,
            scene_version: 0,
        };
        let fact = |path: &str, id: u64, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        actor
            .try_submit(UpdateRequest {
                base,
                changes: vec![
                    Change::UpsertDirectory(fact("/disk/a/empty", 5, false)),
                    Change::UpsertDirectory(fact("/disk/a", 2, true)),
                ],
            })
            .unwrap();
        let Event::Prepared(prepared) = get(&actor) else {
            panic!("prepared")
        };
        assert!(
            prepared
                .structural_paths
                .contains(&PathBuf::from("/disk/a/empty"))
        );
        assert!(
            prepared
                .structural_paths
                .contains(&PathBuf::from("/disk/a"))
        );
        assert!(!prepared.root_facts_touched);
        assert!(prepared.root_identity_stable);
        assert!(
            prepared
                .visual_noop_directory_paths
                .contains(&PathBuf::from("/disk/a/empty"))
        );
        assert!(
            !prepared
                .visual_noop_directory_paths
                .contains(&PathBuf::from("/disk/a"))
        );
        actor.try_commit(prepared.version).unwrap();
        actor
            .try_submit(UpdateRequest {
                base: prepared.version,
                changes: vec![Change::UpsertDirectory(fact("/disk", 1, true))],
            })
            .unwrap();
        let Event::Prepared(root) = get(&actor) else {
            panic!("root")
        };
        assert!(root.root_facts_touched);
        assert!(root.structural_paths.contains(&PathBuf::from("/disk")));
    }
    #[test]
    fn structural_proof_refuses_replaced_root_identity() {
        let fact = |id| scan::DirectoryFact {
            path: "/disk".into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories: false,
            excluded: false,
        };
        let data = scan::ScanResult {
            root: "/disk".into(),
            files: Vec::new(),
            directories: vec![fact(1)].into(),
            skipped: 0,
            errors: Vec::new(),
            elapsed: Duration::ZERO,
        };
        let actor = SceneActor::start(
            Scene::from_scan(Arc::new(data)),
            9,
            egui::Context::default(),
        );
        actor
            .try_submit(UpdateRequest {
                base: Version {
                    source_generation: 9,
                    scene_version: 0,
                },
                changes: vec![Change::UpsertDirectory(fact(2))],
            })
            .unwrap();
        let Event::Prepared(result) = get(&actor) else {
            panic!("root replacement")
        };
        assert!(!result.root_identity_stable);
        assert!(result.root_facts_touched);
    }
    #[test]
    fn root_partition_effects_are_not_treated_as_independent_children() {
        let source = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/root-file", 4, 10, 1),
                file("/disk/a/nested", 8, 11, 2),
            ],
            directories: vec![
                scan::DirectoryFact {
                    path: "/disk".into(),
                    identity: identity(1),
                    enumeration_complete: true,
                    has_subdirectories: true,
                    excluded: false,
                },
                scan::DirectoryFact {
                    path: "/disk/a".into(),
                    identity: identity(2),
                    enumeration_complete: true,
                    has_subdirectories: false,
                    excluded: false,
                },
                scan::DirectoryFact {
                    path: "/disk/empty".into(),
                    identity: identity(3),
                    enumeration_complete: true,
                    has_subdirectories: false,
                    excluded: false,
                },
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let base = Version {
            source_generation: 1,
            scene_version: 0,
        };
        let mut changed_root_file = source.files[0].clone();
        changed_root_file.size = 5;
        let mut changed_nested_file = source.files[1].clone();
        changed_nested_file.size = 9;
        let changes = [
            (Change::UpsertFile(changed_root_file), true),
            (Change::UpsertFile(changed_nested_file.clone()), false),
            (
                Change::RemoveFile {
                    path: source.files[0].path.clone(),
                    identity: source.files[0].identity,
                },
                true,
            ),
            (
                Change::MoveFile {
                    from: source.files[1].path.clone(),
                    identity: source.files[1].identity,
                    to: file("/disk/moved", 8, 11, 1),
                },
                true,
            ),
            (
                Change::MoveFile {
                    from: source.files[0].path.clone(),
                    identity: source.files[0].identity,
                    to: file("/disk/a/moved", 4, 10, 2),
                },
                true,
            ),
            (
                Change::UpsertDirectory(scan::DirectoryFact {
                    path: "/disk/new-empty".into(),
                    identity: identity(4),
                    enumeration_complete: true,
                    has_subdirectories: false,
                    excluded: false,
                }),
                true,
            ),
            (
                Change::RemoveDirectory {
                    path: "/disk/empty".into(),
                    identity: identity(3),
                },
                true,
            ),
        ];
        for (change, expected) in changes {
            let mut scene = Scene::from_scan(source.clone());
            let prepared = prepare(&mut scene, base, vec![change]).unwrap();
            assert_eq!(prepared.root_facts_touched, expected);
        }
    }
    #[test]
    fn removed_subtree_reports_empty_descendant_facts() {
        let fact = |path: &str, id: u64, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let data = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/a/f", 4, 10, 2)],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, true),
                fact("/disk/a/empty", 3, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let mut scene = Scene::from_scan(data);
        let prepared = prepare(
            &mut scene,
            Version {
                source_generation: 1,
                scene_version: 0,
            },
            vec![Change::RemoveSubtreeDirectory {
                path: "/disk/a".into(),
                identity: identity(2),
                parent: fact("/disk", 1, false),
            }],
        )
        .unwrap();
        for path in ["/disk", "/disk/a", "/disk/a/empty"] {
            assert!(
                prepared
                    .structural_paths
                    .iter()
                    .any(|p| p == Path::new(path)),
                "missing {path}"
            );
        }
        assert!(prepared.root_facts_touched);
    }
    #[test]
    fn descendant_completeness_changes_publish_ancestor_delete_eligibility() {
        let fact = |path: &str, id: u64, complete, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: complete,
            has_subdirectories: children,
            excluded: false,
        };
        let data = scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/top", 1, 10, 2),
                file("/disk/a/p/direct", 1, 11, 3),
                file("/disk/a/p/c/item", 1, 12, 4),
            ],
            directories: vec![
                fact("/disk", 1, true, true),
                fact("/disk/a", 2, true, true),
                fact("/disk/a/p", 3, true, true),
                fact("/disk/a/p/c", 4, true, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let actor = SceneActor::start(
            Scene::from_scan(Arc::new(data)),
            9,
            egui::Context::default(),
        );
        let base = Version {
            source_generation: 9,
            scene_version: 0,
        };
        actor
            .try_submit(UpdateRequest {
                base,
                changes: vec![Change::UpsertDirectory(fact(
                    "/disk/a/p/c",
                    4,
                    false,
                    false,
                ))],
            })
            .unwrap();
        let Event::Prepared(prepared) = get(&actor) else {
            panic!("expected prepared update")
        };
        for path in ["/disk/a", "/disk/a/p"] {
            let delta = prepared
                .projections
                .folders
                .iter()
                .find(|delta| {
                    delta
                        .before
                        .as_ref()
                        .is_some_and(|tile| tile.path == Path::new(path))
                })
                .expect("ancestor aggregate must update");
            assert!(delta.before.as_ref().unwrap().can_trash, "{path}");
            assert!(!delta.after.as_ref().unwrap().can_trash, "{path}");
        }
        actor.try_commit(prepared.version).unwrap();
        actor
            .try_submit(UpdateRequest {
                base: prepared.version,
                changes: vec![Change::UpsertDirectory(fact("/disk/a/p/c", 4, true, false))],
            })
            .unwrap();
        let Event::Prepared(restored) = get(&actor) else {
            panic!("expected restored update")
        };
        for path in ["/disk/a", "/disk/a/p"] {
            let delta = restored
                .projections
                .folders
                .iter()
                .find(|delta| {
                    delta
                        .before
                        .as_ref()
                        .is_some_and(|tile| tile.path == Path::new(path))
                })
                .expect("ancestor aggregate must update");
            assert!(!delta.before.as_ref().unwrap().can_trash, "{path}");
            assert!(delta.after.as_ref().unwrap().can_trash, "{path}");
        }
    }
    #[test]
    fn retained_projection_requires_complete_matching_mode_and_identity() {
        let fact = scan::DirectoryFact {
            path: "/disk".into(),
            identity: identity(1),
            enumeration_complete: true,
            has_subdirectories: false,
            excluded: false,
        };
        let data = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/a", 10, 2, 1), file("/disk/b", 20, 3, 1)],
            directories: vec![fact].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(data.clone());
        let a = TileId::File(scene.file_id(Path::new("/disk/a")).unwrap());
        let b = TileId::File(scene.file_id(Path::new("/disk/b")).unwrap());
        let projection = SparseProjection::new(data.clone(), vec![a, b]).unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(
            verify_projection(&scene, View::Files, &projection, &cancel),
            Some(true)
        );
        assert_eq!(
            verify_projection(&scene, View::Hybrid, &projection, &cancel),
            Some(false)
        );
        let missing = SparseProjection::new_optional(data.clone(), vec![Some(a), None]).unwrap();
        assert_eq!(
            verify_projection(&scene, View::Files, &missing, &cancel),
            Some(false)
        );
        let mut changed_files = data.files.clone();
        changed_files[0].size += 1;
        let changed = scan::ScanResult {
            root: data.root.clone(),
            files: changed_files,
            directories: data.directories.clone(),
            skipped: data.skipped,
            errors: data.errors.clone(),
            elapsed: data.elapsed,
        };
        let stale = SparseProjection::new(Arc::new(changed), vec![a, b]).unwrap();
        assert_eq!(
            verify_projection(&scene, View::Files, &stale, &cancel),
            Some(false)
        );
        cancel.store(true, Ordering::Relaxed);
        assert_eq!(
            verify_projection(&scene, View::Files, &projection, &cancel),
            None
        );
    }
    #[test]
    fn checkpoint_uses_only_committed_cow_scene_at_requested_version() {
        let fact = scan::DirectoryFact {
            path: "/disk".into(),
            identity: identity(1),
            enumeration_complete: true,
            has_subdirectories: false,
            excluded: false,
        };
        let data = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/a", 10, 2, 1)],
            directories: vec![fact].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let scene = Scene::from_scan(data.clone());
        let actor = SceneActor::start(scene, 9, egui::Context::default());
        let base = Version {
            source_generation: 9,
            scene_version: 0,
        };
        actor
            .try_checkpoint(CheckpointRequest {
                base: Version {
                    scene_version: 1,
                    ..base
                },
            })
            .unwrap();
        assert!(matches!(get(&actor), Event::Failed(Failure::Stale { .. })));
        actor.try_checkpoint(CheckpointRequest { base }).unwrap();
        let Event::Checkpoint {
            base: version,
            checkpoint,
        } = get(&actor)
        else {
            panic!("checkpoint")
        };
        assert_eq!(version, base);
        let mut journal = crate::scene_journal::Journal::new(data, 9, Arc::new(BTreeSet::new()));
        journal.adopt(checkpoint).unwrap();
        actor
            .try_submit(UpdateRequest {
                base,
                changes: vec![Change::UpsertFile(file("/disk/b", 11, 3, 1))],
            })
            .unwrap();
        let Event::Prepared(prepared) = get(&actor) else {
            panic!("prepared")
        };
        actor.try_checkpoint(CheckpointRequest { base }).unwrap();
        assert!(matches!(get(&actor), Event::Failed(Failure::Pending)));
        actor.try_commit(prepared.version).unwrap();
        let old = journal.replay(&AtomicBool::new(false)).unwrap();
        assert!(old.scene.file_id(Path::new("/disk/b")).is_none());
        journal
            .append(
                base,
                prepared.version,
                Arc::new(crate::scene_journal::Record::Batch(prepared.applied)),
            )
            .unwrap();
        let restored = journal.replay(&AtomicBool::new(false)).unwrap();
        assert!(restored.scene.file_id(Path::new("/disk/b")).is_some());
    }
    #[test]
    fn reconcile_mixed_subtree_replacement_and_files_commits_as_one_version() {
        let fact = |path: &str, id: u64, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let data = scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/old", 10, 3, 2),
                file("/disk/b/old", 3, 5, 4),
                file("/disk/keep", 4, 6, 1),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/a", 2, false),
                fact("/disk/b", 4, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let actor = SceneActor::start(
            Scene::from_scan(Arc::new(data)),
            9,
            egui::Context::default(),
        );
        let next = scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/a/new", 12, 8, 7), file("/disk/keep", 9, 6, 1)],
            directories: vec![fact("/disk", 1, true), fact("/disk/a", 7, false)].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let base = Version {
            source_generation: 9,
            scene_version: 0,
        };
        actor
            .try_reconcile(ReconcileRequest {
                base,
                data: Arc::new(next),
            })
            .unwrap();
        let Event::Prepared(prepared) = get(&actor) else {
            panic!("expected reconciliation")
        };
        assert_eq!(prepared.version.scene_version, 1);
        assert_eq!(
            prepared.structural_scope.as_deref(),
            Some(Path::new("/disk"))
        );
        for path in ["/disk/a/old", "/disk/b/old"] {
            assert!(
                prepared.projections.files.iter().any(|d| d
                    .before
                    .as_ref()
                    .is_some_and(|t| t.path == Path::new(path))
                    && d.after.is_none()),
                "{path}"
            );
        }
        assert!(prepared.projections.files.iter().any(|d| {
            d.after
                .as_ref()
                .is_some_and(|t| t.path == Path::new("/disk/a/new"))
        }));
        assert!(prepared.projections.hybrid.iter().any(|d| {
            d.after
                .as_ref()
                .is_some_and(|t| t.path == Path::new("/disk/a"))
        }));
        actor.try_commit(prepared.version).unwrap();
        actor
            .try_export_projection(ExportProjectionRequest {
                base: prepared.version,
                mode: View::Files,
            })
            .unwrap();
        let Event::ExportProjection { tiles, .. } = get(&actor) else {
            panic!("expected projection")
        };
        assert_eq!(tiles.len(), 2);
        assert!(
            tiles
                .iter()
                .any(|t| t.path == Path::new("/disk/keep") && t.size == 9)
        );
    }
    #[test]
    fn failed_reconcile_shadow_does_not_partially_remove_subtree() {
        let fact = |path: &str, id: u64, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let data = scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/a/old", 10, 3, 2)],
            directories: vec![fact("/disk", 1, true), fact("/disk/a", 2, false)].into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let mut scene = Scene::from_scan(Arc::new(data));
        let existing = scene.file_id(Path::new("/disk/a/old"));
        let result = prepare_reconcile(
            &mut scene,
            Version {
                source_generation: 9,
                scene_version: 0,
            },
            vec![
                Change::RemoveSubtreeDirectory {
                    path: "/disk/a".into(),
                    identity: identity(2),
                    parent: fact("/disk", 1, true),
                },
                Change::UpsertFile(file("/disk/missing/new", 1, 4, 7)),
            ],
            &AtomicBool::new(false),
        );
        assert_eq!(result.unwrap_err(), ChangeError::MissingParent);
        assert_eq!(scene.file_id(Path::new("/disk/a/old")), existing);
        assert!(scene.directory_id(Path::new("/disk/a")).is_some());
    }
    #[test]
    fn reconcile_excluded_and_unknown_subtrees_updates_all_modes_safely() {
        let fact = |path: &str, id: u64, complete: bool, children: bool, excluded: bool| {
            scan::DirectoryFact {
                path: path.into(),
                identity: identity(id),
                enumeration_complete: complete,
                has_subdirectories: children,
                excluded,
            }
        };
        let initial = scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/trash/a", 10, 3, 2),
                file("/disk/unknown/b", 5, 5, 4),
            ],
            directories: vec![
                fact("/disk", 1, true, true, false),
                fact("/disk/trash", 2, true, false, false),
                fact("/disk/unknown", 4, true, false, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let actor = SceneActor::start(
            Scene::from_scan(Arc::new(initial)),
            9,
            egui::Context::default(),
        );
        let next = scan::ScanResult {
            root: "/disk".into(),
            files: vec![],
            directories: vec![
                fact("/disk", 1, true, true, false),
                fact("/disk/trash", 2, false, false, true),
                fact("/disk/unknown", 4, false, false, false),
            ]
            .into(),
            skipped: 2,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let base = Version {
            source_generation: 9,
            scene_version: 0,
        };
        actor
            .try_reconcile(ReconcileRequest {
                base,
                data: Arc::new(next),
            })
            .unwrap();
        let Event::Prepared(prepared) = get(&actor) else {
            panic!("expected reconciliation")
        };
        assert_eq!(
            prepared.structural_scope.as_deref(),
            Some(Path::new("/disk"))
        );
        assert!(prepared.projections.files.iter().any(|d| {
            d.before
                .as_ref()
                .is_some_and(|t| t.path == Path::new("/disk/trash/a"))
                && d.after.is_none()
        }));
        assert!(prepared.projections.files.iter().any(|d| {
            d.after
                .as_ref()
                .is_some_and(|t| t.path == Path::new("/disk/unknown/b") && !t.can_trash)
        }));
        assert!(prepared.projections.folders.iter().any(|d| {
            d.before
                .as_ref()
                .is_some_and(|t| t.path == Path::new("/disk/trash"))
                && d.after.is_none()
        }));
        assert!(prepared.projections.folders.iter().any(|d| {
            d.after
                .as_ref()
                .is_some_and(|t| t.path == Path::new("/disk/unknown") && !t.can_trash)
        }));
        assert!(prepared.projections.hybrid.iter().any(|d| {
            d.after
                .as_ref()
                .is_some_and(|t| t.path == Path::new("/disk/unknown/b") && !t.can_trash)
        }));
        actor.try_commit(prepared.version).unwrap();
        for mode in [View::Files, View::Folders, View::Hybrid] {
            actor
                .try_export_projection(ExportProjectionRequest {
                    base: prepared.version,
                    mode,
                })
                .unwrap();
            let Event::ExportProjection { tiles, .. } = get(&actor) else {
                panic!("expected projection")
            };
            assert!(tiles.iter().all(|t| !t.path.starts_with("/disk/trash")));
            assert_eq!(tiles.len(), 1);
            assert!(!tiles[0].can_trash);
        }
    }
    #[test]
    fn removing_leaf_captures_subtree_and_parent_hybrid_transition() {
        let fact = |path: &str, id: u64, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let data = scan::ScanResult {
            root: "/disk".into(),
            files: vec![file("/disk/p/x", 3, 3, 2), file("/disk/p/a/y", 4, 5, 4)],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/p", 2, true),
                fact("/disk/p/a", 4, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let actor = SceneActor::start(
            Scene::from_scan(Arc::new(data)),
            9,
            egui::Context::default(),
        );
        actor
            .try_submit(UpdateRequest {
                base: Version {
                    source_generation: 9,
                    scene_version: 0,
                },
                changes: vec![Change::RemoveLeafDirectory {
                    path: "/disk/p/a".into(),
                    identity: identity(4),
                    parent: fact("/disk/p", 2, false),
                }],
            })
            .unwrap();
        let Event::Prepared(p) = get(&actor) else {
            panic!("prepared")
        };
        assert!(p.projections.files.iter().any(|d| {
            d.before
                .as_ref()
                .is_some_and(|t| t.path == PathBuf::from("/disk/p/a/y"))
                && d.after.is_none()
        }));
        assert!(p.projections.folders.iter().any(|d| {
            d.before
                .as_ref()
                .is_some_and(|t| t.path == PathBuf::from("/disk/p/a"))
                && d.after.is_none()
        }));
        assert!(p.projections.hybrid.iter().any(|d| {
            d.before
                .as_ref()
                .is_some_and(|t| t.path == PathBuf::from("/disk/p/x"))
                && d.after.is_none()
        }));
        assert!(p.projections.hybrid.iter().any(|d| {
            d.after
                .as_ref()
                .is_some_and(|t| t.path == PathBuf::from("/disk/p"))
        }));
    }
    #[test]
    fn removing_subtree_captures_all_descendants_in_each_projection() {
        let fact = |path: &str, id: u64, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let data = scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/p/direct", 2, 30, 2),
                file("/disk/p/tree/a", 3, 31, 3),
                file("/disk/p/tree/nested/b", 4, 32, 4),
                file("/disk/other/untouched", 5, 33, 5),
            ],
            directories: vec![
                fact("/disk", 1, true),
                fact("/disk/p", 2, true),
                fact("/disk/p/tree", 3, true),
                fact("/disk/p/tree/nested", 4, false),
                fact("/disk/p/tree/empty", 6, false),
                fact("/disk/other", 5, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        };
        let mut scene = Scene::from_scan(Arc::new(data));
        let prepared = prepare(
            &mut scene,
            Version {
                source_generation: 1,
                scene_version: 0,
            },
            vec![Change::RemoveSubtreeDirectory {
                path: "/disk/p/tree".into(),
                identity: identity(3),
                parent: fact("/disk/p", 2, false),
            }],
        )
        .unwrap();
        let removed = |deltas: &[TileDelta], path: &str| {
            deltas.iter().any(|delta| {
                delta
                    .before
                    .as_ref()
                    .is_some_and(|tile| tile.path == Path::new(path))
                    && delta.after.is_none()
            })
        };
        assert!(removed(&prepared.projections.files, "/disk/p/tree/a"));
        assert!(removed(
            &prepared.projections.files,
            "/disk/p/tree/nested/b"
        ));
        assert!(removed(&prepared.projections.folders, "/disk/p/tree"));
        assert!(removed(
            &prepared.projections.folders,
            "/disk/p/tree/nested"
        ));
        assert!(removed(&prepared.projections.hybrid, "/disk/p/tree/a"));
        assert!(removed(&prepared.projections.hybrid, "/disk/p/tree/nested"));
        assert!(removed(&prepared.projections.hybrid, "/disk/p/direct"));
        assert!(prepared.projections.hybrid.iter().any(|delta| {
            delta
                .after
                .as_ref()
                .is_some_and(|tile| tile.path == Path::new("/disk/p"))
        }));
        for delta in prepared
            .projections
            .files
            .iter()
            .chain(&prepared.projections.folders)
            .chain(&prepared.projections.hybrid)
        {
            assert!(
                !delta
                    .before
                    .as_ref()
                    .is_some_and(|tile| tile.path.starts_with("/disk/other"))
            );
        }
    }
    #[test]
    fn prepares_sparse_updates_and_requires_commit_before_next_batch() {
        let actor = actor();
        let base = Version {
            source_generation: 9,
            scene_version: 0,
        };
        actor
            .try_submit(UpdateRequest {
                base,
                changes: vec![Change::RemoveFile {
                    path: "/disk/a/f".into(),
                    identity: identity(3),
                }],
            })
            .unwrap();
        let Event::Prepared(prepared) = get(&actor) else {
            panic!("prepared")
        };
        assert_eq!(prepared.base, base);
        assert_eq!(prepared.version.scene_version, 1);
        assert_eq!(prepared.projections.files.len(), 1);
        assert!(prepared.projections.files[0].before.is_some());
        assert!(prepared.projections.files[0].after.is_none());
        assert_eq!(prepared.projections.folders.len(), 1);
        assert_eq!(
            prepared.projections.folders[0]
                .before
                .as_ref()
                .unwrap()
                .size,
            15
        );
        assert_eq!(
            prepared.projections.folders[0].after.as_ref().unwrap().size,
            5
        );
        assert_eq!(prepared.projections.hybrid.len(), 1);
        actor
            .try_submit(UpdateRequest {
                base,
                changes: vec![],
            })
            .unwrap();
        assert!(matches!(get(&actor), Event::Failed(Failure::Pending)));
        actor.try_commit(prepared.version).unwrap();
        actor
            .try_submit(UpdateRequest {
                base,
                changes: vec![],
            })
            .unwrap();
        assert!(matches!(get(&actor), Event::Failed(Failure::Stale { .. })));
    }
    #[test]
    fn projection_lookup_is_background_identity_checked_and_preserves_slots() {
        let actor = actor();
        let base = Version {
            source_generation: 9,
            scene_version: 0,
        };
        let data = Arc::new(scan::ScanResult {
            root: "/disk".into(),
            files: vec![
                file("/disk/a/f", 10, 3, 2),
                file("/disk/a/g", 5, 99, 2),
                file("/disk/a/f", 999, 3, 2),
                scan::FileEntry {
                    path: "/disk/a".into(),
                    parent: Arc::from(Path::new("/disk")),
                    size: 15,
                    identity: identity(2),
                    parent_identity: Default::default(),
                    parent_is_leaf: false,
                    can_trash: true,
                },
            ],
            directories: Arc::from([]),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        actor
            .try_lookup_projection(ProjectionLookupRequest {
                base,
                data: data.clone(),
                mode: View::Hybrid,
                source_ids: Some(vec![Some(0), Some(1), Some(0), None].into()),
            })
            .unwrap();
        let Event::ProjectionLookup(found) = get(&actor) else {
            panic!("projection")
        };
        assert_eq!(found.base, base);
        assert_eq!(found.mode, View::Hybrid);
        assert!(matches!(found.entries[0], Some(TileId::File(_))));
        assert_eq!(found.entries[1], None);
        assert_eq!(found.entries[2], None); // Same identity, stale size.
        assert!(matches!(found.entries[3], Some(TileId::Directory(_))));
        actor
            .try_lookup_projection(ProjectionLookupRequest {
                base,
                data,
                mode: View::Hybrid,
                source_ids: Some(Arc::from([])),
            })
            .unwrap();
        assert!(matches!(
            get(&actor),
            Event::Failed(Failure::InvalidProjection)
        ));
    }
    #[test]
    fn invalid_batch_keeps_committed_version_and_directory_facts_change_hybrid() {
        let actor = actor();
        let base = Version {
            source_generation: 9,
            scene_version: 0,
        };
        actor
            .try_submit(UpdateRequest {
                base,
                changes: vec![Change::RemoveFile {
                    path: "/disk/a/f".into(),
                    identity: identity(99),
                }],
            })
            .unwrap();
        assert!(matches!(
            get(&actor),
            Event::Failed(Failure::Invalid(ChangeError::IdentityMismatch))
        ));
        actor
            .try_submit(UpdateRequest {
                base,
                changes: vec![Change::UpsertDirectory(scan::DirectoryFact {
                    path: "/disk/a".into(),
                    identity: identity(2),
                    enumeration_complete: true,
                    has_subdirectories: true,
                    excluded: false,
                })],
            })
            .unwrap();
        let Event::Prepared(prepared) = get(&actor) else {
            panic!("prepared")
        };
        assert_eq!(prepared.projections.hybrid.len(), 3);
        assert_eq!(
            prepared
                .projections
                .hybrid
                .iter()
                .filter(|delta| delta.before.is_some() && delta.after.is_none())
                .count(),
            1
        );
        assert_eq!(
            prepared
                .projections
                .hybrid
                .iter()
                .filter(|delta| delta.before.is_none() && delta.after.is_some())
                .count(),
            2
        );
    }
}
