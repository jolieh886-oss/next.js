use std::{
    hash::Hash,
    iter::Peekable,
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use dashmap::Entry;
use smallvec::SmallVec;
use turbo_bincode::TurboBincodeBuffer;
use turbo_tasks::{FxDashMap, TaskId, parallel};

use crate::{
    backend::storage_schema::TaskStorage,
    backing_storage::SnapshotItem,
    database::key_value_database::KeySpace,
    utils::{
        dash_map_drop_contents::drop_contents,
        dash_map_multi::{RefMut, get_multiple_mut},
        sharded::Sharded,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskDataCategory {
    Meta,
    Data,
    All,
}

impl TaskDataCategory {
    pub fn into_specific(self) -> SpecificTaskDataCategory {
        match self {
            TaskDataCategory::Meta => SpecificTaskDataCategory::Meta,
            TaskDataCategory::Data => SpecificTaskDataCategory::Data,
            TaskDataCategory::All => unreachable!(),
        }
    }

    pub fn includes_data(self) -> bool {
        matches!(self, TaskDataCategory::Data | TaskDataCategory::All)
    }

    pub fn includes_meta(self) -> bool {
        matches!(self, TaskDataCategory::Meta | TaskDataCategory::All)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SpecificTaskDataCategory {
    Meta,
    Data,
}

impl SpecificTaskDataCategory {
    /// Returns the KeySpace for storing data of this category
    pub fn key_space(self) -> KeySpace {
        match self {
            SpecificTaskDataCategory::Meta => KeySpace::TaskMeta,
            SpecificTaskDataCategory::Data => KeySpace::TaskData,
        }
    }
}

/// Number of shards for the snapshots map. This is intentionally small since:
/// 1. Snapshots are rare (only during dev mode idle-callback persistence races)
/// 2. We're otherwise saturating the CPU doing persistence, so lock contention here doesn't matter.
/// 3. We want to minimize fixed overhead from sharding
const SNAPSHOT_SHARDS: usize = 16;

pub struct Storage {
    snapshot_mode: AtomicBool,
    /// Tracks TaskIds that have been modified since the last snapshot.
    /// Uses a sharded Vec for efficient append-only writes.
    /// Writes are guarded by the `any_modified` flag on TaskStorage, ensuring single-write
    /// semantics, thus we don't need to worry about duplicates.
    modified: Sharded<Vec<TaskId>>,
    /// Stores snapshots of task state for tasks accessed during snapshot mode.
    /// - `Some(snapshot)`: Task was modified before snapshot mode and accessed again during it.
    ///   Contains a copy of the pre-snapshot state that needs to be persisted.
    /// - `None`: Task was first modified during snapshot mode (not part of current snapshot). Will
    ///   be added to modified list for the next snapshot cycle.
    snapshots: FxDashMap<TaskId, Option<Box<TaskStorage>>>,
    map: FxDashMap<TaskId, Box<TaskStorage>>,
}

impl Storage {
    pub fn new(shard_amount: usize, small_preallocation: bool) -> Self {
        let map_capacity: usize = if small_preallocation {
            1024
        } else {
            1024 * 1024
        };

        Self {
            snapshot_mode: AtomicBool::new(false),
            // TODO: these two datastructures should only exist/be modified if we are going to
            // persist This should probably be sharded by a smaller amount since it sees
            // far fewer mutations
            modified: Sharded::new(shard_amount),
            snapshots: FxDashMap::with_capacity_and_hasher_and_shard_amount(
                0, // Start empty, rarely used
                Default::default(),
                SNAPSHOT_SHARDS,
            ),
            map: FxDashMap::with_capacity_and_hasher_and_shard_amount(
                map_capacity,
                Default::default(),
                shard_amount,
            ),
        }
    }

    /// Processes every modified item (resp. a snapshot of it) with the given functions and returns
    /// the results. Ends snapshot mode afterwards.
    /// process is called while holding a read lock on the task storage, so it can access
    /// the TaskStorage directly without cloning.
    /// process_snapshot is called for tasks that were accessed during snapshot mode and
    /// receives an owned Box<TaskStorage> snapshot.
    /// Both callbacks receive a mutable scratch buffer that can be reused across iterations
    /// to avoid repeated allocations.
    /// The returned iterators are guaranteed to be non-empty and only yield non-empty items.
    pub fn take_snapshot<
        'l,
        P: for<'a> Fn(TaskId, &'a TaskStorage, &mut TurboBincodeBuffer) -> SnapshotItem + Sync,
        PS: Fn(TaskId, Box<TaskStorage>, &mut TurboBincodeBuffer) -> SnapshotItem + Sync,
    >(
        &'l self,
        process: &'l P,
        process_snapshot: &'l PS,
    ) -> Vec<Peekable<SnapshotShard<'l, P, PS>>> {
        if !self.snapshot_mode() {
            self.start_snapshot();
        }
        let modified_tasks = self.modified.take(|modified| modified);

        let guard = Arc::new(SnapshotGuard {
            storage: self,
            modified_tasks,
        });

        // The number of shards is much larger than the number of threads, so the effect of the
        // locks held is negligible.
        parallel::map_collect::<_, _, Vec<_>>(&guard.modified_tasks, |shard| {
            if shard.is_empty() {
                return None;
            }
            let mut direct_snapshots: Vec<(TaskId, Box<TaskStorage>)> = Vec::new();
            let mut modified: SmallVec<[TaskId; 4]> = SmallVec::with_capacity(shard.len());
            for id in shard {
                match self.snapshots.entry(*id) {
                    Entry::Occupied(occupied) if occupied.get().is_some() => {
                        direct_snapshots.push((*id, occupied.remove().unwrap()));
                    }
                    _ => {
                        modified.push(*id);
                    }
                }
            }

            /// How big of a buffer to allocate initially.  Based on metrics from a large
            /// application this should cover about 98% of values with no resizes
            const SCRATCH_BUFFER_SIZE: usize = 4096;
            let shard = SnapshotShard {
                direct_snapshots,
                modified,
                storage: self,
                guard: Some(guard.clone()),
                process,
                process_snapshot,
                scratch_buffer: TurboBincodeBuffer::with_capacity(SCRATCH_BUFFER_SIZE),
            };

            // Peek to filter out shards that only produce empty items
            let mut iter = shard.peekable();
            iter.peek().is_some().then_some(iter)
        })
        .into_iter()
        .flatten()
        .collect()
    }

    /// Start snapshot mode.
    pub fn start_snapshot(&self) {
        self.snapshot_mode.store(true, Ordering::Release);
    }

    /// End snapshot mode.
    /// Items that have snapshots will be kept as modified since they have been accessed during the
    /// snapshot mode. Items that are modified will be removed and considered as unmodified.
    /// When items are accessed in future they will be marked as modified.
    fn end_snapshot(&self, modified_tasks: Vec<Vec<TaskId>>) {
        let modified_count: usize = modified_tasks.iter().map(|s| s.len()).sum();
        let span = tracing::info_span!(
            "end_snapshot",
            modified_count,
            snapshot_count = tracing::field::Empty,
        );
        let _guard = span.enter();

        // Phase 1: Clear modified/new flags on all tasks that were in the original modified list.
        // This must happen WHILE STILL IN snapshot mode so that any concurrent modifications
        // will go to the `snapshots` map (since they see snapshot_mode=true and modified=false).
        for task_ids in &modified_tasks {
            for &task_id in task_ids {
                if let Some(mut inner) = self.map.get_mut(&task_id) {
                    inner.flags.set_data_modified(false);
                    inner.flags.set_meta_modified(false);
                    inner.flags.set_new_persistent_task(false);
                }
            }
        }

        // Phase 2: Leave snapshot mode - modifications now go to modified list
        self.snapshot_mode.store(false, Ordering::Release);

        // Record snapshot count now that no new entries can be added
        span.record("snapshot_count", self.snapshots.len());

        // Phase 3: Handle tasks that had snapshots (they were accessed during snapshot mode).
        // These need to be re-added to modified for the next cycle.
        // Now that we're Inactive, concurrent track_modification will go to the modified list
        // directly, so there's no conflict with us reading+clearing the snapshots map here.
        for entry in self.snapshots.iter() {
            let key = *entry.key();
            if let Some(mut inner) = self.map.get_mut(&key) {
                // Convert snapshot flags to modified flags
                if inner.flags.meta_snapshot() {
                    inner.flags.set_meta_snapshot(false);
                    inner.flags.set_meta_modified(true);
                }
                if inner.flags.data_snapshot() {
                    inner.flags.set_data_snapshot(false);
                    inner.flags.set_data_modified(true);
                }
                // Re-add to modified list since they were accessed during snapshot
                self.modified.lock(key).push(key);
            }
        }
        self.snapshots.clear();
        self.snapshots.shrink_to_fit();
    }

    /// Returns true if actively snapshotting (modifications should go to snapshots map).
    /// Returns false if inactive (modifications go to modified list).
    fn snapshot_mode(&self) -> bool {
        self.snapshot_mode.load(Ordering::Acquire)
    }

    pub fn access_mut(&self, key: TaskId) -> StorageWriteGuard<'_> {
        let inner = match self.map.entry(key) {
            dashmap::mapref::entry::Entry::Occupied(e) => e.into_ref(),
            dashmap::mapref::entry::Entry::Vacant(e) => e.insert(Box::new(TaskStorage::new())),
        };
        StorageWriteGuard {
            storage: self,
            inner: inner.into(),
        }
    }

    pub fn access_pair_mut(
        &self,
        key1: TaskId,
        key2: TaskId,
    ) -> (StorageWriteGuard<'_>, StorageWriteGuard<'_>) {
        let (a, b) = get_multiple_mut(&self.map, key1, key2, || Box::new(TaskStorage::new()));
        (
            StorageWriteGuard {
                storage: self,
                inner: a,
            },
            StorageWriteGuard {
                storage: self,
                inner: b,
            },
        )
    }

    pub fn drop_contents(&self) {
        drop_contents(&self.map);
        // Clear the modified list
        self.modified.take(|_| ());
        self.snapshots.clear();
    }
}

pub struct StorageWriteGuard<'a> {
    storage: &'a Storage,
    inner: RefMut<'a, TaskId, Box<TaskStorage>>,
}

impl StorageWriteGuard<'_> {
    /// Tracks mutation of this task
    #[inline(always)]
    pub fn track_modification(
        &mut self,
        category: SpecificTaskDataCategory,
        #[allow(unused_variables)] name: &str,
    ) {
        self.track_modification_internal(
            category,
            #[cfg(feature = "trace_task_modification")]
            name,
        );
    }

    fn track_modification_internal(
        &mut self,
        category: SpecificTaskDataCategory,
        #[cfg(feature = "trace_task_modification")] name: &str,
    ) {
        let flags = &self.inner.flags;
        if flags.is_snapshot(category) {
            return;
        }
        let modified = flags.is_modified(category);
        #[cfg(feature = "trace_task_modification")]
        let _span = (!modified).then(|| tracing::trace_span!("mark_modified", name).entered());
        match (self.storage.snapshot_mode(), modified) {
            (false, false) => {
                // Not in snapshot mode and item is unmodified
                if !flags.any_snapshot() && !flags.any_modified() {
                    // First modification - add to modified list
                    let key = *self.inner.key();
                    self.storage.modified.lock(key).push(key);
                }
                self.inner.flags.set_modified(category, true);
            }
            (false, true) => {
                // Not in snapshot mode and item is already modified
                // Do nothing
            }
            (true, false) => {
                // In snapshot mode and item is unmodified (so it's not part of the snapshot)
                if !flags.any_snapshot() {
                    self.storage.snapshots.insert(*self.inner.key(), None);
                }
                self.inner.flags.set_snapshot(category, true);
            }
            (true, true) => {
                // In snapshot mode and item is modified (so it's part of the snapshot)
                // We need to store the original version that is part of the snapshot
                if !flags.any_snapshot() {
                    // Snapshot all non-transient fields but keep the modified bits.
                    let mut snapshot = self.inner.clone_snapshot();
                    snapshot.flags.set_data_modified(flags.data_modified());
                    snapshot.flags.set_meta_modified(flags.meta_modified());
                    self.storage
                        .snapshots
                        .insert(*self.inner.key(), Some(Box::new(snapshot)));
                }
                self.inner.flags.set_snapshot(category, true);
            }
        }
    }
}

impl Deref for StorageWriteGuard<'_> {
    type Target = TaskStorage;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for StorageWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

pub struct SnapshotGuard<'l> {
    storage: &'l Storage,
    /// Original modified task IDs from when the snapshot began.
    /// Their modified/new flags will be cleared when the snapshot ends.
    modified_tasks: Vec<Vec<TaskId>>,
}

impl Drop for SnapshotGuard<'_> {
    fn drop(&mut self) {
        self.storage
            .end_snapshot(std::mem::take(&mut self.modified_tasks));
    }
}

pub struct SnapshotShard<'l, P, PS> {
    direct_snapshots: Vec<(TaskId, Box<TaskStorage>)>,
    modified: SmallVec<[TaskId; 4]>,
    storage: &'l Storage,
    guard: Option<Arc<SnapshotGuard<'l>>>,
    process: &'l P,
    process_snapshot: &'l PS,
    /// Scratch buffer for encoding task data, reused across iterations to avoid allocations
    scratch_buffer: TurboBincodeBuffer,
}

impl<'l, P, PS> SnapshotShard<'l, P, PS>
where
    P: Fn(TaskId, &TaskStorage, &mut TurboBincodeBuffer) -> SnapshotItem + Sync,
    PS: Fn(TaskId, Box<TaskStorage>, &mut TurboBincodeBuffer) -> SnapshotItem + Sync,
{
    fn next_item(&mut self) -> Option<SnapshotItem> {
        if let Some((task_id, snapshot)) = self.direct_snapshots.pop() {
            return Some((self.process_snapshot)(
                task_id,
                snapshot,
                &mut self.scratch_buffer,
            ));
        }
        while let Some(task_id) = self.modified.pop() {
            let inner = self.storage.map.get(&task_id).unwrap();
            if !inner.flags.any_snapshot() {
                return Some((self.process)(task_id, &inner, &mut self.scratch_buffer));
            } else {
                drop(inner);

                // Check if this task has a snapshot stored (it was accessed during snapshot mode)
                // Use get_mut + take instead of remove so the entry stays in the map.
                // end_snapshot needs to see these entries to re-add them to modified.
                if let Some(snapshot) = self
                    .storage
                    .snapshots
                    .get_mut(&task_id)
                    .and_then(|mut e| e.take())
                {
                    return Some((self.process_snapshot)(
                        task_id,
                        snapshot,
                        &mut self.scratch_buffer,
                    ));
                } else {
                    unreachable!("modified tasks that are also snapshot must have stored a clone.");
                }
            }
        }
        None
    }
}

impl<'l, P, PS> Iterator for SnapshotShard<'l, P, PS>
where
    P: Fn(TaskId, &TaskStorage, &mut TurboBincodeBuffer) -> SnapshotItem + Sync,
    PS: Fn(TaskId, Box<TaskStorage>, &mut TurboBincodeBuffer) -> SnapshotItem + Sync,
{
    type Item = SnapshotItem;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(item) = self.next_item() {
            if !item.is_empty() {
                return Some(item);
            }
        }
        self.guard = None;
        None
    }
}
