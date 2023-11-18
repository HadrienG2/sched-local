//! State shared between the thread pool and all of its workers

pub(crate) mod flags;
pub(crate) mod futex;
pub(crate) mod hierarchical;
pub(crate) mod job;

use self::{
    flags::{bitref::BitRef, AtomicFlags},
    futex::{StealLocation, WorkerFutex},
    job::DynJob,
};
use crossbeam::{
    deque::{self, Injector, Stealer},
    utils::CachePadded,
};
use hwlocality::{bitmap::BitmapIndex, cpu::cpuset::CpuSet, Topology};
use std::{
    borrow::Borrow,
    sync::{atomic::Ordering, Arc},
};

/// State shared between all thread pool users and workers, with
/// non-hierarchical work availability tracking.
#[derive(Debug, Default)]
pub(crate) struct SharedState {
    /// Global work injector
    pub injector: Injector<DynJob>,

    /// Worker interfaces
    pub workers: Box<[CachePadded<WorkerInterface>]>,

    /// Per-worker truth that each worker _might_ have work ready to be stolen
    /// inside of its work queue
    ///
    /// Set by the associated worker upon pushing work, cleared by the worker
    /// when it tries to pop work and the queue is empty. See note in the
    /// work-stealing code to understand why it is preferrable that stealers do
    /// not clear this flag upon observing an empty work queue.
    ///
    /// The flag is set with Release ordering, so if the readout of a set flag
    /// is performed with Acquire ordering or followed by an Acquire barrier,
    /// the pushed work should be observable.
    pub work_availability: AtomicFlags,
}
//
impl SharedState {
    /// Set up the shared and worker-local state
    pub fn with_worker_config(
        topology: &Topology,
        affinity: impl Borrow<CpuSet>,
    ) -> (Arc<Self>, Box<[WorkerConfig]>) {
        // Determine which CPUs we can actually use, and cross-check that the
        // implementation supports that
        let cpuset = topology.cpuset() & affinity;
        let num_workers = cpuset.weight().unwrap();
        assert_ne!(
            num_workers, 0,
            "a thread pool without threads can't make progress and will deadlock on first request"
        );
        assert!(
            num_workers < WorkerFutex::MAX_WORKERS,
            "unsupported number of worker threads"
        );

        // Set up worker-local state
        let mut worker_configs = Vec::with_capacity(num_workers);
        let mut worker_interfaces = Vec::with_capacity(num_workers);
        for cpu in cpuset {
            let (interface, work_queue) = WorkerInterface::with_work_queue();
            worker_interfaces.push(CachePadded::new(interface));
            worker_configs.push(WorkerConfig { work_queue, cpu });
        }

        // Set up global shared state
        let result = Arc::new(Self {
            injector: Injector::new(),
            workers: worker_interfaces.into(),
            work_availability: AtomicFlags::new(num_workers),
        });
        (result, worker_configs.into())
    }

    /// Recommend that the work-less thread closest to a certain originating
    /// locality steal a task from the specified location
    pub fn recommend_steal<'self_, const INCLUDE_CENTER: bool, const CACHE_ITER_MASKS: bool>(
        &'self_ self,
        local_worker: &BitRef<'self_, CACHE_ITER_MASKS>,
        task_location: StealLocation,
        update: Ordering,
    ) {
        // Check if there are job-less neighbors to submit work to...
        //
        // Need Acquire ordering so the futex is read after the work
        // availability flag, no work availability caching/speculation allowed.
        let Some(mut asleep_neighbors) = self
            .work_availability
            .iter_unset_around::<INCLUDE_CENTER, CACHE_ITER_MASKS>(local_worker, Ordering::Acquire)
        else {
            return;
        };

        // ...and if so, tell the closest one about our newly submitted job
        #[cold]
        fn unlikely<'self_, const CACHE_ITER_MASKS: bool>(
            self_: &'self_ SharedState,
            asleep_neighbors: impl Iterator<Item = BitRef<'self_, false>>,
            local_worker: &BitRef<'self_, CACHE_ITER_MASKS>,
            task_location: StealLocation,
            update: Ordering,
        ) {
            // Iterate over increasingly remote job-less neighbors
            let local_worker = local_worker.linear_idx(&self_.work_availability);
            for closest_asleep in asleep_neighbors {
                // Update their futex recommendation as appropriate
                //
                // Can use Relaxed ordering on failure because failing to
                // suggest work to a worker has no observable consequences and
                // isn't used to inform any decision other than looking up the
                // state of the next worker. Worker states are independent.
                let closest_asleep = closest_asleep.linear_idx(&self_.work_availability);
                let accepted = self_.workers[closest_asleep].futex.suggest_steal(
                    task_location,
                    local_worker,
                    update,
                    Ordering::Relaxed,
                );
                if accepted.is_ok() {
                    return;
                }
            }
        }
        unlikely(
            self,
            &mut asleep_neighbors,
            local_worker,
            task_location,
            update,
        )
    }

    /// Enumerate workers that a thief could steal work from, at increasing
    /// distance from said thief
    pub fn find_steals<'self_, const CACHE_ITER_MASKS: bool>(
        &'self_ self,
        thief: &BitRef<'self_, CACHE_ITER_MASKS>,
        load: Ordering,
    ) -> Option<impl Iterator<Item = usize> + '_> {
        let work_availability = &self.work_availability;
        work_availability
            .iter_set_around::<false, CACHE_ITER_MASKS>(thief, load)
            .map(|iter| iter.map(|bit| bit.linear_idx(work_availability)))
    }
}

/// State needed to configure a new worker
#[derive(Debug)]
pub(crate) struct WorkerConfig {
    /// Work queue
    pub work_queue: deque::Worker<DynJob>,

    /// CPU which this worker should be pinned to
    pub cpu: BitmapIndex,
}

/// External interface to a single worker in a thread pool
#[derive(Debug)]
pub(crate) struct WorkerInterface {
    /// A way to steal from the worker
    pub stealer: Stealer<DynJob>,

    /// Futex that the worker sleeps on when it has nothing to do, used to
    /// instruct it what to do when it is awakened.
    pub futex: WorkerFutex,
}
//
impl WorkerInterface {
    /// Set up a worker's work queue and external interface
    pub fn with_work_queue() -> (Self, deque::Worker<DynJob>) {
        let worker = deque::Worker::new_lifo();
        let interface = Self {
            stealer: worker.stealer(),
            futex: WorkerFutex::new(),
        };
        (interface, worker)
    }
}
