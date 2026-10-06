//! The streaming pool: worker threads that load assets and world sectors by priority, and
//! a budgeted hand-off of finished work to the render thread.
//!
//! Loading (file reads, decompression, decoding) runs on workers and costs the frame
//! nothing. Hand-off (GPU uploads, registration) runs on the render thread, so it is
//! budgeted: [`StreamingPool::drain_budgeted`] stops handing off once the frame's budget
//! is spent (plan 17: sector stream-in render-thread hand-off under 2 ms per frame). One
//! item is always handed off per call, so progress never stalls on an item larger than
//! the budget.
//!
//! Priorities change as the camera moves: queued jobs can be re-prioritized or cancelled.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::time::HostClock;

/// A unit of streaming work.
pub trait StreamJob: Send + 'static {
    /// What the job produces for hand-off.
    type Output: Send + 'static;
    /// Does the work. Runs on a worker thread.
    fn run(self) -> Self::Output;
}

/// Job priority; higher runs first, ties run in submission order.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Priority(pub u32);

/// Identifies a submitted job.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct JobId(u64);

/// A finished job awaiting hand-off.
#[derive(Debug)]
pub enum Completed<O> {
    /// The job produced output.
    Done {
        /// The job.
        id: JobId,
        /// Its output.
        output: O,
    },
    /// The job panicked; the worker survived and the failure is reported, not hidden.
    Failed {
        /// The job.
        id: JobId,
    },
}

/// Outcome of one budgeted hand-off.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct HandoffReport {
    /// Items handed off.
    pub handed_off: u32,
    /// Failed jobs reported.
    pub failed: u32,
    /// Time spent, measured on the host clock.
    pub elapsed: Duration,
    /// True when finished items remain for a later frame.
    pub remaining: bool,
}

struct Queue<J> {
    jobs: Vec<(JobId, Priority, J)>,
    stop: bool,
}

struct Shared<J> {
    queue: Mutex<Queue<J>>,
    ready: Condvar,
}

/// The streaming pool.
pub struct StreamingPool<J: StreamJob> {
    shared: Arc<Shared<J>>,
    done: Receiver<Completed<J::Output>>,
    pending: VecDeque<Completed<J::Output>>,
    workers: Vec<JoinHandle<()>>,
    next_id: u64,
}

impl<J: StreamJob> core::fmt::Debug for StreamingPool<J> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamingPool")
            .field("workers", &self.workers.len())
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl<J: StreamJob> StreamingPool<J> {
    /// Starts `workers` worker threads (at least one).
    ///
    /// # Errors
    /// The OS error if a worker cannot be spawned; workers already started are stopped.
    pub fn new(workers: usize) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                jobs: Vec::new(),
                stop: false,
            }),
            ready: Condvar::new(),
        });
        let (tx, rx) = channel();
        let mut pool = Self {
            shared,
            done: rx,
            pending: VecDeque::new(),
            workers: Vec::new(),
            next_id: 0,
        };
        for i in 0..workers.max(1) {
            let shared = Arc::clone(&pool.shared);
            let tx: Sender<Completed<J::Output>> = tx.clone();
            let h = thread::Builder::new()
                .name(format!("mantis-stream-{i}"))
                .spawn(move || worker(&shared, &tx))?;
            pool.workers.push(h);
        }
        Ok(pool)
    }

    /// Queues a job.
    pub fn submit(&mut self, job: J, priority: Priority) -> JobId {
        let id = JobId(self.next_id);
        self.next_id = self.next_id.wrapping_add(1);
        let mut q = self.shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
        q.jobs.push((id, priority, job));
        drop(q);
        self.shared.ready.notify_one();
        id
    }

    /// Changes a queued job's priority. Returns false if it already started or was never queued.
    pub fn reprioritize(&self, id: JobId, priority: Priority) -> bool {
        let mut q = self.shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
        match q.jobs.iter_mut().find(|(j, _, _)| *j == id) {
            Some(entry) => {
                entry.1 = priority;
                true
            }
            None => false,
        }
    }

    /// Removes a queued job. Returns false if it already started or was never queued.
    pub fn cancel(&self, id: JobId) -> bool {
        let mut q = self.shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
        match q.jobs.iter().position(|(j, _, _)| *j == id) {
            Some(i) => {
                let _ = q.jobs.swap_remove(i);
                true
            }
            None => false,
        }
    }

    /// Jobs waiting for a worker.
    pub fn queued(&self) -> usize {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .jobs
            .len()
    }

    fn pull_finished(&mut self) {
        while let Ok(c) = self.done.try_recv() {
            self.pending.push_back(c);
        }
    }

    /// Hands off finished work until `budget` (measured on `clock`) is spent. Before each
    /// item after the first, `estimate` predicts its hand-off cost; an item predicted to
    /// overrun the budget waits for the next frame. Failed jobs are reported through
    /// `on_failed` and cost nothing against the budget.
    pub fn drain_budgeted(
        &mut self,
        clock: &dyn HostClock,
        budget: Duration,
        mut estimate: impl FnMut(&J::Output) -> Duration,
        mut handoff: impl FnMut(JobId, J::Output),
        mut on_failed: impl FnMut(JobId),
    ) -> HandoffReport {
        self.pull_finished();
        let start = clock.now();
        let mut report = HandoffReport::default();
        while let Some(next) = self.pending.front() {
            let spent = clock.now().saturating_since(start);
            if let Completed::Done { output, .. } = next
                && report.handed_off > 0
                && spent.saturating_add(estimate(output)) > budget
            {
                break;
            }
            match self.pending.pop_front() {
                Some(Completed::Done { id, output }) => {
                    handoff(id, output);
                    report.handed_off = report.handed_off.saturating_add(1);
                }
                Some(Completed::Failed { id }) => {
                    on_failed(id);
                    report.failed = report.failed.saturating_add(1);
                }
                None => break,
            }
            if clock.now().saturating_since(start) >= budget {
                break;
            }
        }
        report.elapsed = clock.now().saturating_since(start);
        report.remaining = !self.pending.is_empty();
        report
    }

    /// Finished items not yet handed off (after pulling from workers).
    pub fn finished(&mut self) -> usize {
        self.pull_finished();
        self.pending.len()
    }
}

fn worker<J: StreamJob>(shared: &Shared<J>, tx: &Sender<Completed<J::Output>>) {
    loop {
        let (id, job) = {
            let mut q = shared.queue.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                if q.stop {
                    return;
                }
                // Highest priority; ties go to the lowest id (submission order).
                let best = q
                    .jobs
                    .iter()
                    .enumerate()
                    .max_by(|(_, a), (_, b)| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
                    .map(|(i, _)| i);
                if let Some(i) = best {
                    let (id, _, job) = q.jobs.swap_remove(i);
                    break (id, job);
                }
                q = shared.ready.wait(q).unwrap_or_else(PoisonError::into_inner);
            }
        };
        let result = match catch_unwind(AssertUnwindSafe(move || job.run())) {
            Ok(output) => Completed::Done { id, output },
            Err(_) => Completed::Failed { id },
        };
        if tx.send(result).is_err() {
            return; // pool dropped
        }
    }
}

impl<J: StreamJob> Drop for StreamingPool<J> {
    fn drop(&mut self) {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stop = true;
        self.shared.ready.notify_all();
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}
