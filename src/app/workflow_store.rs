//! The server's synchronous door to the async [`WorkflowStore`].
//!
//! `03-storage-schema.md` §2: the store is opened **lazily on first
//! `workflow.*` use**, so a karvex that never touches workflows never pays the
//! open cost, and a directory another server holds puts the whole subsystem in
//! `Unavailable { reason: "store_locked", holder }` rather than failing one
//! call at a time.
//!
//! The API handlers are synchronous (`handle_api_request` returns a `String`)
//! while SurrealDB is async, so the store lives on its own thread with its own
//! current-thread runtime and is reached over a channel. Blocking the event
//! loop for the reply is deliberate and bounded: the engine's *durable* writes
//! (`04` §9) are queued by `WorkflowRuntimeState` and submitted here without a
//! reply, so nothing on a node's critical path waits on the disk. Only the
//! handful of request/response methods — create, version.create, run, list —
//! wait, and only because their answer is the response.
//!
//! Going through a thread rather than `block_in_place` keeps this correct on
//! every runtime flavour: the headless server builds a multi-thread runtime,
//! but in-crate tests drive `App` with no runtime at all.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use tracing::{debug, warn};

use crate::workflow::store::{StoreError, StoreLocation, WorkflowStore};

/// How long a request/response store call may hold the event loop before it is
/// reported as unavailable. The store is an embedded database on local disk, so
/// anything near this is pathological — but "deliberate and bounded" has to have
/// something enforcing the bound, or a wedged query freezes input and rendering
/// for as long as it takes.
const CALL_DEADLINE: Duration = Duration::from_secs(30);

/// How many durable writes may be in the store thread's queue at once. Past
/// this, writes stay in `WorkflowRuntimeState`'s own bounded queue, which is
/// what makes its budget real backpressure instead of a limit the unbounded
/// channel downstream can never let it reach.
const STORE_QUEUE_BUDGET: usize = 512;

/// What the store thread reports back to the event loop without a reply
/// channel: the depth of its own queue, and how many durable writes failed
/// (`04` §9 degrades the run's persistence rather than failing it).
#[derive(Debug, Default)]
struct QueueStats {
    in_flight: AtomicUsize,
    failures: AtomicU64,
}

/// A store failure that has taken the whole subsystem out of service, reduced
/// to the pair a `workflow.*` response needs. [`StoreError`] is not `Clone`, and
/// the subsystem-level state has to outlive the call that discovered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoreUnavailable {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl StoreUnavailable {
    fn from_error(error: &StoreError) -> Self {
        Self {
            code: error.api_code(),
            message: error.to_string(),
        }
    }
}

/// Borrowed store plus the runtime that drives it, handed to a job on the store
/// thread. `block_on` is safe here precisely because this thread is not inside
/// an async context.
pub(crate) struct StoreContext<'a> {
    runtime: &'a tokio::runtime::Runtime,
    store: &'a WorkflowStore,
}

impl StoreContext<'_> {
    pub(crate) fn store(&self) -> &WorkflowStore {
        self.store
    }

    pub(crate) fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(future)
    }
}

type StoreJob = Box<dyn FnOnce(&StoreContext<'_>) + Send + 'static>;

struct StoreWorker {
    jobs: Sender<StoreJob>,
    stats: Arc<QueueStats>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for StoreWorker {
    /// Closing the channel ends the worker's loop; joining lets the writes
    /// already queued behind it reach disk instead of dying with the process.
    fn drop(&mut self) {
        let (dead, _) = channel();
        drop(std::mem::replace(&mut self.jobs, dead));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
enum HandleState {
    #[default]
    Unopened,
    Open(StoreWorker),
    /// The open failed. `03` §2 makes this a subsystem state, not a per-call
    /// one, so it is remembered rather than retried on every request — and it
    /// is never degraded to an in-memory store, which would look like data
    /// loss.
    Unavailable(StoreUnavailable),
}

/// Owns the store thread and opens it on first use.
#[derive(Default)]
pub(crate) struct WorkflowStoreHandle {
    state: HandleState,
}

impl std::fmt::Debug for WorkflowStoreHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match &self.state {
            HandleState::Unopened => "unopened",
            HandleState::Open(_) => "open",
            HandleState::Unavailable(_) => "unavailable",
        };
        f.debug_struct("WorkflowStoreHandle")
            .field("state", &state)
            .finish()
    }
}

impl WorkflowStoreHandle {
    /// Runs `job` on the store thread and waits for its answer.
    pub(crate) fn call<T, F>(&mut self, job: F) -> Result<T, StoreUnavailable>
    where
        F: FnOnce(&StoreContext<'_>) -> T + Send + 'static,
        T: Send + 'static,
    {
        let worker = self.open()?;
        let (reply, wait) = channel();
        let submitted = worker.jobs.send(Box::new(move |cx| {
            let _ = reply.send(job(cx));
        }));
        if submitted.is_err() {
            return Err(Self::thread_lost());
        }
        match wait.recv_timeout(CALL_DEADLINE) {
            Ok(answer) => Ok(answer),
            Err(RecvTimeoutError::Timeout) => Err(Self::call_timed_out()),
            Err(RecvTimeoutError::Disconnected) => Err(Self::thread_lost()),
        }
    }

    /// Queues `job` without waiting. Used for the engine's durable writes,
    /// whose failure degrades persistence instead of stalling the run
    /// (`04` §9): a job that reports an error is counted here, on the store
    /// thread, and collected by [`Self::take_write_failures`], because the
    /// store thread has no way to reach `WorkflowRuntimeState` itself.
    ///
    /// The returned bool says only whether the job was *queued*.
    pub(crate) fn submit<F>(&mut self, job: F) -> bool
    where
        F: FnOnce(&StoreContext<'_>) -> Result<(), StoreError> + Send + 'static,
    {
        let Ok(worker) = self.open() else {
            return false;
        };
        let stats = Arc::clone(&worker.stats);
        stats.in_flight.fetch_add(1, Ordering::Relaxed);
        let queued = worker
            .jobs
            .send(Box::new(move |cx| {
                if let Err(error) = job(cx) {
                    warn!(error = %error, "workflow store write failed");
                    stats.failures.fetch_add(1, Ordering::Relaxed);
                }
                stats.in_flight.fetch_sub(1, Ordering::Relaxed);
            }))
            .is_ok();
        if !queued {
            worker.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
        queued
    }

    /// How many submitted jobs the store thread has not finished yet.
    pub(crate) fn in_flight(&self) -> usize {
        match &self.state {
            HandleState::Open(worker) => worker.stats.in_flight.load(Ordering::Relaxed),
            _ => 0,
        }
    }

    /// Durable-write failures observed on the store thread since the last call,
    /// and clears the counter. The store thread cannot reach
    /// `WorkflowRuntimeState`, so this is how a failed write becomes a
    /// surfaced `persistence_degraded` rather than only a log line.
    pub(crate) fn take_write_failures(&self) -> u64 {
        match &self.state {
            HandleState::Open(worker) => worker.stats.failures.swap(0, Ordering::Relaxed),
            _ => 0,
        }
    }

    /// Whether the database is already open. The durable-write drain is gated
    /// on this: a run can only exist because a `workflow.*` call opened the
    /// store, so queueing writes never has to open it, and an engine tick in a
    /// server that has never touched workflows cannot take the lock as a side
    /// effect.
    pub(crate) fn is_open(&self) -> bool {
        matches!(self.state, HandleState::Open(_))
    }

    /// An already-open handle backed by `kv-mem`. Unit tests drive `App`'s
    /// `workflow.*` handlers through this so they never touch — or lock — the
    /// developer's real database.
    #[cfg(test)]
    pub(crate) fn in_memory() -> Self {
        match Self::spawn(StoreLocation::Memory) {
            Ok(worker) => Self {
                state: HandleState::Open(worker),
            },
            Err(unavailable) => Self {
                state: HandleState::Unavailable(unavailable),
            },
        }
    }

    fn thread_lost() -> StoreUnavailable {
        StoreUnavailable {
            code: crate::workflow::store::error::WORKFLOW_STORE_ERROR_CODE,
            message: "the workflow store thread is no longer running".to_string(),
        }
    }

    fn call_timed_out() -> StoreUnavailable {
        StoreUnavailable {
            code: crate::workflow::store::error::WORKFLOW_STORE_ERROR_CODE,
            message: format!(
                "the workflow store did not answer within {}s",
                CALL_DEADLINE.as_secs()
            ),
        }
    }

    fn open(&mut self) -> Result<&StoreWorker, StoreUnavailable> {
        if let HandleState::Unopened = self.state {
            self.state = match Self::spawn(WorkflowStore::default_location()) {
                Ok(worker) => HandleState::Open(worker),
                Err(unavailable) => {
                    warn!(
                        code = unavailable.code,
                        message = %unavailable.message,
                        "workflow store unavailable"
                    );
                    HandleState::Unavailable(unavailable)
                }
            };
        }
        match &self.state {
            HandleState::Open(worker) => Ok(worker),
            HandleState::Unavailable(unavailable) => Err(unavailable.clone()),
            // `Unopened` was replaced above; this arm cannot be reached without
            // that assignment being removed.
            HandleState::Unopened => Err(Self::thread_lost()),
        }
    }

    fn spawn(location: StoreLocation) -> Result<StoreWorker, StoreUnavailable> {
        let (jobs_tx, jobs_rx) = channel::<StoreJob>();
        let (ready_tx, ready_rx) = channel::<Result<(), StoreUnavailable>>();
        let thread = std::thread::Builder::new()
            .name("karvex-workflow-store".to_string())
            .spawn(move || worker_main(location, &ready_tx, &jobs_rx))
            .map_err(|error| StoreUnavailable {
                code: crate::workflow::store::error::WORKFLOW_STORE_ERROR_CODE,
                message: format!("the workflow store thread could not be started: {error}"),
            })?;

        // Deliberately not deadlined, unlike `call`: a slow first open (the
        // migrations run here) that timed out would leave the whole subsystem
        // stuck in `Unavailable`, which `03` §2 makes sticky.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(StoreWorker {
                jobs: jobs_tx,
                stats: Arc::new(QueueStats::default()),
                thread: Some(thread),
            }),
            Ok(Err(unavailable)) => {
                let _ = thread.join();
                Err(unavailable)
            }
            Err(_) => {
                let _ = thread.join();
                Err(Self::thread_lost())
            }
        }
    }
}

fn worker_main(
    location: StoreLocation,
    ready: &Sender<Result<(), StoreUnavailable>>,
    jobs: &Receiver<StoreJob>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(StoreUnavailable {
                code: crate::workflow::store::error::WORKFLOW_STORE_ERROR_CODE,
                message: format!("the workflow store runtime could not be built: {error}"),
            }));
            return;
        }
    };

    let store = match runtime.block_on(WorkflowStore::open(location)) {
        Ok(store) => store,
        Err(error) => {
            let _ = ready.send(Err(StoreUnavailable::from_error(&error)));
            return;
        }
    };
    debug!(location = ?store.location(), "workflow store opened");
    if ready.send(Ok(())).is_err() {
        return;
    }

    let context = StoreContext {
        runtime: &runtime,
        store: &store,
    };
    while let Ok(job) = jobs.recv() {
        job(&context);
    }
}

impl crate::app::App {
    /// Hands the engine's queued durable writes to the store thread. Called
    /// after every effect batch; a write that cannot be queued marks the run
    /// persistence-degraded rather than failing it (`04` §9).
    pub(crate) fn drain_workflow_store_writes(&mut self) {
        // Opening the database from here would defeat the lazy-open rule of
        // `03` §2 and would make an `AppState`-only unit test take the on-disk
        // lock. A run always begins with a `workflow.*` call that opened the
        // store, so the gate never costs a real run its journal.
        if !self.workflow_store.is_open() {
            return;
        }
        // A write that failed on the store thread degrades the run's
        // persistence, which is surfaced — the journal is incomplete, but the
        // in-memory graph is authoritative during a run, so nothing is failed
        // over it (`04` §9).
        if self.workflow_store.take_write_failures() > 0 {
            self.mark_workflow_persistence_degraded();
        }
        if self.workflow.pending_write_count() == 0 {
            return;
        }
        // Only as many as the store thread's queue has room for. The rest stay
        // in the engine's own bounded queue, so a store thread that falls
        // behind applies backpressure there instead of growing without limit.
        let room = STORE_QUEUE_BUDGET.saturating_sub(self.workflow_store.in_flight());
        if room == 0 {
            return;
        }
        for write in self.workflow.take_pending_writes(room) {
            let queued = self
                .workflow_store
                .submit(move |cx| cx.block_on(cx.store().write(write)));
            if !queued {
                self.mark_workflow_persistence_degraded();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_handle() -> WorkflowStoreHandle {
        let handle = WorkflowStoreHandle::in_memory();
        assert!(handle.is_open(), "the in-memory store opens");
        handle
    }

    #[test]
    fn a_call_runs_on_the_store_thread_and_returns_its_answer() {
        let mut handle = memory_handle();
        let workflows = handle
            .call(|cx| cx.block_on(cx.store().list_workflows()))
            .expect("the store thread answers");
        assert_eq!(workflows.expect("the query succeeds").len(), 0);
    }

    #[test]
    fn work_submitted_without_a_reply_still_reaches_the_store() {
        let mut handle = memory_handle();
        let (done_tx, done_rx) = channel();
        assert!(handle.submit(move |cx| {
            let created = cx.block_on(cx.store().create_workflow(
                "queued",
                "",
                crate::workflow::tier::Tier::High,
            ));
            let _ = done_tx.send(created.is_ok());
            created.map(|_| ())
        }));
        assert!(done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the submitted job runs"));

        let listed = handle
            .call(|cx| cx.block_on(cx.store().list_workflows()))
            .expect("the store thread answers")
            .expect("the query succeeds");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "queued");
        assert_eq!(
            handle.take_write_failures(),
            0,
            "a job that succeeded is not counted as a lost write"
        );
    }

    #[test]
    fn a_submitted_job_that_fails_is_counted_for_the_event_loop() {
        let mut handle = memory_handle();
        let (done_tx, done_rx) = channel();
        assert!(handle.submit(move |_| {
            let _ = done_tx.send(());
            Err(StoreError::Query("deliberate failure".to_string()))
        }));
        done_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the submitted job runs");
        // The counter is written after the job body returns, so wait for the
        // store thread to come back for more work before reading it.
        handle
            .call(|_| ())
            .expect("the store thread answers after the failing job");

        assert_eq!(handle.take_write_failures(), 1);
        assert_eq!(
            handle.take_write_failures(),
            0,
            "taking the failures clears them"
        );
        assert_eq!(handle.in_flight(), 0);
    }

    #[test]
    fn an_unopened_handle_reports_itself_as_unopened() {
        let handle = WorkflowStoreHandle::default();
        assert!(!handle.is_open());
        assert!(format!("{handle:?}").contains("unopened"));
    }

    #[test]
    fn a_handle_that_could_not_open_keeps_reporting_the_same_failure() {
        let mut handle = WorkflowStoreHandle {
            state: HandleState::Unavailable(StoreUnavailable {
                code: crate::workflow::store::error::WORKFLOW_UNAVAILABLE_CODE,
                message: "held by pid 4242".to_string(),
            }),
        };
        let first = handle
            .call(|_| ())
            .expect_err("the subsystem is unavailable");
        let second = handle
            .call(|_| ())
            .expect_err("the subsystem is unavailable");
        assert_eq!(first, second);
        assert_eq!(
            first.code,
            crate::workflow::store::error::WORKFLOW_UNAVAILABLE_CODE
        );
        assert!(!handle.is_open());
    }
}
