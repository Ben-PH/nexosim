use std::cell::RefCell;
use std::fmt;
use std::future::Future;
use std::sync::atomic::Ordering;

use slab::Slab;

use super::task::{self, CancelToken, Promise, Runnable};
use super::NEXT_EXECUTOR_ID;

use crate::macros::scoped_thread_local::scoped_thread_local;

const QUEUE_MIN_CAPACITY: usize = 32;

scoped_thread_local!(static EXECUTOR_CONTEXT: ExecutorContext);
scoped_thread_local!(static ACTIVE_TASKS: RefCell<Slab<CancelToken>>);

/// A single-threaded `async` executor.
pub(crate) struct Executor {
    /// Shared executor data.
    context: ExecutorContext,
    /// List of tasks that have not completed yet.
    active_tasks: RefCell<Slab<CancelToken>>,
}

impl Executor {
    /// Creates an executor that runs futures on the current thread.
    pub(crate) fn new() -> Self {
        // Each executor instance has a unique ID inherited by tasks to ensure
        // that tasks are scheduled on their parent executor.
        let executor_id = NEXT_EXECUTOR_ID.fetch_add(1, Ordering::Relaxed);
        assert!(
            executor_id <= usize::MAX / 2,
            "too many executors have been instantiated"
        );

        let context = ExecutorContext::new(executor_id);
        let active_tasks = RefCell::new(Slab::new());

        Self {
            context,
            active_tasks,
        }
    }

    /// Spawns a task and returns a promise that can be polled to retrieve the
    /// task's output.
    ///
    /// Note that spawned tasks are not executed until [`run()`](Executor::run)
    /// is called.
    pub(crate) fn spawn<T>(&self, future: T) -> Promise<T::Output>
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        // Book a slot to store the task cancellation token.
        let mut active_tasks = self.active_tasks.borrow_mut();
        let task_entry = active_tasks.vacant_entry();

        // Wrap the future so that it removes its cancel token from the
        // executor's list when dropped.
        let future = CancellableFuture::new(future, task_entry.key());

        let (promise, runnable, cancel_token) =
            task::spawn(future, schedule_task, self.context.executor_id);

        task_entry.insert(cancel_token);
        let mut queue = self.context.queue.borrow_mut();
        queue.push(runnable);

        promise
    }

    /// Spawns a task which output will never be retrieved.
    ///
    /// This is mostly useful to avoid undue reference counting for futures that
    /// return a `()` type.
    ///
    /// Note that spawned tasks are not executed until [`run()`](Executor::run)
    /// is called.
    pub(crate) fn spawn_and_forget<T>(&self, future: T)
    where
        T: Future + Send + 'static,
        T::Output: Send + 'static,
    {
        // Book a slot to store the task cancellation token.
        let mut active_tasks = self.active_tasks.borrow_mut();
        let task_entry = active_tasks.vacant_entry();

        // Wrap the future so that it removes its cancel token from the
        // executor's list when dropped.
        let future = CancellableFuture::new(future, task_entry.key());

        let (runnable, cancel_token) =
            task::spawn_and_forget(future, schedule_task, self.context.executor_id);

        task_entry.insert(cancel_token);
        let mut queue = self.context.queue.borrow_mut();
        queue.push(runnable);
    }

    /// Execute spawned tasks, blocking until all futures have completed or
    /// until the executor reaches a deadlock.
    pub(crate) fn run(&mut self) {
        ACTIVE_TASKS.set(&self.active_tasks, || {
            EXECUTOR_CONTEXT.set(&self.context, || loop {
                let task = match self.context.queue.borrow_mut().pop() {
                    Some(task) => task,
                    None => break,
                };

                task.run();
            })
        });
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // Drop all tasks that have not completed.
        //
        // The executor context must be set because some tasks may schedule
        // other tasks when dropped, which requires that the work queue be
        // available.
        EXECUTOR_CONTEXT.set(&self.context, || {
            // Cancel all pending futures.
            //
            // `ACTIVE_TASKS` is explicitly unset to prevent
            // `CancellableFuture::drop()` from trying to remove its own token
            // from the list of active tasks as this would result in a nested
            // call to `borrow_mut` and thus a panic. This is mainly to stay on
            // the safe side: `ACTIVE_TASKS` should not be set anyway, unless
            // for some reason the executor runs inside another executor.
            ACTIVE_TASKS.unset(|| {
                let mut tasks = self.active_tasks.borrow_mut();
                for task in tasks.drain() {
                    task.cancel();
                }

                // Some of the dropped tasks may have scheduled other tasks that
                // were not yet cancelled, preventing them from being dropped
                // upon cancellation. This is OK: the scheduled tasks will be
                // dropped when the work queue is dropped, and they cannot
                // re-schedule one another since all tasks were cancelled.
            });
        });
    }
}

impl fmt::Debug for Executor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Executor").finish_non_exhaustive()
    }
}

/// Shared executor context.
///
/// This contains all executor resources that can be shared between threads.
struct ExecutorContext {
    /// Work queue.
    queue: RefCell<Vec<Runnable>>,
    /// Unique executor identifier inherited by all tasks spawned on this
    /// executor instance.
    executor_id: usize,
}

impl ExecutorContext {
    /// Creates a new shared executor context.
    fn new(executor_id: usize) -> Self {
        Self {
            queue: RefCell::new(Vec::with_capacity(QUEUE_MIN_CAPACITY)),
            executor_id,
        }
    }
}

/// A `Future` wrapper that removes its cancellation token from the list of
/// active tasks when dropped.
struct CancellableFuture<T: Future> {
    inner: T,
    cancellation_key: usize,
}

impl<T: Future> CancellableFuture<T> {
    /// Creates a new `CancellableFuture`.
    fn new(fut: T, cancellation_key: usize) -> Self {
        Self {
            inner: fut,
            cancellation_key,
        }
    }
}

impl<T: Future> Future for CancellableFuture<T> {
    type Output = T::Output;

    #[inline(always)]
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).poll(cx) }
    }
}

impl<T: Future> Drop for CancellableFuture<T> {
    fn drop(&mut self) {
        // Remove the task from the list of active tasks while the executor is
        // running (meaning that `ACTIVE_TASK` is set). Otherwise do nothing and
        // let the executor's drop handler do the cleanup.
        let _ = ACTIVE_TASKS.map(|active_tasks| {
            // Don't use `borrow_mut()` because this function can be called from
            // a destructor and should not panic. In the worse case, the cancel
            // token will be left in the list of active tasks, which does
            // prevents eager task deallocation but does not cause any issue
            // otherwise.
            if let Ok(mut active_tasks) = active_tasks.try_borrow_mut() {
                let _cancel_token = active_tasks.try_remove(self.cancellation_key);
            }
        });
    }
}

/// Schedules a `Runnable` from within a worker thread.
///
/// # Panics
///
/// This function will panic if called from called outside from the executor
/// work thread or from another executor instance than the one the task for this
/// `Runnable` was spawned on.
fn schedule_task(task: Runnable, executor_id: usize) {
    EXECUTOR_CONTEXT
        .map(|context| {
            // Check that this task was indeed spawned on this executor.
            assert_eq!(
                executor_id, context.executor_id,
                "Tasks must be awaken on the same executor they are spawned on"
            );

            let mut queue = context.queue.borrow_mut();
            queue.push(task);
        })
        .expect("Tasks may not be awaken outside executor threads");
}
