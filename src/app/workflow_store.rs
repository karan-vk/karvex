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

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread::JoinHandle;

use tracing::{debug, warn};

use crate::workflow::store::{StoreError, StoreLocation, WorkflowStore};

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
        wait.recv().map_err(|_| Self::thread_lost())
    }

    /// Queues `job` without waiting. Used for the engine's durable writes, whose
    /// failure degrades persistence instead of stalling the run (`04` §9).
    pub(crate) fn submit<F>(&mut self, job: F) -> bool
    where
        F: FnOnce(&StoreContext<'_>) + Send + 'static,
    {
        match self.open() {
            Ok(worker) => worker.jobs.send(Box::new(job)).is_ok(),
            Err(_) => false,
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

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(StoreWorker {
                jobs: jobs_tx,
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
        if !self.workflow_store.is_open() || self.workflow.pending_write_count() == 0 {
            return;
        }
        let writes = self.workflow.take_pending_writes();
        for write in writes {
            let queued = self.workflow_store.submit(move |cx| {
                if let Err(error) = cx.block_on(cx.store().write(write)) {
                    warn!(error = %error, "workflow store write failed");
                }
            });
            if !queued {
                self.workflow.mark_persistence_degraded();
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
