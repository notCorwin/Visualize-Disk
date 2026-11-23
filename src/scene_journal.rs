//! Replay the exact sequence of published scene transactions after a worker
//! failure. The journal is persistent: taking a recovery snapshot is O(1),
//! and records are retained only as long as a published frame can need them.
use crate::{
    scan,
    scene_actor::Version,
    scene_model::{Change, ChangeError, Scene},
};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Keep the original operation order. A rescan diff must not be recomputed
/// during recovery: hash iteration may allocate stable IDs in a new order.
pub enum Record {
    Batch(Arc<[Change]>),
    Ordered(Arc<[Change]>),
    /// Hidden source entries are stored as shared snapshots rather than one
    /// allocated path per deleted file. IDs are applied in source-index order.
    Hidden {
        before: Arc<BTreeSet<usize>>,
        after: Arc<BTreeSet<usize>>,
    },
}

struct Node {
    previous: Option<Arc<Node>>,
    version: Version,
    record: Arc<Record>,
}

// A session can contain many updates. Iteratively release an unshared tail
// so dropping the final snapshot cannot overflow the stack.
impl Drop for Node {
    fn drop(&mut self) {
        let mut previous = self.previous.take();
        while let Some(node) = previous {
            match Arc::try_unwrap(node) {
                Ok(mut node) => previous = node.previous.take(),
                Err(_) => break,
            }
        }
    }
}

#[derive(Clone)]
pub struct Journal {
    source: Arc<scan::ScanResult>,
    initial_hidden: Arc<BTreeSet<usize>>,
    initial: Version,
    checkpoint: Option<Arc<Scene>>,
    latest: Version,
    head: Option<Arc<Node>>,
    records: usize,
    retained_items: usize,
}

#[derive(Debug)]
pub enum Error {
    Cancelled,
    InvalidVersion,
    InvalidHidden,
    InvalidChange(ChangeError),
}

pub struct Restored {
    pub scene: Scene,
    pub version: Version,
}

/// A checkpoint is prepared on the worker and adopted only if the UI still
/// displays exactly this version. It preserves arena slots and generations.
pub struct Checkpoint {
    scene: Arc<Scene>,
    version: Version,
}

impl std::fmt::Debug for Checkpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Checkpoint")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl Checkpoint {
    /// The actor supplies a shallow copy of its exact committed COW scene.
    pub(crate) fn from_scene(scene: Scene, version: Version) -> Self {
        Self {
            scene: Arc::new(scene),
            version,
        }
    }
}

// Bounds routine journal growth. A single incoming batch can exceed this
// soft limit; the caller must pause further submissions until compacted.
const MAX_RECORDS: usize = 64;
const MAX_RETAINED_ITEMS: usize = 65_536;

impl Journal {
    pub fn new(
        source: Arc<scan::ScanResult>,
        source_generation: u64,
        initial_hidden: Arc<BTreeSet<usize>>,
    ) -> Self {
        let initial = Version {
            source_generation,
            scene_version: 0,
        };
        Self {
            source,
            initial_hidden,
            initial,
            checkpoint: None,
            latest: initial,
            head: None,
            records: 0,
            retained_items: 0,
        }
    }

    pub fn version(&self) -> Version {
        self.latest
    }

    pub fn needs_checkpoint(&self) -> bool {
        self.records >= MAX_RECORDS || self.retained_items >= MAX_RETAINED_ITEMS
    }

    /// Prepare off-thread. Failure leaves the published journal untouched.
    pub fn checkpoint(&self, cancelled: &AtomicBool) -> Result<Checkpoint, Error> {
        let restored = self.replay(cancelled)?;
        Ok(Checkpoint {
            scene: Arc::new(restored.scene),
            version: restored.version,
        })
    }

    /// Atomically discard history only if no later frame was published. The
    /// caller must retry if the scene has advanced while the worker ran.
    pub fn adopt(&mut self, checkpoint: Checkpoint) -> Result<(), Error> {
        if checkpoint.version != self.latest {
            return Err(Error::InvalidVersion);
        }
        self.initial = checkpoint.version;
        self.initial_hidden = Arc::new(BTreeSet::new());
        self.checkpoint = Some(checkpoint.scene);
        self.head = None;
        self.records = 0;
        self.retained_items = 0;
        Ok(())
    }

    /// Only append after the UI accepted the corresponding prepared frame.
    /// A large hidden transaction may advance multiple private actor versions.
    pub fn append(
        &mut self,
        base: Version,
        version: Version,
        record: Arc<Record>,
    ) -> Result<(), Error> {
        if base != self.latest
            || version.source_generation != base.source_generation
            || version.scene_version <= base.scene_version
        {
            return Err(Error::InvalidVersion);
        }
        let size = match record.as_ref() {
            Record::Batch(items) | Record::Ordered(items) => items.len(),
            Record::Hidden { before, after } => before.len().saturating_add(after.len()),
        };
        self.head = Some(Arc::new(Node {
            previous: self.head.clone(),
            version,
            record,
        }));
        self.latest = version;
        self.records = self.records.saturating_add(1);
        self.retained_items = self.retained_items.saturating_add(size);
        Ok(())
    }

    /// Must run off the UI thread. No state is published on cancellation or
    /// error; the old rendered frame and this journal remain unchanged.
    pub fn replay(&self, cancelled: &AtomicBool) -> Result<Restored, Error> {
        if cancelled.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let mut scene = if let Some(scene) = &self.checkpoint {
            (**scene).clone()
        } else {
            let mut scene = Scene::from_scan_cancellable(self.source.clone(), cancelled)
                .ok_or(Error::Cancelled)?;
            apply_hidden(
                &mut scene,
                &self.source,
                &BTreeSet::new(),
                &self.initial_hidden,
                cancelled,
            )?;
            scene
        };
        let mut records = Vec::new();
        let mut cursor = self.head.as_ref();
        while let Some(node) = cursor {
            if records.len() % 4096 == 0 && cancelled.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            records.push(node.as_ref());
            cursor = node.previous.as_ref();
        }
        let mut version = self.initial;
        for node in records.into_iter().rev() {
            if cancelled.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            match node.record.as_ref() {
                Record::Batch(changes) => {
                    scene
                        .apply_batch(changes.to_vec())
                        .map_err(Error::InvalidChange)?;
                }
                Record::Ordered(changes) => {
                    for (index, change) in changes.iter().enumerate() {
                        if index % 1024 == 0 && cancelled.load(Ordering::Relaxed) {
                            return Err(Error::Cancelled);
                        }
                        scene.apply(change.clone()).map_err(Error::InvalidChange)?;
                    }
                }
                Record::Hidden { before, after } => {
                    apply_hidden(&mut scene, &self.source, before, after, cancelled)?
                }
            }
            version = node.version;
        }
        if cancelled.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        Ok(Restored { scene, version })
    }
}

fn apply_hidden(
    scene: &mut Scene,
    source: &scan::ScanResult,
    before: &BTreeSet<usize>,
    after: &BTreeSet<usize>,
    cancelled: &AtomicBool,
) -> Result<(), Error> {
    if !before.is_subset(after) || after.last().is_some_and(|id| *id >= source.files.len()) {
        return Err(Error::InvalidHidden);
    }
    let mut changes = Vec::with_capacity(4096);
    for id in after.difference(before) {
        if changes.is_empty() && cancelled.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let file = &source.files[*id];
        changes.push(Change::RemoveFile {
            path: file.path.clone(),
            identity: file.identity,
        });
        if changes.len() == 4096 {
            scene
                .apply_batch(std::mem::take(&mut changes))
                .map_err(Error::InvalidChange)?;
            changes = Vec::with_capacity(4096);
        }
    }
    if !changes.is_empty() {
        scene.apply_batch(changes).map_err(Error::InvalidChange)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, time::Duration};

    fn identity(inode: u64) -> scan::FileIdentity {
        scan::FileIdentity {
            device: 1,
            inode: inode as _,
        }
    }
    fn file(path: &str, inode: u64) -> scan::FileEntry {
        let path = std::path::PathBuf::from(path);
        scan::FileEntry {
            parent: Arc::from(path.parent().unwrap()),
            path,
            size: 3,
            identity: identity(inode),
            parent_identity: identity(1),
            parent_is_leaf: true,
            can_trash: true,
        }
    }
    fn source() -> Arc<scan::ScanResult> {
        Arc::new(scan::ScanResult {
            root: "/r".into(),
            files: vec![file("/r/a", 2), file("/r/b", 3)],
            directories: vec![scan::DirectoryFact {
                path: "/r".into(),
                identity: identity(1),
                enumeration_complete: true,
                has_subdirectories: false,
                excluded: false,
            }]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        })
    }
    fn v(n: u64) -> Version {
        Version {
            source_generation: 7,
            scene_version: n,
        }
    }

    #[test]
    fn replays_published_changes_with_identical_stable_ids() {
        let source = source();
        let mut expected = Scene::from_scan(source.clone());
        let mut journal = Journal::new(source, 7, Arc::new(BTreeSet::new()));
        let before = Arc::new(BTreeSet::new());
        let after = Arc::new(BTreeSet::from([0]));
        expected
            .apply_batch(vec![Change::RemoveFile {
                path: "/r/a".into(),
                identity: identity(2),
            }])
            .unwrap();
        journal
            .append(v(0), v(1), Arc::new(Record::Hidden { before, after }))
            .unwrap();
        let changes: Arc<[Change]> = vec![Change::UpsertFile(file("/r/c", 4))].into();
        expected.apply_batch(changes.to_vec()).unwrap();
        journal
            .append(v(1), v(2), Arc::new(Record::Batch(changes)))
            .unwrap();
        let recovered = journal.replay(&AtomicBool::new(false)).unwrap();
        assert_eq!(recovered.version, v(2));
        for path in ["/r/a", "/r/b", "/r/c"] {
            assert_eq!(
                recovered.scene.file_id(Path::new(path)),
                expected.file_id(Path::new(path))
            );
        }
    }

    #[test]
    fn snapshots_remain_unchanged_and_reject_invalid_versions() {
        let mut journal = Journal::new(source(), 7, Arc::new(BTreeSet::new()));
        let previous = journal.clone();
        assert!(matches!(
            journal.append(v(1), v(2), Arc::new(Record::Batch(Arc::new([])))),
            Err(Error::InvalidVersion)
        ));
        journal
            .append(
                v(0),
                v(1),
                Arc::new(Record::Batch(
                    vec![Change::UpsertFile(file("/r/c", 4))].into(),
                )),
            )
            .unwrap();
        assert_eq!(previous.version(), v(0));
        assert!(
            previous
                .replay(&AtomicBool::new(false))
                .unwrap()
                .scene
                .file_id(Path::new("/r/c"))
                .is_none()
        );
        assert!(matches!(
            journal.replay(&AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn checkpoint_preserves_ids_and_rejects_stale_adoption() {
        let mut journal = Journal::new(source(), 7, Arc::new(BTreeSet::new()));
        journal
            .append(
                v(0),
                v(1),
                Arc::new(Record::Batch(
                    vec![Change::UpsertFile(file("/r/c", 4))].into(),
                )),
            )
            .unwrap();
        let old = journal.clone();
        let old_checkpoint = old.checkpoint(&AtomicBool::new(false)).unwrap();
        journal
            .append(
                v(1),
                v(2),
                Arc::new(Record::Batch(
                    vec![Change::RemoveFile {
                        path: "/r/a".into(),
                        identity: identity(2),
                    }]
                    .into(),
                )),
            )
            .unwrap();
        assert!(matches!(
            journal.adopt(old_checkpoint),
            Err(Error::InvalidVersion)
        ));
        let expected = journal.replay(&AtomicBool::new(false)).unwrap();
        let checkpoint = journal.checkpoint(&AtomicBool::new(false)).unwrap();
        journal.adopt(checkpoint).unwrap();
        assert_eq!(journal.version(), v(2));
        let restored = journal.replay(&AtomicBool::new(false)).unwrap();
        for path in ["/r/a", "/r/b", "/r/c"] {
            assert_eq!(
                restored.scene.file_id(Path::new(path)),
                expected.scene.file_id(Path::new(path))
            );
        }
        // Subsequent ID reuse still follows the checkpoint's original arenas.
        journal
            .append(
                v(2),
                v(3),
                Arc::new(Record::Batch(
                    vec![Change::UpsertFile(file("/r/d", 5))].into(),
                )),
            )
            .unwrap();
        let new_scene = journal.replay(&AtomicBool::new(false)).unwrap().scene;
        assert_ne!(
            new_scene.file_id(Path::new("/r/d")),
            old.replay(&AtomicBool::new(false))
                .unwrap()
                .scene
                .file_id(Path::new("/r/a"))
        );
    }

    #[test]
    fn independently_replayed_subtree_removal_preserves_reused_directory_ids() {
        let fact = |path: &str, inode, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(inode),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let data = Arc::new(scan::ScanResult {
            root: "/r".into(),
            files: vec![],
            directories: vec![
                fact("/r", 1, true),
                fact("/r/a", 2, true),
                fact("/r/a/x", 3, false),
                fact("/r/a/y", 4, false),
                fact("/r/a/z", 5, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let mut expected = Scene::from_scan(data.clone());
        let mut journal = Journal::new(data, 7, Arc::new(BTreeSet::new()));
        let remove: Arc<[Change]> = vec![Change::RemoveSubtreeDirectory {
            path: "/r/a".into(),
            identity: identity(2),
            parent: fact("/r", 1, false),
        }]
        .into();
        expected.apply_batch(remove.to_vec()).unwrap();
        journal
            .append(v(0), v(1), Arc::new(Record::Batch(remove)))
            .unwrap();
        let create: Arc<[Change]> = vec![
            Change::UpsertDirectory(fact("/r/n1", 6, false)),
            Change::UpsertDirectory(fact("/r/n2", 7, false)),
            Change::UpsertDirectory(fact("/r/n3", 8, false)),
        ]
        .into();
        expected.apply_batch(create.to_vec()).unwrap();
        journal
            .append(v(1), v(2), Arc::new(Record::Batch(create)))
            .unwrap();
        for _ in 0..20 {
            let recovered = journal.replay(&AtomicBool::new(false)).unwrap();
            for path in ["/r/n1", "/r/n2", "/r/n3"] {
                assert_eq!(
                    recovered.scene.directory_id(Path::new(path)),
                    expected.directory_id(Path::new(path)),
                    "{path}"
                );
            }
        }
    }

    #[test]
    fn ordered_replacement_replays_with_identical_slot_generations() {
        let fact = |path: &str, inode, children| scan::DirectoryFact {
            path: path.into(),
            identity: identity(inode),
            enumeration_complete: true,
            has_subdirectories: children,
            excluded: false,
        };
        let data = Arc::new(scan::ScanResult {
            root: "/r".into(),
            files: vec![],
            directories: vec![
                fact("/r", 1, true),
                fact("/r/a", 2, true),
                fact("/r/a/x", 3, false),
            ]
            .into(),
            skipped: 0,
            errors: vec![],
            elapsed: Duration::ZERO,
        });
        let mut expected = Scene::from_scan(data.clone());
        let mut journal = Journal::new(data, 7, Arc::new(BTreeSet::new()));
        let changes: Arc<[Change]> = vec![
            Change::RemoveSubtreeDirectory {
                path: "/r/a".into(),
                identity: identity(2),
                parent: fact("/r", 1, false),
            },
            Change::UpsertDirectory(fact("/r/a", 4, true)),
            Change::UpsertDirectory(fact("/r/a/y", 5, false)),
        ]
        .into();
        for change in changes.iter().cloned() {
            expected.apply(change).unwrap();
        }
        journal
            .append(v(0), v(1), Arc::new(Record::Ordered(changes)))
            .unwrap();
        let restored = journal.replay(&AtomicBool::new(false)).unwrap();
        for path in ["/r/a", "/r/a/y"] {
            assert_eq!(
                restored.scene.directory_id(Path::new(path)),
                expected.directory_id(Path::new(path)),
                "{path}"
            );
        }
    }
}
