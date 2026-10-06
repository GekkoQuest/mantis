//! Parallel pose evaluation on worker threads (plan 8.4).

use crate::instance::AnimInstance;

/// Updates every instance by `dt`, splitting the slice into at most `workers` contiguous
/// chunks evaluated on scoped threads (the calling thread takes the first chunk). Each
/// instance's update depends only on its own state and the shared immutable graph, so
/// the result is bit-identical to updating the instances one by one in order.
///
/// `workers` of 0 or 1, or a single instance, runs serially on the calling thread.
/// Spawning threads allocates; the per-instance updates do not.
pub fn evaluate_batch(instances: &mut [AnimInstance], dt: f32, workers: usize) {
    let workers = workers.clamp(1, instances.len().max(1));
    if workers == 1 {
        for instance in instances.iter_mut() {
            instance.update(dt);
        }
        return;
    }
    let chunk = instances.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let mut chunks = instances.chunks_mut(chunk);
        let first = chunks.next();
        for rest in chunks {
            scope.spawn(move || {
                for instance in rest {
                    instance.update(dt);
                }
            });
        }
        for instance in first.into_iter().flatten() {
            instance.update(dt);
        }
    });
}
