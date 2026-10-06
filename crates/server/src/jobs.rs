//! The process-level worker set for per-client jobs (plan 6.2, decision 0008).
//!
//! A cell offers one job per client. Each job is queued for a worker while the
//! cell has fewer than its slot quota in flight and the queue has room;
//! otherwise it runs inline on the cell thread. The cell thread then waits
//! for its own jobs only, so one cell's burst never delays another's tick
//! beyond the shared workers. Dispatch is allocation-free: the queue is
//! pre-sized and a job is an `Arc` clone plus an index.
//!
//! Every job runs through a [`JobWrapper`], on whichever thread executes it.
//! Tests install one that counts allocations per job (decision 0008: the
//! allocation harness wraps the jobs as well as the cell thread).

use crate::lock;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// Runs one job body: `f` must be called exactly once.
pub type JobWrapper = fn(&mut dyn FnMut());

fn direct(f: &mut dyn FnMut()) {
    f();
}

/// A set of jobs offered by one owner (a cell).
pub trait JobBatch: Send + Sync + 'static {
    /// Runs job `index`.
    fn run(&self, index: usize);
}

/// Completion tracking for one batch owner.
#[derive(Default)]
pub struct Completion {
    pending: Mutex<usize>,
    done: Condvar,
}

impl Completion {
    /// A new tracker.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn finish_one(&self) {
        let mut p = lock(&self.pending);
        *p = p.saturating_sub(1);
        if *p == 0 {
            self.done.notify_all();
        }
    }
}

struct Job {
    batch: Arc<dyn JobBatch>,
    index: usize,
    completion: Arc<Completion>,
}

struct Pool {
    queue: Mutex<VecDeque<Job>>,
    ready: Condvar,
    shutdown: AtomicBool,
    capacity: usize,
    wrapper: JobWrapper,
}

/// How one batch was executed.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct BatchReport {
    /// Jobs run on workers.
    pub offloaded: u32,
    /// Jobs run inline on the calling thread.
    pub inline: u32,
}

/// The process-level worker set.
pub struct WorkerSet {
    pool: Arc<Pool>,
    threads: Vec<JoinHandle<()>>,
}

impl WorkerSet {
    /// Starts `threads` workers with a queue of `queue_capacity` jobs. Jobs
    /// run through `wrapper` (or directly).
    #[must_use]
    pub fn new(threads: usize, queue_capacity: usize, wrapper: Option<JobWrapper>) -> Self {
        let wrapper = wrapper.unwrap_or(direct);
        let pool = Arc::new(Pool {
            queue: Mutex::new(VecDeque::with_capacity(queue_capacity)),
            ready: Condvar::new(),
            shutdown: AtomicBool::new(false),
            capacity: queue_capacity,
            wrapper,
        });
        let threads = (0..threads)
            .filter_map(|i| {
                let pool = Arc::clone(&pool);
                std::thread::Builder::new()
                    .name(format!("mantis-job-{i}"))
                    .spawn(move || worker(&pool))
                    .ok()
            })
            .collect();
        Self { pool, threads }
    }

    /// Worker threads running.
    #[must_use]
    pub fn threads(&self) -> usize {
        self.threads.len()
    }

    /// The wrapper jobs run through.
    #[must_use]
    pub fn wrapper(&self) -> JobWrapper {
        self.pool.wrapper
    }

    /// Runs `batch` for every index in `indices`: offloaded while fewer than
    /// `quota` of this owner's jobs are in flight and the queue has room,
    /// inline otherwise. Returns when all of them have finished.
    pub fn run_batch(
        &self,
        batch: &Arc<dyn JobBatch>,
        completion: &Arc<Completion>,
        indices: impl Iterator<Item = usize>,
        quota: usize,
    ) -> BatchReport {
        let mut report = BatchReport::default();
        for index in indices {
            let offload = !self.threads.is_empty() && *lock(&completion.pending) < quota && {
                let q = lock(&self.pool.queue);
                q.len() < self.pool.capacity
            };
            if offload {
                *lock(&completion.pending) += 1;
                lock(&self.pool.queue).push_back(Job {
                    batch: Arc::clone(batch),
                    index,
                    completion: Arc::clone(completion),
                });
                self.pool.ready.notify_one();
                report.offloaded += 1;
            } else {
                (self.pool.wrapper)(&mut || batch.run(index));
                report.inline += 1;
            }
        }
        let mut pending = lock(&completion.pending);
        while *pending > 0 {
            pending = completion
                .done
                .wait(pending)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        report
    }
}

fn worker(pool: &Pool) {
    loop {
        let job = {
            let mut q = lock(&pool.queue);
            loop {
                if let Some(job) = q.pop_front() {
                    break Some(job);
                }
                if pool.shutdown.load(Ordering::Acquire) {
                    break None;
                }
                q = pool
                    .ready
                    .wait(q)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let Some(job) = job else { return };
        (pool.wrapper)(&mut || job.batch.run(job.index));
        job.completion.finish_one();
    }
}

impl Drop for WorkerSet {
    fn drop(&mut self) {
        self.pool.shutdown.store(true, Ordering::Release);
        self.pool.ready.notify_all();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    struct Squares {
        out: Vec<AtomicU64>,
    }

    impl JobBatch for Squares {
        fn run(&self, index: usize) {
            if let Some(slot) = self.out.get(index) {
                slot.store((index * index) as u64, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn runs_every_job_with_quota_and_inline_fallback() {
        let set = WorkerSet::new(3, 8, None);
        let squares = Arc::new(Squares {
            out: (0..100).map(|_| AtomicU64::new(0)).collect(),
        });
        let batch: Arc<dyn JobBatch> = squares.clone();
        let done = Completion::new();
        let r = set.run_batch(&batch, &done, 0..100, 4);
        assert_eq!(r.offloaded + r.inline, 100);
        assert!(r.inline > 0, "quota 4 forces inline work");
        for (i, v) in squares.out.iter().enumerate() {
            assert_eq!(v.load(Ordering::Relaxed), (i * i) as u64);
        }
        // Quota 0: everything inline.
        let r = set.run_batch(&batch, &done, 0..10, 0);
        assert_eq!(
            r,
            BatchReport {
                offloaded: 0,
                inline: 10
            }
        );
    }

    #[test]
    fn no_threads_means_inline() {
        let set = WorkerSet::new(0, 8, None);
        let squares = Arc::new(Squares {
            out: (0..5).map(|_| AtomicU64::new(0)).collect(),
        });
        let batch: Arc<dyn JobBatch> = squares;
        let r = set.run_batch(&batch, &Completion::new(), 0..5, 8);
        assert_eq!(
            r,
            BatchReport {
                offloaded: 0,
                inline: 5
            }
        );
    }
}
