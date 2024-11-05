use std::fmt;
use std::sync::{Arc, Mutex};

use crate::channel::ChannelObserver;
use crate::executor::{Executor, SimulationContext};
use crate::model::ProtoModel;
use crate::time::{AtomicTime, MonotonicTime, TearableAtomicTime};
use crate::time::{Clock, NoClock};
use crate::util::priority_queue::PriorityQueue;
use crate::util::sync_cell::SyncCell;

use super::{add_model, ExecutionError, Mailbox, Scheduler, SchedulerQueue, Simulation};

/// Builder for a multi-threaded, discrete-event simulation.
pub struct SimInit {
    executor: Executor,
    scheduler_queue: Arc<Mutex<SchedulerQueue>>,
    time: AtomicTime,
    clock: Box<dyn Clock + 'static>,
    observers: Vec<(String, Box<dyn ChannelObserver>)>,
}

impl SimInit {
    /// Creates a builder for a multithreaded simulation running on all
    /// available logical threads.
    pub fn new() -> Self {
        Self::with_num_threads(num_cpus::get())
    }

    /// Creates a builder for a simulation running on the specified number of
    /// threads.
    ///
    /// Note that the number of worker threads is automatically constrained to
    /// be between 1 and `usize::BITS` (inclusive).
    pub fn with_num_threads(num_threads: usize) -> Self {
        let num_threads = num_threads.clamp(1, usize::BITS as usize);
        let time = SyncCell::new(TearableAtomicTime::new(MonotonicTime::EPOCH));
        let simulation_context = SimulationContext {
            #[cfg(feature = "tracing")]
            time_reader: time.reader(),
        };

        let executor = if num_threads == 1 {
            Executor::new_single_threaded(simulation_context)
        } else {
            Executor::new_multi_threaded(num_threads, simulation_context)
        };

        Self {
            executor,
            scheduler_queue: Arc::new(Mutex::new(PriorityQueue::new())),
            time,
            clock: Box::new(NoClock::new()),
            observers: Vec::new(),
        }
    }

    /// Adds a model and its mailbox to the simulation bench.
    ///
    /// The `name` argument needs not be unique. If an empty string is provided,
    /// it is replaced by the string `<unknown>`. This name serves an identifier
    /// for logging or error-reporting purposes.
    pub fn add_model<P: ProtoModel>(
        mut self,
        model: P,
        mailbox: Mailbox<P::Model>,
        name: impl Into<String>,
    ) -> Self {
        let name = name.into();
        self.observers
            .push((name.clone(), Box::new(mailbox.0.observer())));
        let scheduler = Scheduler::new(self.scheduler_queue.clone(), self.time.reader());
        add_model(model, mailbox, name, scheduler, &self.executor);

        self
    }

    /// Synchronize the simulation with the provided [`Clock`].
    ///
    /// If the clock isn't explicitly set then the default [`NoClock`] is used,
    /// resulting in the simulation running as fast as possible.
    pub fn set_clock(mut self, clock: impl Clock + 'static) -> Self {
        self.clock = Box::new(clock);

        self
    }

    /// Builds a simulation initialized at the specified simulation time,
    /// executing the [`Model::init()`](crate::model::Model::init) method on all
    /// model initializers.
    pub fn init(mut self, start_time: MonotonicTime) -> Result<Simulation, ExecutionError> {
        self.time.write(start_time);
        self.clock.synchronize(start_time);

        let mut simulation = Simulation::new(
            self.executor,
            self.scheduler_queue,
            self.time,
            self.clock,
            self.observers,
        );
        simulation.run()?;

        Ok(simulation)
    }
}

impl Default for SimInit {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for SimInit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SimInit").finish_non_exhaustive()
    }
}
