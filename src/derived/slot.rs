use crate::debug::TableEntry;
use crate::dependency::DatabaseSlot;
use crate::dependency::Dependency;
use crate::derived::MemoizationPolicy;
use crate::durability::Durability;
use crate::lru::LruIndex;
use crate::lru::LruNode;
use crate::plumbing::CycleDetected;
use crate::plumbing::GetQueryTable;
use crate::plumbing::HasQueryGroup;
use crate::plumbing::QueryFunction;
use crate::revision::Revision;
use crate::runtime::FxIndexSet;
use crate::runtime::Runtime;
use crate::runtime::RuntimeId;
use crate::runtime::StampedValue;
use crate::{Database, DiscardIf, DiscardWhat, Event, EventKind, SweepStrategy};
use log::{debug, info};
use parking_lot::Mutex;
use parking_lot::RwLock;
use smallvec::SmallVec;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

pub(super) struct Slot<DB, Q, MP>
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
    MP: MemoizationPolicy<DB, Q>,
{
    key: Q::Key,
    state: RwLock<QueryState<DB, Q>>,
    policy: PhantomData<MP>,
    lru_index: LruIndex,
}

/// Defines the "current state" of query's memoized results.
enum QueryState<DB, Q>
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
{
    NotComputed,

    /// The runtime with the given id is currently computing the
    /// result of this query; if we see this value in the table, it
    /// indeeds a cycle.
    InProgress {
        id: RuntimeId,
        waiting: Mutex<SmallVec<[Sender<StampedValue<Q::Value>>; 2]>>,
    },

    /// We have computed the query already, and here is the result.
    Memoized(Memo<DB, Q>),
}

struct Memo<DB, Q>
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
{
    /// The result of the query, if we decide to memoize it.
    value: Option<Q::Value>,

    /// Last revision when this memo was verified (if there are
    /// untracked inputs, this will also be when the memo was
    /// created).
    verified_at: Revision,

    /// Last revision when the memoized value was observed to change.
    changed_at: Revision,

    /// Minimum durability of the inputs to this query.
    durability: Durability,

    /// The inputs that went into our query, if we are tracking them.
    inputs: MemoInputs<DB>,
}

/// An insertion-order-preserving set of queries. Used to track the
/// inputs accessed during query execution.
pub(super) enum MemoInputs<DB: Database> {
    /// Non-empty set of inputs, fully known
    Tracked {
        inputs: Arc<FxIndexSet<Dependency<DB>>>,
    },

    /// Empty set of inputs, fully known.
    NoInputs,

    /// Unknown quantity of inputs
    Untracked,
}

/// Return value of `probe` helper.
enum ProbeState<V, G> {
    UpToDate(Result<V, CycleDetected>),
    StaleOrAbsent(G),
}

impl<DB, Q, MP> Slot<DB, Q, MP>
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
    MP: MemoizationPolicy<DB, Q>,
{
    pub(super) fn new(key: Q::Key) -> Self {
        Self {
            key,
            state: RwLock::new(QueryState::NotComputed),
            lru_index: LruIndex::default(),
            policy: PhantomData,
        }
    }

    pub(super) fn database_key(&self, db: &DB) -> DB::DatabaseKey {
        <DB as GetQueryTable<Q>>::database_key(db, self.key.clone())
    }

    pub(super) fn read(&self, db: &DB) -> Result<StampedValue<Q::Value>, CycleDetected> {
        let runtime = db.salsa_runtime();

        // NB: We don't need to worry about people modifying the
        // revision out from under our feet. Either `db` is a frozen
        // database, in which case there is a lock, or the mutator
        // thread is the current thread, and it will be prevented from
        // doing any `set` invocations while the query function runs.
        let revision_now = runtime.current_revision();

        info!("{:?}: invoked at {:?}", self, revision_now,);

        // First, do a check with a read-lock.
        match self.probe(db, self.state.read(), runtime, revision_now) {
            ProbeState::UpToDate(v) => return v,
            ProbeState::StaleOrAbsent(_guard) => (),
        }

        self.read_upgrade(db, revision_now)
    }

    /// Second phase of a read operation: acquires an upgradable-read
    /// and -- if needed -- validates whether inputs have changed,
    /// recomputes value, etc. This is invoked after our initial probe
    /// shows a potentially out of date value.
    fn read_upgrade(
        &self,
        db: &DB,
        revision_now: Revision,
    ) -> Result<StampedValue<Q::Value>, CycleDetected> {
        let runtime = db.salsa_runtime();

        debug!("{:?}: read_upgrade(revision_now={:?})", self, revision_now,);

        // Check with an upgradable read to see if there is a value
        // already. (This permits other readers but prevents anyone
        // else from running `read_upgrade` at the same time.)
        //
        // FIXME(Amanieu/parking_lot#101) -- we are using a write-lock
        // and not an upgradable read here because upgradable reads
        // can sometimes encounter deadlocks.
        let old_memo = match self.probe(db, self.state.write(), runtime, revision_now) {
            ProbeState::UpToDate(v) => return v,
            ProbeState::StaleOrAbsent(mut state) => {
                match std::mem::replace(&mut *state, QueryState::in_progress(runtime.id())) {
                    QueryState::Memoized(old_memo) => Some(old_memo),
                    QueryState::InProgress { .. } => unreachable!(),
                    QueryState::NotComputed => None,
                }
            }
        };

        let database_key = self.database_key(db);
        let mut panic_guard = PanicGuard::new(&database_key, self, old_memo, runtime);

        // If we have an old-value, it *may* now be stale, since there
        // has been a new revision since the last time we checked. So,
        // first things first, let's walk over each of our previous
        // inputs and check whether they are out of date.
        if let Some(memo) = &mut panic_guard.memo {
            if let Some(value) = memo.validate_memoized_value(db, revision_now) {
                info!("{:?}: validated old memoized value", self,);

                db.salsa_event(|| Event {
                    runtime_id: runtime.id(),
                    kind: EventKind::DidValidateMemoizedValue {
                        database_key: database_key.clone(),
                    },
                });

                panic_guard.proceed(&value);

                return Ok(value);
            }
        }

        // Query was not previously executed, or value is potentially
        // stale, or value is absent. Let's execute!
        let mut result = runtime.execute_query_implementation(db, &database_key, || {
            info!("{:?}: executing query", self);

            Q::execute(db, self.key.clone())
        });

        // We assume that query is side-effect free -- that is, does
        // not mutate the "inputs" to the query system. Sanity check
        // that assumption here, at least to the best of our ability.
        assert_eq!(
            runtime.current_revision(),
            revision_now,
            "revision altered during query execution",
        );

        // If the new value is equal to the old one, then it didn't
        // really change, even if some of its inputs have. So we can
        // "backdate" its `changed_at` revision to be the same as the
        // old value.
        if let Some(old_memo) = &panic_guard.memo {
            if let Some(old_value) = &old_memo.value {
                // Careful: if the value became less durable than it
                // used to be, that is a "breaking change" that our
                // consumers must be aware of. Becoming *more* durable
                // is not. See the test `constant_to_non_constant`.
                if result.durability >= old_memo.durability
                    && MP::memoized_value_eq(&old_value, &result.value)
                {
                    debug!(
                        "read_upgrade({:?}): value is equal, back-dating to {:?}",
                        self, old_memo.changed_at,
                    );

                    assert!(old_memo.changed_at <= result.changed_at);
                    result.changed_at = old_memo.changed_at;
                }
            }
        }

        let new_value = StampedValue {
            value: result.value,
            durability: result.durability,
            changed_at: result.changed_at,
        };

        let value = if self.should_memoize_value(&self.key) {
            Some(new_value.value.clone())
        } else {
            None
        };

        debug!(
            "read_upgrade({:?}): result.changed_at={:?}, \
             result.durability={:?}, result.dependencies = {:#?}",
            self, result.changed_at, result.durability, result.dependencies,
        );

        let inputs = match result.dependencies {
            None => MemoInputs::Untracked,

            Some(dependencies) => {
                if dependencies.is_empty() {
                    MemoInputs::NoInputs
                } else {
                    MemoInputs::Tracked {
                        inputs: Arc::new(dependencies),
                    }
                }
            }
        };
        debug!("read_upgrade({:?}): inputs={:?}", self, inputs);

        panic_guard.memo = Some(Memo {
            value,
            changed_at: result.changed_at,
            verified_at: revision_now,
            inputs,
            durability: result.durability,
        });

        panic_guard.proceed(&new_value);

        Ok(new_value)
    }

    /// Helper for `read`:
    ///
    /// Invoked with the guard `map` of some lock on `self.map` (read
    /// or write) as well as details about the key to look up.  Looks
    /// in the map to see if we have an up-to-date value or a
    /// cycle. Returns a suitable `ProbeState`:
    ///
    /// - `ProbeState::UpToDate(r)` if the table has an up-to-date
    ///   value (or we blocked on another thread that produced such a value).
    /// - `ProbeState::CycleDetected` if this thread is (directly or
    ///   indirectly) already computing this value.
    /// - `ProbeState::BlockedOnOtherThread` if some other thread
    ///   (which does not depend on us) was already computing this
    ///   value; caller should re-acquire the lock and try again.
    /// - `ProbeState::StaleOrAbsent` if either (a) there is no memo
    ///    for this key, (b) the memo has no value; or (c) the memo
    ///    has not been verified at the current revision.
    ///
    /// Note that in all cases **except** for `StaleOrAbsent`, the lock on
    /// `map` will have been released.
    fn probe<StateGuard>(
        &self,
        db: &DB,
        state: StateGuard,
        runtime: &Runtime<DB>,
        revision_now: Revision,
    ) -> ProbeState<StampedValue<Q::Value>, StateGuard>
    where
        StateGuard: Deref<Target = QueryState<DB, Q>>,
    {
        match &*state {
            QueryState::NotComputed => { /* fall through */ }

            QueryState::InProgress { id, waiting } => {
                let other_id = *id;
                return match self.register_with_in_progress_thread(db, runtime, other_id, waiting) {
                    Ok(rx) => {
                        // Release our lock on `self.map`, so other thread
                        // can complete.
                        std::mem::drop(state);

                        db.salsa_event(|| Event {
                            runtime_id: db.salsa_runtime().id(),
                            kind: EventKind::WillBlockOn {
                                other_runtime_id: other_id,
                                database_key: self.database_key(db),
                            },
                        });

                        let value = rx.recv().unwrap_or_else(|_| db.on_propagated_panic());
                        ProbeState::UpToDate(Ok(value))
                    }

                    Err(CycleDetected) => ProbeState::UpToDate(Err(CycleDetected)),
                };
            }

            QueryState::Memoized(memo) => {
                debug!(
                    "{:?}: found memoized value, verified_at={:?}, changed_at={:?}",
                    self, memo.verified_at, memo.changed_at,
                );

                if let Some(value) = &memo.value {
                    if memo.verified_at == revision_now {
                        let value = StampedValue {
                            durability: memo.durability,
                            changed_at: memo.changed_at,
                            value: value.clone(),
                        };

                        info!(
                            "{:?}: returning memoized value changed at {:?}",
                            self, value.changed_at
                        );

                        return ProbeState::UpToDate(Ok(value));
                    }
                }
            }
        }

        ProbeState::StaleOrAbsent(state)
    }

    pub(super) fn durability(&self, db: &DB) -> Durability {
        match &*self.state.read() {
            QueryState::NotComputed => Durability::LOW,
            QueryState::InProgress { .. } => panic!("query in progress"),
            QueryState::Memoized(memo) => {
                if memo.check_durability(db) {
                    memo.durability
                } else {
                    Durability::LOW
                }
            }
        }
    }

    pub(super) fn as_table_entry(&self) -> Option<TableEntry<Q::Key, Q::Value>> {
        match &*self.state.read() {
            QueryState::NotComputed => None,
            QueryState::InProgress { .. } => Some(TableEntry::new(self.key.clone(), None)),
            QueryState::Memoized(memo) => {
                Some(TableEntry::new(self.key.clone(), memo.value.clone()))
            }
        }
    }

    pub(super) fn evict(&self) {
        let mut state = self.state.write();
        if let QueryState::Memoized(memo) = &mut *state {
            // Similar to GC, evicting a value with an untracked input could
            // lead to inconsistencies. Note that we can't check
            // `has_untracked_input` when we add the value to the cache,
            // because inputs can become untracked in the next revision.
            if memo.has_untracked_input() {
                return;
            }
            memo.value = None;
        }
    }

    pub(super) fn sweep(&self, revision_now: Revision, strategy: SweepStrategy) {
        let mut state = self.state.write();
        match &mut *state {
            QueryState::NotComputed => (),

            // Leave stuff that is currently being computed -- the
            // other thread doing that work has unique access to
            // this slot and we should not interfere.
            QueryState::InProgress { .. } => {
                debug!("sweep({:?}): in-progress", self);
            }

            // Otherwise, drop only value or the whole memo accoring to the
            // strategy.
            QueryState::Memoized(memo) => {
                debug!(
                    "sweep({:?}): last verified at {:?}, current revision {:?}",
                    self, memo.verified_at, revision_now
                );

                // Check if this memo read something "untracked"
                // -- meaning non-deterministic.  In this case, we
                // can only collect "outdated" data that wasn't
                // used in the current revision. This is because
                // if we collected something from the current
                // revision, we might wind up re-executing the
                // query later in the revision and getting a
                // distinct result.
                let has_untracked_input = memo.has_untracked_input();

                // Since we don't acquire a query lock in this
                // method, it *is* possible for the revision to
                // change while we are executing. However, it is
                // *not* possible for any memos to have been
                // written into this table that reflect the new
                // revision, since we are holding the write lock
                // when we read `revision_now`.
                assert!(memo.verified_at <= revision_now);
                match strategy.discard_if {
                    DiscardIf::Never => unreachable!(),

                    // If we are only discarding outdated things,
                    // and this is not outdated, keep it.
                    DiscardIf::Outdated if memo.verified_at == revision_now => (),

                    // As explained on the `has_untracked_input` variable
                    // definition, if this is a volatile entry, we
                    // can't discard it unless it is outdated.
                    DiscardIf::Always
                        if has_untracked_input && memo.verified_at == revision_now => {}

                    // Otherwise, we can discard -- discard whatever the user requested.
                    DiscardIf::Outdated | DiscardIf::Always => match strategy.discard_what {
                        DiscardWhat::Nothing => unreachable!(),
                        DiscardWhat::Values => {
                            memo.value = None;
                        }
                        DiscardWhat::Everything => {
                            *state = QueryState::NotComputed;
                        }
                    },
                }
            }
        }
    }

    /// Helper:
    ///
    /// When we encounter an `InProgress` indicator, we need to either
    /// report a cycle or else register ourselves to be notified when
    /// that work completes. This helper does that; it returns a port
    /// where you can wait for the final value that wound up being
    /// computed (but first drop the lock on the map).
    fn register_with_in_progress_thread(
        &self,
        db: &DB,
        runtime: &Runtime<DB>,
        other_id: RuntimeId,
        waiting: &Mutex<SmallVec<[Sender<StampedValue<Q::Value>>; 2]>>,
    ) -> Result<Receiver<StampedValue<Q::Value>>, CycleDetected> {
        if other_id == runtime.id() {
            return Err(CycleDetected);
        } else {
            if !runtime.try_block_on(&self.database_key(db), other_id) {
                return Err(CycleDetected);
            }

            let (tx, rx) = mpsc::channel();

            // The reader of this will have to acquire map
            // lock, we don't need any particular ordering.
            waiting.lock().push(tx);

            Ok(rx)
        }
    }

    fn should_memoize_value(&self, key: &Q::Key) -> bool {
        MP::should_memoize_value(key)
    }
}

impl<DB, Q> QueryState<DB, Q>
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
{
    fn in_progress(id: RuntimeId) -> Self {
        QueryState::InProgress {
            id,
            waiting: Default::default(),
        }
    }
}

struct PanicGuard<'me, DB, Q, MP>
where
    DB: Database + HasQueryGroup<Q::Group>,
    Q: QueryFunction<DB>,
    MP: MemoizationPolicy<DB, Q>,
{
    database_key: &'me DB::DatabaseKey,
    slot: &'me Slot<DB, Q, MP>,
    memo: Option<Memo<DB, Q>>,
    runtime: &'me Runtime<DB>,
}

impl<'me, DB, Q, MP> PanicGuard<'me, DB, Q, MP>
where
    DB: Database + HasQueryGroup<Q::Group>,
    Q: QueryFunction<DB>,
    MP: MemoizationPolicy<DB, Q>,
{
    fn new(
        database_key: &'me DB::DatabaseKey,
        slot: &'me Slot<DB, Q, MP>,
        memo: Option<Memo<DB, Q>>,
        runtime: &'me Runtime<DB>,
    ) -> Self {
        Self {
            database_key,
            slot,
            memo,
            runtime,
        }
    }

    /// Proceed with our panic guard by overwriting the placeholder for `key`.
    /// Once that completes, ensure that our deconstructor is not run once we
    /// are out of scope.
    fn proceed(mut self, new_value: &StampedValue<Q::Value>) {
        self.overwrite_placeholder(Some(new_value));
        std::mem::forget(self)
    }

    /// Overwrites the `InProgress` placeholder for `key` that we
    /// inserted; if others were blocked, waiting for us to finish,
    /// then notify them.
    fn overwrite_placeholder(&mut self, new_value: Option<&StampedValue<Q::Value>>) {
        let mut write = self.slot.state.write();

        let old_value = match self.memo.take() {
            // Replace the `InProgress` marker that we installed with the new
            // memo, thus releasing our unique access to this key.
            Some(memo) => std::mem::replace(&mut *write, QueryState::Memoized(memo)),

            // We had installed an `InProgress` marker, but we panicked before
            // it could be removed. At this point, we therefore "own" unique
            // access to our slot, so we can just remove the key.
            None => std::mem::replace(&mut *write, QueryState::NotComputed),
        };

        match old_value {
            QueryState::InProgress { id, waiting } => {
                assert_eq!(id, self.runtime.id());

                self.runtime
                    .unblock_queries_blocked_on_self(&self.database_key);

                match new_value {
                    // If anybody has installed themselves in our "waiting"
                    // list, notify them that the value is available.
                    Some(new_value) => {
                        for tx in waiting.into_inner() {
                            tx.send(new_value.clone()).unwrap()
                        }
                    }

                    // We have no value to send when we are panicking.
                    // Therefore, we need to drop the sending half of the
                    // channel so that our panic propagates to those waiting
                    // on the receiving half.
                    None => std::mem::drop(waiting),
                }
            }
            _ => panic!(
                "\
Unexpected panic during query evaluation, aborting the process.

Please report this bug to https://github.com/salsa-rs/salsa/issues."
            ),
        }
    }
}

impl<'me, DB, Q, MP> Drop for PanicGuard<'me, DB, Q, MP>
where
    DB: Database + HasQueryGroup<Q::Group>,
    Q: QueryFunction<DB>,
    MP: MemoizationPolicy<DB, Q>,
{
    fn drop(&mut self) {
        if std::thread::panicking() {
            // We panicked before we could proceed and need to remove `key`.
            self.overwrite_placeholder(None)
        } else {
            // If no panic occurred, then panic guard ought to be
            // "forgotten" and so this Drop code should never run.
            panic!(".forget() was not called")
        }
    }
}

impl<DB, Q> Memo<DB, Q>
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
{
    /// True if this memo is known not to have changed based on its durability.
    fn check_durability(&self, db: &DB) -> bool {
        let last_changed = db.salsa_runtime().last_changed_revision(self.durability);
        debug!(
            "check_durability(last_changed={:?} <= verified_at={:?}) = {:?}",
            last_changed,
            self.verified_at,
            last_changed <= self.verified_at,
        );
        last_changed <= self.verified_at
    }

    fn validate_memoized_value(
        &mut self,
        db: &DB,
        revision_now: Revision,
    ) -> Option<StampedValue<Q::Value>> {
        // If we don't have a memoized value, nothing to validate.
        if self.value.is_none() {
            return None;
        }

        assert!(self.verified_at != revision_now);
        let verified_at = self.verified_at;

        debug!(
            "validate_memoized_value({:?}): verified_at={:#?}",
            Q::default(),
            self.inputs,
        );

        if self.check_durability(db) {
            return Some(self.mark_value_as_verified(revision_now));
        }

        match &self.inputs {
            // We can't validate values that had untracked inputs; just have to
            // re-execute.
            MemoInputs::Untracked { .. } => {
                return None;
            }

            MemoInputs::NoInputs => {}

            // Check whether any of our inputs changed since the
            // **last point where we were verified** (not since we
            // last changed). This is important: if we have
            // memoized values, then an input may have changed in
            // revision R2, but we found that *our* value was the
            // same regardless, so our change date is still
            // R1. But our *verification* date will be R2, and we
            // are only interested in finding out whether the
            // input changed *again*.
            MemoInputs::Tracked { inputs } => {
                let changed_input = inputs
                    .iter()
                    .filter(|input| input.maybe_changed_since(db, verified_at))
                    .next();

                if let Some(input) = changed_input {
                    debug!(
                        "{:?}::validate_memoized_value: `{:?}` may have changed",
                        Q::default(),
                        input
                    );

                    return None;
                }
            }
        };

        Some(self.mark_value_as_verified(revision_now))
    }

    fn mark_value_as_verified(&mut self, revision_now: Revision) -> StampedValue<Q::Value> {
        let value = match &self.value {
            Some(v) => v.clone(),
            None => panic!("invoked `verify_value` without a value!"),
        };
        self.verified_at = revision_now;

        StampedValue {
            durability: self.durability,
            changed_at: self.changed_at,
            value,
        }
    }

    fn has_untracked_input(&self) -> bool {
        match self.inputs {
            MemoInputs::Untracked => true,
            _ => false,
        }
    }
}

impl<DB, Q, MP> std::fmt::Debug for Slot<DB, Q, MP>
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
    MP: MemoizationPolicy<DB, Q>,
{
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(fmt, "{:?}({:?})", Q::default(), self.key)
    }
}

impl<DB: Database> std::fmt::Debug for MemoInputs<DB> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoInputs::Tracked { inputs } => {
                fmt.debug_struct("Tracked").field("inputs", inputs).finish()
            }
            MemoInputs::NoInputs => fmt.debug_struct("NoInputs").finish(),
            MemoInputs::Untracked => fmt.debug_struct("Untracked").finish(),
        }
    }
}

impl<DB, Q, MP> LruNode for Slot<DB, Q, MP>
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
    MP: MemoizationPolicy<DB, Q>,
{
    fn lru_index(&self) -> &LruIndex {
        &self.lru_index
    }
}

// The unsafe obligation here is for us to assert that `Slot<DB, Q,
// MP>` is `Send + Sync + 'static`, assuming `Q::Key` and `Q::Value`
// are. We assert this with the `check_send_sync` and `check_static`
// functions below.
unsafe impl<DB, Q, MP> DatabaseSlot<DB> for Slot<DB, Q, MP>
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
    MP: MemoizationPolicy<DB, Q>,
{
    fn maybe_changed_since(&self, db: &DB, revision: Revision) -> bool {
        let runtime = db.salsa_runtime();
        let revision_now = runtime.current_revision();

        debug!(
            "maybe_changed_since({:?}) called with revision={:?}, revision_now={:?}",
            self, revision, revision_now,
        );

        // Acquire read lock to start. In some of the arms below, we
        // drop this explicitly.
        let state = self.state.read();

        // Look for a memoized value.
        let memo = match &*state {
            // If somebody depends on us, but we have no map
            // entry, that must mean that it was found to be out
            // of date and removed.
            QueryState::NotComputed => {
                debug!("maybe_changed_since({:?}: no value", self);
                return true;
            }

            // This value is being actively recomputed. Wait for
            // that thread to finish (assuming it's not dependent
            // on us...) and check its associated revision.
            QueryState::InProgress { id, waiting } => {
                let other_id = *id;
                debug!(
                    "maybe_changed_since({:?}: blocking on thread `{:?}`",
                    self, other_id,
                );
                match self.register_with_in_progress_thread(db, runtime, other_id, waiting) {
                    Ok(rx) => {
                        // Release our lock on `self.map`, so other thread
                        // can complete.
                        std::mem::drop(state);

                        let value = rx.recv().unwrap_or_else(|_| db.on_propagated_panic());
                        return value.changed_at > revision;
                    }

                    // Consider a cycle to have changed.
                    Err(CycleDetected) => return true,
                }
            }

            QueryState::Memoized(memo) => memo,
        };

        if memo.verified_at == revision_now {
            debug!(
                "maybe_changed_since({:?}: {:?} since up-to-date memo that changed at {:?}",
                self,
                memo.changed_at > revision,
                memo.changed_at,
            );
            return memo.changed_at > revision;
        }

        let maybe_changed;

        // If we only depended on constants, and no constant has been
        // modified since then, we cannot have changed; no need to
        // trace our inputs.
        if memo.check_durability(db) {
            std::mem::drop(state);
            maybe_changed = false;
        } else {
            match &memo.inputs {
                MemoInputs::Untracked => {
                    // we don't know the full set of
                    // inputs, so if there is a new
                    // revision, we must assume it is
                    // dirty
                    debug!(
                        "maybe_changed_since({:?}: true since untracked inputs",
                        self,
                    );
                    return true;
                }

                MemoInputs::NoInputs => {
                    std::mem::drop(state);
                    maybe_changed = false;
                }

                MemoInputs::Tracked { inputs } => {
                    // At this point, the value may be dirty (we have
                    // to check the database-keys). If we have a cached
                    // value, we'll just fall back to invoking `read`,
                    // which will do that checking (and a bit more) --
                    // note that we skip the "pure read" part as we
                    // already know the result.
                    assert!(inputs.len() > 0);
                    if memo.value.is_some() {
                        std::mem::drop(state);
                        return match self.read_upgrade(db, revision_now) {
                            Ok(v) => {
                                debug!(
                                    "maybe_changed_since({:?}: {:?} since (recomputed) value changed at {:?}",
                                    self,
                                    v.changed_at > revision,
                                    v.changed_at,
                                );
                                v.changed_at > revision
                            }
                            Err(CycleDetected) => true,
                        };
                    }

                    let inputs = inputs.clone();

                    // We have a **tracked set of inputs**
                    // (found in `database_keys`) that need to
                    // be validated.
                    std::mem::drop(state);

                    // Iterate the inputs and see if any have maybe changed.
                    maybe_changed = inputs
                        .iter()
                        .filter(|input| input.maybe_changed_since(db, revision))
                        .inspect(|input| debug!("{:?}: input `{:?}` may have changed", self, input))
                        .next()
                        .is_some();
                }
            }
        }

        // Either way, we have to update our entry.
        //
        // Keep in mind, though, we only acquired a read lock so a lot
        // could have happened in the interim. =) Therefore, we have
        // to probe the current state of `key` and in some cases we
        // ought to do nothing.
        {
            let mut state = self.state.write();
            match &mut *state {
                QueryState::Memoized(memo) => {
                    if memo.verified_at == revision_now {
                        // Since we started verifying inputs, somebody
                        // else has come along and updated this value
                        // (they may even have recomputed
                        // it). Therefore, we should not touch this
                        // memo.
                        //
                        // FIXME: Should we still return whatever
                        // `maybe_changed` value we computed,
                        // however..? It seems .. harmless to indicate
                        // that the value has changed, but possibly
                        // less efficient? (It may cause some
                        // downstream value to be recomputed that
                        // wouldn't otherwise have to be?)
                    } else if maybe_changed {
                        // We found this entry is out of date and
                        // nobody touch it in the meantime. Just
                        // remove it.
                        *state = QueryState::NotComputed;
                    } else {
                        // We found this entry is valid. Update the
                        // `verified_at` to reflect the current
                        // revision.
                        memo.verified_at = revision_now;
                    }
                }

                QueryState::InProgress { .. } => {
                    // Since we started verifying inputs, somebody
                    // else has come along and started updated this
                    // value. Just leave their marker alone and return
                    // whatever `maybe_changed` value we computed.
                }

                QueryState::NotComputed => {
                    // Since we started verifying inputs, somebody
                    // else has come along and removed this value. The
                    // GC can do this, for example. That's fine.
                }
            }
        }

        maybe_changed
    }
}

/// Check that `Slot<DB, Q, MP>: Send + Sync` as long as
/// `DB::DatabaseData: Send + Sync`, which in turn implies that
/// `Q::Key: Send + Sync`, `Q::Value: Send + Sync`.
#[allow(dead_code)]
fn check_send_sync<DB, Q, MP>()
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
    MP: MemoizationPolicy<DB, Q>,
    DB::DatabaseData: Send + Sync,
    Q::Key: Send + Sync,
    Q::Value: Send + Sync,
{
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<Slot<DB, Q, MP>>();
}

/// Check that `Slot<DB, Q, MP>: 'static` as long as
/// `DB::DatabaseData: 'static`, which in turn implies that
/// `Q::Key: 'static`, `Q::Value: 'static`.
#[allow(dead_code)]
fn check_static<DB, Q, MP>()
where
    Q: QueryFunction<DB>,
    DB: Database + HasQueryGroup<Q::Group>,
    MP: MemoizationPolicy<DB, Q>,
    DB: 'static,
    DB::DatabaseData: 'static,
    Q::Key: 'static,
    Q::Value: 'static,
{
    fn is_static<T: 'static>() {}
    is_static::<Slot<DB, Q, MP>>();
}
