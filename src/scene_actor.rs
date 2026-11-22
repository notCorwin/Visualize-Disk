//! Single-writer scene actor. UI commands never wait for directory work and a
//! prepared transaction must be acknowledged before another can be applied.
use crate::scan::{self, FileIdentity};
use crate::scene_model::{Change, ChangeError, Scene, Tile, TileId, View};
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
#[derive(Debug)]
pub enum Failure {
    Stale { current: Version },
    Pending,
    Invalid(ChangeError),
    InvalidCommit,
    InvalidProjection,
}
#[derive(Debug)]
pub enum Event {
    Prepared(Prepared),
    ProjectionLookup(ProjectionLookupResult),
    Failed(Failure),
}

enum Command {
    Update(UpdateRequest),
    ProjectionLookup(ProjectionLookupRequest),
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
        let (tx, commands) = mpsc::sync_channel(4);
        let (events, rx) = mpsc::channel();
        let worker_cancelled = Arc::clone(&cancelled);
        thread::Builder::new()
            .name("scene-updates".into())
            .spawn(move || {
                let mut scene = scene;
                let mut committed = Version {
                    source_generation,
                    scene_version: 0,
                };
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
                paths.insert(fact.path.clone());
                if let Some(parent) = fact.path.parent() {
                    paths.insert(parent.to_path_buf());
                }
            }
            Change::RemoveDirectory { path, .. } => {
                paths.insert(path.clone());
                if let Some(parent) = path.parent() {
                    paths.insert(parent.to_path_buf());
                }
            }
            Change::RemoveLeafDirectory { path, parent, .. } => {
                paths.insert(path.clone());
                paths.insert(parent.path.clone());
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
fn leaf_transitions(scene: &Scene, changes: &[Change]) -> BTreeSet<PathBuf> {
    let mut changed = BTreeSet::new();
    for change in changes {
        let (path, new_leaf) = match change {
            Change::UpsertDirectory(fact) => (
                &fact.path,
                Some(fact.enumeration_complete && !fact.has_subdirectories),
            ),
            Change::RemoveDirectory { path, .. } => (path, None),
            Change::RemoveLeafDirectory { path, .. } => (path, None),
            _ => continue,
        };
        if scene.directory_leaf(path) != new_leaf {
            changed.insert(path.clone());
        }
    }
    // A leaf removal may turn its parent into a hybrid aggregate. Its files
    // must be captured before and after even when no file path changed.
    for change in changes {
        if let Change::RemoveLeafDirectory { parent, .. } = change {
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
    let paths = affected(scene, &changes);
    let files_changed = file_paths(&changes);
    let transitions = leaf_transitions(scene, &changes);
    let files = capture(scene, &paths, &files_changed, &transitions, View::Files);
    let folders = capture(scene, &paths, &files_changed, &transitions, View::Folders);
    let hybrid = capture(scene, &paths, &files_changed, &transitions, View::Hybrid);
    let impact = scene.apply_batch(changes)?;
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
    Ok(Prepared {
        base,
        version,
        projections,
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
    fn actor() -> SceneActor {
        let fact = |path: &str, id: u64, has_subdirectories: bool| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories,
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
    fn removing_leaf_captures_subtree_and_parent_hybrid_transition() {
        let fact = |path: &str, id: u64, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(id),
            enumeration_complete: true,
            has_subdirectories: children,
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
