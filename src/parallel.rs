use crate::spatial::{DeferredTile, Features, Partitions};
use crate::text_index::IndexedMatches;
use crate::{Bounds, Error, Feature, FeatureType, PreparedQuery, Result, source::Source};
use rayon::{iter::Either, prelude::*};
use std::{
    io,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[cfg(all(feature = "mmap", target_os = "linux"))]
mod mapped;

const DEFAULT_PREFIX_STEPS: u64 = 4096;
const WORKER_BATCH_SIZE: usize = 1024;

fn default_prefix_steps<S: Source>(
    query: &PreparedQuery<'_, '_, S>,
    bounds: Option<Bounds>,
) -> u64 {
    if bounds.is_some() && query.library.source().prefers_concurrent_tile_reads() {
        0
    } else {
        DEFAULT_PREFIX_STEPS
    }
}

#[derive(Clone, Copy)]
enum ReductionPrefix {
    Fixed(u64),
    Adaptive(Duration),
}

fn step_budget(mut remaining: u64) -> impl FnMut() -> Result<bool> {
    move || {
        let proceed = remaining != 0;
        remaining = remaining.saturating_sub(1);
        Ok(proceed)
    }
}

fn map_partitions<'a, S: Source + Sync, T: Send>(
    partitions: Partitions<'a, S>,
    groups: Option<usize>,
    map: impl Fn(&mut Features<'a, S>, &[DeferredTile]) -> T + Send + Sync,
) -> impl ParallelIterator<Item = T> {
    match partitions {
        Partitions::Split(parts) => Either::Left(
            parts
                .into_par_iter()
                .map(move |mut features| map(&mut features, &[])),
        ),
        Partitions::Wide { template, tiles } => {
            // Keep residual work and tile groups in one indexed range so that
            // idle workers can steal work even when tile density varies.
            let first_tile = usize::from(template.has_remaining_work());
            let group_size = groups.map_or(1, |groups| tiles.len().div_ceil(groups).max(1));
            let len = tiles.len().div_ceil(group_size) + first_tile;
            Either::Right((0..len).into_par_iter().map_init(
                || None,
                move |scratch, index| {
                    if let Some(group) = index.checked_sub(first_tile) {
                        let cursor = scratch.get_or_insert_with(|| template.worker_cursor());
                        let start = group * group_size;
                        if groups.is_some() {
                            let end = (start + group_size).min(tiles.len());
                            map(cursor, &tiles[start..end])
                        } else {
                            cursor.defer_tile(tiles[start]);
                            map(cursor, &[])
                        }
                    } else {
                        let mut prefix = template.fork_remaining();
                        map(&mut prefix, &[])
                    }
                },
            ))
        }
    }
}

/// Reusable query workers with bounded, unordered result delivery.
///
/// Both spatial scans and text-index lookups deliver checked `Feature` handles.
/// At most two queued batches per worker, one batch per active worker, and one
/// consumer batch are retained. A worker batch holds at most 1,024 handles.
/// The serial prefix delivers each feature immediately without buffering. Tile-index
/// partitions use additional memory proportional to the intersecting tile count.
pub struct QueryExecutor {
    pool: rayon::ThreadPool,
    reduction_prefix: Option<ReductionPrefix>,
}
impl QueryExecutor {
    /// Create a reusable worker pool with the requested nonzero thread count.
    pub fn new(threads: usize) -> io::Result<Self> {
        if threads == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "zero workers"));
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            pool,
            reduction_prefix: None,
        })
    }
    /// Return the number of threads in this pool.
    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Set the serial traversal prefix for [`Self::try_fold_reduce`].
    ///
    /// The default is 4,096 steps for local reads. Bounded high-latency reads
    /// dispatch immediately by default. A shorter prefix can improve field-heavy
    /// reductions by starting workers sooner, but adds scheduling overhead to
    /// small queries. Zero dispatches eagerly; one-worker executors remain serial.
    /// This changes scheduling only: the library's total traversal limit still
    /// applies. Counts and lazy iteration use their own scheduling policies.
    #[must_use]
    pub fn with_reduction_prefix_steps(mut self, steps: u64) -> Self {
        self.reduction_prefix = Some(ReductionPrefix::Fixed(steps));
        self
    }

    /// Plan field-reduction scheduling from a bounded, reusable serial pilot.
    ///
    /// A serial pilot checks progress at 256, 512, 1,024 and 2,048 traversal
    /// steps, executing callbacks exactly once and retaining their results.
    /// Sufficiently costly pilots dispatch the remaining work; cheap or
    /// relation-bearing pilots, or pilots with fewer than two matches, continue
    /// to the usual 4,096-step prefix.
    /// The threshold is calibrated from this pool's dispatch cost when enabling
    /// this policy. Result order remains unspecified, as with fixed prefixes.
    /// Calling `with_reduction_prefix_steps` subsequently restores a fixed prefix.
    #[must_use]
    pub fn with_adaptive_reductions(mut self) -> Self {
        // Warm the pool before measuring dispatch; this does not read a library
        // or invoke application callbacks. Calibration is outside query execution.
        self.pool.install(|| ());
        let mut samples = [Duration::ZERO; 7];
        for sample in &mut samples {
            let start = Instant::now();
            self.pool.install(|| ());
            *sample = start.elapsed();
        }
        samples.sort_unstable();
        self.reduction_prefix = Some(ReductionPrefix::Adaptive(
            samples[3].saturating_mul(2).max(Duration::from_micros(10)),
        ));
        self
    }

    /// Calibrated pilot-time threshold, or `None` for fixed-prefix scheduling.
    /// Useful for diagnostics; enabling the adaptive policy recalibrates it.
    pub fn adaptive_reduction_budget(&self) -> Option<Duration> {
        match self.reduction_prefix {
            Some(ReductionPrefix::Adaptive(budget)) => Some(budget),
            _ => None,
        }
    }

    /// Run a one-shot count, building a temporary worker pool only when
    /// multiple partitions remain after the serial prefix.
    ///
    /// High-latency sources can request immediate tile dispatch for bounded
    /// queries to overlap independent reads. Local sources avoid pool startup
    /// overhead for short queries.
    pub fn count_once<S: Source + Sync>(
        threads: usize,
        query: &PreparedQuery<'_, '_, S>,
        bounds: Option<Bounds>,
    ) -> Result<u64> {
        let mut features = query.features(bounds);
        if threads <= 1 {
            return query.count_features(&mut features, || Ok(true));
        }
        let prefix_steps = default_prefix_steps(query, bounds);
        let prefix = query.count_features(&mut features, step_budget(prefix_steps))?;
        if features.finished() {
            return Ok(prefix);
        }
        let (partitions, index_steps) = features.partitions(threads.saturating_mul(4))?;
        let work_items = match &partitions {
            Partitions::Split(parts) => parts.len(),
            Partitions::Wide { template, tiles } => {
                tiles.len() + usize::from(template.has_remaining_work())
            }
        };
        let workers = threads.min(work_items.max(1));
        if workers <= 1 {
            let mut total = prefix;
            let mut limit_remaining = query.step_limit() - index_steps;
            let step = || {
                if limit_remaining == 0 {
                    return Err(Error::Limit("spatial traversal steps"));
                }
                limit_remaining -= 1;
                Ok(true)
            };
            match partitions {
                Partitions::Split(parts) => {
                    let mut step = step;
                    for mut part in parts {
                        total += query.count_features(&mut part, &mut step)?;
                    }
                }
                Partitions::Wide { template, tiles } => {
                    let mut step = step;
                    if template.has_remaining_work() {
                        let mut rem = template.fork_remaining();
                        total += query.count_features(&mut rem, &mut step)?;
                    }
                    let mut cursor = template.worker_cursor();
                    for tile in tiles {
                        cursor.defer_tile(tile);
                        total += query.count_features(&mut cursor, &mut step)?;
                    }
                }
            }
            return Ok(total);
        }
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .build()
            .map_err(|err| Error::Io(io::Error::other(err)))?;
        let budget = Budget::new(query.step_limit() - index_steps);
        let suffix = pool.install(|| {
            map_partitions(partitions, None, |features, _| {
                if budget.cancelled.load(Ordering::Relaxed) {
                    return Ok(0u64);
                }
                let mut lease = budget.lease();
                let result = query.count_features(features, || lease.step());
                if result.is_err() {
                    budget.cancel();
                }
                result
            })
            .try_reduce(|| 0u64, |left, right| Ok(left + right))
        })?;
        Ok(prefix + suffix)
    }

    /// Count matching features using worker-local reductions.
    ///
    /// This eagerly consumes the complete query. Each worker keeps a scalar
    /// count; individual result handles are not queued or sent to the caller.
    /// Validation and the shared traversal-step limit match ordinary execution.
    pub fn count<S: Source + Sync>(
        &self,
        query: &PreparedQuery<'_, '_, S>,
        bounds: Option<Bounds>,
    ) -> Result<u64> {
        let mut features = query.features(bounds);
        if self.threads() == 1 {
            return query.count_features(&mut features, || Ok(true));
        }
        let prefix_steps = default_prefix_steps(query, bounds);
        let prefix = query.count_features(&mut features, step_budget(prefix_steps))?;
        if features.finished() {
            return Ok(prefix);
        }
        let (partitions, index_steps) = features.partitions(self.threads().saturating_mul(4))?;
        let budget = Budget::new(query.step_limit() - index_steps);
        let suffix = self.pool.install(|| {
            map_partitions(partitions, None, |features, _| {
                if budget.cancelled.load(Ordering::Relaxed) {
                    return Ok(0u64);
                }
                let mut lease = budget.lease();
                let result = query.count_features(features, || lease.step());
                if result.is_err() {
                    budget.cancel();
                }
                result
            })
            .try_reduce(|| 0u64, |left, right| Ok(left + right))
        })?;
        Ok(prefix + suffix)
    }

    /// Reduce fields on workers without queueing individual feature handles.
    ///
    /// `identity` may be called multiple times, including for empty partitions.
    /// `reduce` must be associative with that identity; result order is unspecified.
    /// Each accumulator belongs to one worker at a time, so it need not be `Sync`.
    /// Bounded high-latency reads dispatch immediately. Otherwise the default
    /// serial prefix is 4,096 steps. [`Self::with_reduction_prefix_steps`] and
    /// [`Self::with_adaptive_reductions`] override either default.
    ///
    /// Traversal, callback and reduction errors discard the answer and cancel
    /// outstanding work. All workers join before returning. Concurrent callbacks
    /// may already have run; their side effects are not rolled back. Cancellation
    /// cannot interrupt a callback or blocking source read. Unlike `with_query`,
    /// this eagerly consumes the complete query and has no early-result limit.
    pub fn try_fold_reduce<'a, S: Source + Sync, T: Send>(
        &self,
        query: &PreparedQuery<'a, '_, S>,
        bounds: Option<Bounds>,
        identity: impl Fn() -> T + Sync,
        fold: impl Fn(T, Feature<'a, S>) -> Result<T> + Sync,
        reduce: impl Fn(T, T) -> Result<T> + Sync,
    ) -> Result<T> {
        let policy = self
            .reduction_prefix
            .unwrap_or_else(|| ReductionPrefix::Fixed(default_prefix_steps(query, bounds)));
        self.reduce(query, bounds, policy, identity, fold, reduce)
    }

    fn reduce<'a, S: Source + Sync, T: Send>(
        &self,
        query: &PreparedQuery<'a, '_, S>,
        bounds: Option<Bounds>,
        policy: ReductionPrefix,
        identity: impl Fn() -> T + Sync,
        fold: impl Fn(T, Feature<'a, S>) -> Result<T> + Sync,
        reduce: impl Fn(T, T) -> Result<T> + Sync,
    ) -> Result<T> {
        let mut features = query.features(bounds);
        if self.threads() == 1 {
            return query.fold_in(&mut features, || Ok(true), identity(), fold);
        }
        let mut prefix = identity();
        match policy {
            ReductionPrefix::Adaptive(budget) => {
                let start = Instant::now();
                let mut matches = 0u64;
                let mut relations = false;
                let mut consumed = 0;
                for checkpoint in [256, 512, 1024, 2048, DEFAULT_PREFIX_STEPS] {
                    prefix = query.fold_in(
                        &mut features,
                        step_budget(checkpoint - consumed),
                        prefix,
                        |acc, feature| {
                            matches += 1;
                            relations |= feature.kind() == FeatureType::Relation;
                            fold(acc, feature)
                        },
                    )?;
                    if features.finished() {
                        return Ok(prefix);
                    }
                    // Require evidence of useful matches before scheduling. A few
                    // relations can contain enormous geometries; their prefix cost
                    // poorly predicts remaining work.
                    if matches >= 2 && !relations && start.elapsed() >= budget {
                        break;
                    }
                    consumed = checkpoint;
                }
            }
            ReductionPrefix::Fixed(steps) => {
                prefix = query.fold_in(&mut features, step_budget(steps), prefix, &fold)?;
                if features.finished() {
                    return Ok(prefix);
                }
            }
        }
        let (partitions, index_steps) = features.partitions(self.threads().saturating_mul(4))?;
        let budget = Budget::new(query.step_limit() - index_steps);
        let suffix = self.pool.install(|| {
            map_partitions(partitions, None, |features, _| {
                if budget.cancelled.load(Ordering::Relaxed) {
                    return Ok(identity());
                }
                let mut lease = budget.lease();
                let result = query.fold_in(features, || lease.step(), identity(), &fold);
                if result.is_err() {
                    budget.cancel();
                }
                result
            })
            .try_reduce(&identity, |left, right| {
                let result = reduce(left, right);
                if result.is_err() {
                    budget.cancel();
                }
                result
            })
        })?;
        reduce(prefix, suffix)
    }

    /// Consume results on the calling thread while workers search distinct tiles.
    /// Iteration starts lazily on the calling thread and delivers each match
    /// immediately. The serial prefix allows up to 4,096 traversal steps.
    /// After eight matches, at most 512 more steps are spent serially.
    /// Queries that outlive the prefix resume on the pool. For a high-latency
    /// source with a bounding box smaller than the world and multiple workers,
    /// the first call to `next` dispatches whole tiles. An unused stream does
    /// no traversal. A single worker can traverse such a source serially.
    /// On Linux, workers scanning an entire immutable memory-mapped library
    /// begin reading ahead within their tiles after observing major page faults.
    ///
    /// Result order is unspecified. Consume errors exactly as with a serial
    /// iterator. Returning early disconnects delivery and joins outstanding work;
    /// no worker outlives this call or its borrowed library/query. The outer error
    /// is retained for API compatibility; traversal/partitioning failures are
    /// iterator items. Workers check cancellation at each traversal step on
    /// high-latency sources and at most every 256 steps otherwise. An in-flight
    /// blocking source read can delay the join.
    pub fn with_query<'a, S: Source + Sync, R>(
        &self,
        query: &PreparedQuery<'a, '_, S>,
        bounds: Option<Bounds>,
        consume: impl FnOnce(&mut QueryResults<'a, '_, S>) -> R,
    ) -> Result<R> {
        let remote = query.library.source().prefers_concurrent_tile_reads();
        if remote && self.threads() == 1 {
            let mut items = query.execute(bounds);
            return Ok(consume(&mut QueryResults::new(move || {
                items
                    .next()
                    .map_or(Ok(Refill::End), |item| item.map(Refill::Feature))
            })));
        }
        let prefix = if remote && bounds.is_some_and(|b| b != Bounds::WORLD) {
            0
        } else {
            DEFAULT_PREFIX_STEPS
        };
        self.with_prefix(query, bounds, prefix, consume)
    }
    fn with_indexed_matches<'a, S: Source + Sync, R>(
        &self,
        mut matches: IndexedMatches<'a, S>,
        consume: impl FnOnce(&mut QueryResults<'a, '_, S>) -> R,
    ) -> R {
        if self.threads() == 1 || matches.remaining() <= 4 * WORKER_BATCH_SIZE {
            let refill = move || {
                matches
                    .next()
                    .map_or(Ok(Refill::End), |result| result.map(Refill::Feature))
            };
            return consume(&mut QueryResults::new(refill));
        }

        let queue_capacity = self.threads().saturating_mul(2);
        self.pool.in_place_scope(|scope| {
            let mut indexed = Some(matches);
            let mut first = true;
            let refill = move || {
                if first {
                    first = false;
                    return indexed
                        .as_mut()
                        .and_then(Iterator::next)
                        .map_or(Ok(Refill::End), |result| result.map(Refill::Feature));
                }

                let matches = indexed.take().expect("indexed refill dispatches once");
                let (sender, receiver) = mpsc::sync_channel(queue_capacity);
                // A completed index already charged the spatial and tag budgets.
                let budget = Arc::new(Budget::new(0));
                let worker_budget = Arc::clone(&budget);
                scope.spawn(move |_| {
                    let result = matches.par_chunks(WORKER_BATCH_SIZE).try_for_each(
                        |matches| -> Result<()> {
                            if worker_budget.cancelled.load(Ordering::Relaxed) {
                                return Ok(());
                            }
                            let mut batch = Vec::with_capacity(matches.remaining());
                            for (i, matched) in matches.enumerate() {
                                if i % 256 == 255 && worker_budget.cancelled.load(Ordering::Relaxed)
                                {
                                    return Ok(());
                                }
                                batch.push(matched?);
                            }
                            if sender.send(Ok(batch)).is_err() {
                                worker_budget.cancel();
                            }
                            Ok(())
                        },
                    );
                    if let Err(error) = result {
                        let _ = sender.send(Err(error));
                    }
                });
                Ok(Refill::Parallel { receiver, budget })
            };
            consume(&mut QueryResults::new(refill))
        })
    }

    fn with_prefix<'a, S: Source + Sync, R>(
        &self,
        query: &PreparedQuery<'a, '_, S>,
        bounds: Option<Bounds>,
        prefix_steps: u64,
        consume: impl FnOnce(&mut QueryResults<'a, '_, S>) -> R,
    ) -> Result<R> {
        if let Some(matches) = query.indexed_matches(bounds) {
            return Ok(self.with_indexed_matches(matches, consume));
        }
        let observation = query
            .scan_observation(bounds)
            .map(|observation| ParallelObservation {
                seen: 0,
                total_limit: query.step_limit(),
                finish: Box::new(move |steps, seen| observation.finish(steps, seen)),
            });
        let observation_enabled = observation.is_some();
        let target = self.threads().saturating_mul(4);
        let queue_capacity = self.threads().saturating_mul(2);
        Ok(self.pool.in_place_scope(|scope| {
            let mut features = None;
            let mut remaining = prefix_steps;
            let mut yielded = 0usize;
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            let mut prefix_faults = None;
            let refill = move || -> Result<Refill<'a, S>> {
                // No I/O, traversal or worker dispatch occurs until next().
                let cursor = features.get_or_insert_with(|| {
                    let cursor = query.features(bounds);
                    #[cfg(all(feature = "mmap", target_os = "linux"))]
                    {
                        prefix_faults = cursor.partition_fault_sample();
                    }
                    cursor
                });
                if let Some(feature) = query.next_in(cursor, || {
                    let proceed = remaining != 0;
                    remaining = remaining.saturating_sub(1);
                    Ok(proceed)
                }) {
                    yielded += 1;
                    if yielded == 8 {
                        remaining = remaining.min(512);
                    }
                    return feature.map(Refill::Feature);
                }
                if cursor.finished() {
                    return Ok(Refill::End);
                }
                let cursor = features.take().expect("initialized traversal");
                let poll_each_step = query.library.source().prefers_concurrent_tile_reads();
                let (partitions, index_steps) = cursor.partitions(target)?;
                #[cfg(all(feature = "mmap", target_os = "linux"))]
                let read_ahead = prefix_faults
                    .take()
                    .map(|s| AtomicBool::new(s.observed_stall()));
                let budget = Arc::new(Budget::new(query.step_limit() - index_steps));
                let (sender, receiver) = mpsc::sync_channel(queue_capacity);
                let worker_budget = Arc::clone(&budget);
                scope.spawn(move |_| {
                    let first_sent = AtomicBool::new(yielded != 0);
                    let queued = AtomicUsize::new(yielded);
                    let send_batch = |batch: Vec<Feature<'a, S>>| {
                        let count = batch.len();
                        if sender.send(Ok(batch)).is_err() {
                            return false;
                        }
                        if queued.load(Ordering::Relaxed) < WORKER_BATCH_SIZE {
                            queued.fetch_add(count, Ordering::Relaxed);
                        }
                        true
                    };
                    let scan_partition =
                        |features: &mut Features<'a, S>, tiles: &[DeferredTile]| -> Result<()> {
                            let mut lease = worker_budget.lease();
                            let mut batch = Vec::new();
                            let mut initial_capacity = 32;
                            let live = || !worker_budget.cancelled.load(Ordering::Relaxed);
                            let mut step = || {
                                if poll_each_step && !live() {
                                    Ok(false)
                                } else {
                                    lease.step()
                                }
                            };
                            #[cfg(all(feature = "mmap", target_os = "linux"))]
                            let mut probe = read_ahead
                                .as_ref()
                                .filter(|_| live())
                                .map(mapped::PartitionProbe::new);
                            let mut scan = |features: &mut Features<'a, S>| -> Result<bool> {
                                // Skip prepaid tile-header reads after cancellation.
                                if !live() {
                                    return Ok(false);
                                }
                                #[cfg(all(feature = "mmap", target_os = "linux"))]
                                if probe
                                    .as_mut()
                                    .is_some_and(mapped::PartitionProbe::before_tile)
                                {
                                    features.hint_pending_tile(&live);
                                }
                                // At most one initial single-feature batch makes the
                                // first match visible without waiting for a full batch.
                                // Check once per partition, outside the hot result loop.
                                if !first_sent.load(Ordering::Relaxed) {
                                    if let Some(feature) = query.next_in(features, &mut step) {
                                        let feature = feature?;
                                        if !first_sent.swap(true, Ordering::Relaxed) {
                                            if !send_batch(vec![feature]) {
                                                return Ok(false);
                                            }
                                        } else {
                                            batch.push(feature);
                                        }
                                    }
                                }
                                // Many tiles produce just a few matches. Start with a
                                // small allocation and grow to full batches as needed.
                                let disconnected = query
                                    .scan_in(features, &mut step, |feature| {
                                        if batch.is_empty() {
                                            batch.reserve_exact(initial_capacity);
                                        }
                                        batch.push(feature);
                                        if batch.len() < WORKER_BATCH_SIZE {
                                            return Ok(false);
                                        }
                                        initial_capacity = WORKER_BATCH_SIZE;
                                        Ok(!send_batch(std::mem::take(&mut batch)))
                                    })
                                    .transpose()?;
                                if disconnected.is_some() {
                                    return Ok(false);
                                }
                                // Make the early results available promptly, even
                                // when an individual tile has only a few matches.
                                if queued.load(Ordering::Relaxed) < WORKER_BATCH_SIZE
                                    && !batch.is_empty()
                                {
                                    return Ok(send_batch(std::mem::take(&mut batch)));
                                }
                                Ok(true)
                            };
                            if tiles.is_empty() {
                                scan(features)?;
                            } else {
                                for &tile in tiles {
                                    features.defer_tile(tile);
                                    if !scan(features)? {
                                        break;
                                    }
                                }
                            }
                            #[cfg(all(feature = "mmap", target_os = "linux"))]
                            if let Some(probe) = probe {
                                probe.finish();
                            }
                            if !batch.is_empty() {
                                let _ = send_batch(batch);
                            }
                            Ok(())
                        };
                    let result =
                        map_partitions(partitions, Some(target.saturating_mul(4)), scan_partition)
                            .try_for_each(|result| result.inspect_err(|_| worker_budget.cancel()));
                    if let Err(error) = result {
                        let _ = sender.send(Err(error));
                    } else if observation_enabled {
                        worker_budget.completed.store(true, Ordering::Release);
                    }
                    drop(sender);
                });
                Ok(Refill::Parallel { receiver, budget })
            };
            let mut results = QueryResults::new(refill);
            results.observation = observation;
            let result = consume(&mut results);
            drop(results);
            result
        }))
    }
}

enum Refill<'a, S> {
    Feature(Feature<'a, S>),
    Parallel {
        receiver: mpsc::Receiver<Result<Vec<Feature<'a, S>>>>,
        budget: Arc<Budget>,
    },
    End,
}
type Refiller<'a, 'q, S> = Box<dyn FnMut() -> Result<Refill<'a, S>> + Send + 'q>;

struct ParallelObservation<'q> {
    seen: u8,
    total_limit: u64,
    finish: Box<dyn FnOnce(u64, u8) + Send + 'q>,
}

/// Lazy unordered results, scoped to a `QueryExecutor::with_query` call.
/// The second lifetime covers the prepared query and scoped worker launcher.
pub struct QueryResults<'a, 'q, S> {
    receiver: Option<mpsc::Receiver<Result<Vec<Feature<'a, S>>>>>,
    current: std::vec::IntoIter<Feature<'a, S>>,
    refill: Option<Refiller<'a, 'q, S>>,
    budget: Option<Arc<Budget>>,
    observation: Option<ParallelObservation<'q>>,
    done: bool,
}
impl<'a, 'q, S> QueryResults<'a, 'q, S> {
    fn new(refill: impl FnMut() -> Result<Refill<'a, S>> + Send + 'q) -> Self {
        Self {
            receiver: None,
            current: Vec::new().into_iter(),
            refill: Some(Box::new(refill)),
            budget: None,
            observation: None,
            done: false,
        }
    }
}
impl<'a, S> Iterator for QueryResults<'a, '_, S> {
    type Item = Result<Feature<'a, S>>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // End/error is only observed with an exhausted batch. Keep its flag
        // off the hot path while ordinary checked handles remain buffered.
        if let Some(feature) = self.current.next() {
            return Some(Ok(feature));
        }
        if self.done {
            return None;
        }
        self.refill_next().transpose()
    }
}
impl<'a, S> QueryResults<'a, '_, S> {
    // Keep the branch-heavy lazy/receive path out of the per-feature hot loop.
    #[inline(never)]
    fn refill_next(&mut self) -> Result<Option<Feature<'a, S>>> {
        loop {
            if let Some(feature) = self.current.next() {
                return Ok(Some(feature));
            }
            if let Some(receiver) = &self.receiver {
                match receiver.recv() {
                    Ok(Ok(batch)) => {
                        if let Some(observer) = &mut self.observation {
                            observer.seen = (usize::from(observer.seen) + batch.len()).min(2) as u8;
                        }
                        self.current = batch.into_iter();
                    }
                    Ok(Err(error)) => {
                        self.done = true;
                        if let Some(budget) = &self.budget {
                            budget.cancel();
                        }
                        return Err(error);
                    }
                    Err(_) => {
                        self.done = true;
                        if let (Some(observer), Some(budget)) =
                            (self.observation.take(), &self.budget)
                        {
                            if let Some(steps) = budget.completed_steps(observer.total_limit) {
                                (observer.finish)(steps, observer.seen);
                            }
                        }
                        return Ok(None);
                    }
                }
                continue;
            }
            let result = self.refill.as_mut().expect("active result stream")();
            match result {
                Ok(Refill::Feature(feature)) => {
                    if let Some(observer) = &mut self.observation {
                        observer.seen = (observer.seen + 1).min(2);
                    }
                    return Ok(Some(feature));
                }
                Ok(Refill::Parallel { receiver, budget }) => {
                    self.receiver = Some(receiver);
                    self.budget = Some(budget);
                    self.refill = None;
                }
                Ok(Refill::End) => {
                    self.done = true;
                    self.refill = None;
                    return Ok(None);
                }
                Err(error) => {
                    self.done = true;
                    self.refill = None;
                    return Err(error);
                }
            }
        }
    }
}
impl<S> std::iter::FusedIterator for QueryResults<'_, '_, S> {}
impl<S> Drop for QueryResults<'_, '_, S> {
    fn drop(&mut self) {
        if let Some(budget) = &self.budget {
            budget.cancel();
        }
        // Receiver drops immediately afterwards, releasing blocked senders.
        // The enclosing Rayon scope then joins all outstanding workers.
    }
}

// Reserve steps before executing them. Workers amortize synchronization over
// 4,096 steps and return unused reservations. When the limit is near, wait for
// outstanding reservations instead of incorrectly failing a query that fits.
// The sum of executed and currently reserved steps never exceeds the limit.
struct Budget {
    state: Mutex<BudgetState>,
    changed: Condvar,
    cancelled: AtomicBool,
    completed: AtomicBool,
}
struct BudgetState {
    available: u64,
    reserved: u64,
}
impl Budget {
    fn new(limit: u64) -> Self {
        Self {
            state: Mutex::new(BudgetState {
                available: limit,
                reserved: 0,
            }),
            changed: Condvar::new(),
            cancelled: AtomicBool::new(false),
            completed: AtomicBool::new(false),
        }
    }
    fn completed_steps(&self, total_limit: u64) -> Option<u64> {
        if !self.completed.load(Ordering::Acquire) {
            return None;
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        debug_assert_eq!(state.reserved, 0);
        Some(total_limit - state.available)
    }
    fn cancel(&self) {
        // Synchronize notification with waiters so cancellation cannot miss a
        // waiter between its condition check and Condvar::wait.
        let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.cancelled.store(true, Ordering::Relaxed);
        self.changed.notify_all();
    }
    fn lease(&self) -> Lease<'_> {
        Lease {
            budget: self,
            grant: 0,
            remaining: 0,
        }
    }
}
struct Lease<'a> {
    budget: &'a Budget,
    grant: u64,
    remaining: u64,
}
impl Lease<'_> {
    fn release(&mut self) {
        if self.grant != 0 {
            let mut state = self.budget.state.lock().unwrap_or_else(|e| e.into_inner());
            state.reserved -= self.grant;
            state.available += self.remaining;
            self.grant = 0;
            self.remaining = 0;
            self.budget.changed.notify_all();
        }
    }
    #[inline]
    fn step(&mut self) -> Result<bool> {
        // Poll cancellation every 256 steps, including when the lease expires.
        if self.remaining & 255 == 0 && !self.step_boundary()? {
            return Ok(false);
        }
        self.remaining -= 1;
        Ok(true)
    }

    #[cold]
    #[inline(never)]
    fn step_boundary(&mut self) -> Result<bool> {
        if self.budget.cancelled.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if self.remaining != 0 {
            return Ok(true);
        }
        // Refill and settle the exhausted reservation under a single lock.
        let mut state = self.budget.state.lock().unwrap_or_else(|e| e.into_inner());
        state.reserved -= self.grant;
        self.grant = 0;
        while state.available == 0 {
            if self.budget.cancelled.load(Ordering::Relaxed) {
                return Ok(false);
            }
            if state.reserved == 0 {
                self.budget.changed.notify_all();
                return Err(Error::Limit("spatial traversal steps"));
            }
            state = self
                .budget
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        if self.budget.cancelled.load(Ordering::Relaxed) {
            return Ok(false);
        }
        self.grant = state.available.min(4096);
        self.remaining = self.grant;
        state.available -= self.grant;
        state.reserved += self.grant;
        Ok(true)
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests;
