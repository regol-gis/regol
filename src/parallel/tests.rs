use super::*;
use crate::{Library, Limits, Query};

// Use one worker pipeline per source type in the test binary. Compiling a
// pipeline per closure would exceed the CI runner's memory budget.
type TestConsumer<'a, 'f, S> = Box<dyn FnOnce(&mut QueryResults<'a, '_, S>) + 'f>;
impl QueryExecutor {
    fn inspect_results<'a, S: Source + Sync, R>(
        &self,
        query: &PreparedQuery<'a, '_, S>,
        bounds: Option<Bounds>,
        prefix: Option<u64>,
        inspect: impl FnOnce(&mut QueryResults<'a, '_, S>) -> R,
    ) -> Result<R> {
        let mut result = None;
        let visit: TestConsumer<'a, '_, S> = Box::new(|rows| result = Some(inspect(rows)));
        match prefix {
            Some(steps) => self.with_prefix(query, bounds, steps, visit)?,
            None => self.with_query(query, bounds, visit)?,
        }
        Ok(result.expect("executor invokes its consumer"))
    }
}

struct Observed<'a> {
    bytes: &'a [u8],
    reads: std::sync::atomic::AtomicUsize,
    fault: std::sync::atomic::AtomicU64,
}
impl Source for Observed<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if offset == self.fault.load(Ordering::Relaxed) {
            return Err(io::Error::other("injected record failure"));
        }
        self.bytes.read_exact_at(offset, out)
    }
}
fn observed(bytes: &[u8]) -> Observed<'_> {
    Observed {
        bytes,
        reads: 0.into(),
        fault: u64::MAX.into(),
    }
}

#[test]
fn scoped_consumer_may_move_iteration_to_another_thread() -> Result<()> {
    let library = Library::new(include_bytes!("../../tests/fixtures/wide_exports.gol").as_slice())?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let executor = QueryExecutor::new(4)?;
    let count = executor.inspect_results(&prepared, None, None, |results| {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    results.try_fold(0usize, |count, feature| feature.map(|_| count + 1))
                })
                .join()
                .unwrap()
        })
    })??;
    assert_eq!(count, prepared.iter().count());
    Ok(())
}

#[test]
fn unused_results_do_no_io_and_first_result_does_not_prefetch() -> Result<()> {
    let library = Library::new(observed(include_bytes!(
        "../../tests/fixtures/wide_exports.gol"
    )))?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let executor = QueryExecutor::new(4)?;
    library.source().reads.store(0, Ordering::Relaxed);
    executor.inspect_results(&prepared, None, None, |_| ())?;
    assert_eq!(library.source().reads.load(Ordering::Relaxed), 0);
    let expected = prepared.iter().next().unwrap()?.id();
    let expected_reads = library.source().reads.swap(0, Ordering::Relaxed);
    let first =
        executor.inspect_results(&prepared, None, None, |results| results.next().unwrap())??;
    assert_eq!(first.id(), expected);
    assert_eq!(
        library.source().reads.load(Ordering::Relaxed),
        expected_reads
    );
    Ok(())
}

#[test]
fn count_reduction_preserves_limits_after_parallel_handoff() -> Result<()> {
    let bytes = include_bytes!("../../tests/fixtures/wide_exports.gol").as_slice();
    let query = Query::all();
    let serial = |limit| -> Result<u64> {
        let library = Library::with_limits(
            bytes,
            Limits {
                max_traversal_steps: limit,
                ..Limits::default()
            },
        )?;
        query
            .prepare(&library)?
            .iter()
            .try_fold(0u64, |n, f| f.map(|_| n + 1))
    };
    let mut low = 0;
    let mut high = 1_000_000;
    let expected = serial(high)?;
    assert!(expected > 10000);
    while low < high {
        let mid = low + (high - low) / 2;
        if serial(mid).is_ok() {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    let executor = QueryExecutor::new(4)?;
    for limit in [
        255,
        256,
        257,
        511,
        512,
        513,
        1023,
        1024,
        1025,
        2047,
        2048,
        2049,
        4095,
        4096,
        4097,
        8191,
        8192,
        8193,
        low - 1,
        low,
        low + 1,
    ] {
        let library = Library::with_limits(
            bytes,
            Limits {
                max_traversal_steps: limit,
                ..Limits::default()
            },
        )?;
        let prepared = query.prepare(&library)?;
        for prefix in [0, 1, 256, u64::MAX] {
            let configured = QueryExecutor::new(4)?.with_reduction_prefix_steps(prefix);
            let result = configured.try_fold_reduce(
                &prepared,
                None,
                || 0u64,
                |n, _| Ok(n + 1),
                |a, b| Ok(a + b),
            );
            assert_eq!(
                result.is_ok(),
                limit >= low,
                "prefix={prefix} limit={limit} minimum={low}"
            );
            if let Ok(count) = result {
                assert_eq!(count, expected);
            }
        }
        for budget in [Duration::ZERO, Duration::MAX] {
            let mut adaptive = QueryExecutor::new(4)?;
            adaptive.reduction_prefix = Some(ReductionPrefix::Adaptive(budget));
            let result = adaptive.try_fold_reduce(
                &prepared,
                None,
                || 0u64,
                |n, _| Ok(n + 1),
                |a, b| Ok(a + b),
            );
            assert_eq!(
                result.is_ok(),
                limit >= low,
                "adaptive budget={budget:?} limit={limit} minimum={low}"
            );
            if let Ok(count) = result {
                assert_eq!(count, expected);
            }
        }
        for result in [
            prepared.count(),
            executor.count(&prepared, None),
            prepared.try_fold(0u64, |n, _| Ok(n + 1)),
            executor.try_fold_reduce(&prepared, None, || 0u64, |n, _| Ok(n + 1), |a, b| Ok(a + b)),
        ] {
            assert_eq!(result.is_ok(), limit >= low, "limit={limit} minimum={low}");
            if let Ok(count) = result {
                assert_eq!(count, expected);
            }
        }
    }
    Ok(())
}

#[test]
fn count_worker_errors_cancel_reductions_and_allow_pool_reuse() -> Result<()> {
    let library = Library::new(observed(include_bytes!(
        "../../tests/fixtures/wide_exports.gol"
    )))?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    // Beyond the serial prefix, so the injected read fails on a worker.
    let late = prepared.iter().nth(10000).expect("large fixture")?;
    let expected = prepared.count()?;
    let executor = QueryExecutor::new(4)?;
    library
        .source()
        .fault
        .store(late.anchor(), Ordering::Relaxed);
    assert!(executor.count(&prepared, None).is_err());
    assert!(
        executor
            .try_fold_reduce(&prepared, None, || 0u64, |n, _| Ok(n + 1), |a, b| Ok(a + b))
            .is_err()
    );
    library.source().fault.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(executor.count(&prepared, None)?, expected);
    Ok(())
}

#[test]
fn worker_record_error_fuses_results_and_pool_can_be_reused() -> Result<()> {
    let library = Library::new(observed(include_bytes!(
        "../../tests/fixtures/wide_exports.gol"
    )))?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let first = prepared.iter().next().unwrap()?;
    let expected_count = prepared.iter().count();
    library
        .source()
        .fault
        .store(first.anchor(), Ordering::Relaxed);
    let executor = QueryExecutor::new(4)?;
    let result = executor.inspect_results(&prepared, None, Some(0), |results| {
        let result = results.by_ref().collect::<Result<Vec<_>>>();
        assert!(results.next().is_none());
        result
    })?;
    assert!(result.is_err());
    library.source().fault.store(u64::MAX, Ordering::Relaxed);
    let count = executor.inspect_results(&prepared, None, Some(0), |results| {
        results.try_fold(0usize, |count, feature| feature.map(|_| count + 1))
    })??;
    assert_eq!(count, expected_count);
    Ok(())
}

#[test]
fn cancellation_bounds_extra_steps_and_releases_budget_waiters() -> Result<()> {
    for consumed in [0, 1, 255, 256, 257, 4095, 4096] {
        let budget = Budget::new(10000);
        let mut lease = budget.lease();
        for _ in 0..consumed {
            assert!(lease.step()?);
        }
        budget.cancel();
        let mut extra = 0;
        while lease.step()? {
            extra += 1;
            assert!(extra <= 255);
        }
    }
    let budget = Budget::new(1);
    let mut holder = budget.lease();
    assert!(holder.step()?);
    let (entered, ready) = mpsc::channel();
    let (finished, result) = mpsc::channel();
    let outcome = std::thread::scope(|scope| {
        scope.spawn(|| {
            entered.send(()).unwrap();
            finished.send(budget.lease().step()).unwrap();
        });
        ready.recv().unwrap();
        budget.cancel();
        let outcome = result.recv_timeout(std::time::Duration::from_secs(2));
        // Release even on failure so a failing test cannot strand a thread.
        drop(holder);
        outcome
    });
    assert!(!outcome.expect("cancel must wake reserved-budget waiters")?);
    Ok(())
}

#[test]
fn serial_prefix_handoffs_preserve_results_and_exact_budget() -> Result<()> {
    let bytes = include_bytes!("../../tests/fixtures/tiled_global.gol").as_slice();
    for workers in [1, 4] {
        let executor = QueryExecutor::new(workers)?;
        for text in ["*", "*[name]", "*[!name]"] {
            let query = Query::parse(text)?;
            let library = Library::new(bytes)?;
            let prepared = query.prepare(&library)?;
            let mut expected = prepared
                .iter()
                .map(|f| f.map(|f| f.id()))
                .collect::<Result<Vec<_>>>()?;
            expected.sort_unstable();
            for prefix in 0..160 {
                let mut actual =
                    executor.inspect_results(&prepared, None, Some(prefix), |results| {
                        results
                            .map(|f| f.map(|f| f.id()))
                            .collect::<Result<Vec<_>>>()
                    })??;
                actual.sort_unstable();
                assert_eq!(
                    actual, expected,
                    "workers={workers} query={text} prefix={prefix}"
                );
            }
        }
        let query = Query::all();
        for limit in 0..200 {
            let library = Library::with_limits(
                bytes,
                Limits {
                    max_traversal_steps: limit,
                    ..Limits::default()
                },
            )?;
            let prepared = query.prepare(&library)?;
            let expected = prepared.iter().collect::<Result<Vec<_>>>();
            for prefix in [0, 1, 31, 4096] {
                let actual = executor
                    .inspect_results(&prepared, None, Some(prefix), |results| {
                        results.collect::<Result<Vec<_>>>()
                    })
                    .and_then(|result| result);
                assert_eq!(
                    actual.is_ok(),
                    expected.is_ok(),
                    "workers={workers} limit={limit} prefix={prefix}"
                );
                if let (Ok(actual), Ok(expected)) = (actual, &expected) {
                    assert_eq!(actual.len(), expected.len());
                }
            }
        }
    }
    Ok(())
}
#[test]
fn deferred_headers_respect_cancellation_and_the_pool_remains_usable() -> Result<()> {
    struct DelayedHeaders<'a> {
        bytes: &'a [u8],
        cancelled: AtomicBool,
        late_headers: std::sync::atomic::AtomicUsize,
        headers: std::sync::atomic::AtomicUsize,
    }
    impl Source for DelayedHeaders<'_> {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            // Payloads begin on 4 KiB boundaries. Slow these reads so the
            // consumer can cancel while other partitions are still pending.
            if offset % 4096 == 0 && out.len() == 24 {
                self.headers.fetch_add(1, Ordering::SeqCst);
                if self.cancelled.load(Ordering::SeqCst) {
                    self.late_headers.fetch_add(1, Ordering::SeqCst);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            self.bytes.read_exact_at(offset, out)
        }
    }
    let bytes = include_bytes!("../../tests/fixtures/tiled_global.gol").as_slice();
    let library = Library::new(DelayedHeaders {
        bytes,
        cancelled: AtomicBool::new(false),
        late_headers: std::sync::atomic::AtomicUsize::new(0),
        headers: std::sync::atomic::AtomicUsize::new(0),
    })?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let expected = prepared.iter().collect::<Result<Vec<_>>>()?.len();
    for workers in [1, 2] {
        let executor = QueryExecutor::new(workers)?;
        let before = library.source().headers.load(Ordering::SeqCst);
        executor.inspect_results(&prepared, None, Some(0), |results| -> Result<()> {
            results.next().expect("first match")?;
            results.budget.as_ref().expect("parallel handoff").cancel();
            library.source().cancelled.store(true, Ordering::SeqCst);
            Ok(())
        })??;
        // At most one already-starting payload per worker may race the
        // cancellation check. Later queued partitions must do no I/O.
        assert!(library.source().headers.load(Ordering::SeqCst) > before);
        assert!(library.source().late_headers.load(Ordering::SeqCst) <= workers);
        library.source().cancelled.store(false, Ordering::SeqCst);
        library.source().late_headers.store(0, Ordering::SeqCst);
        let count = executor.inspect_results(&prepared, None, Some(0), |results| {
            results.try_fold(0usize, |n, f| f.map(|_| n + 1))
        })??;
        assert_eq!(count, expected);
    }
    Ok(())
}

#[test]
fn damaged_deferred_tiles_match_serial_success_errors_and_bounds() -> Result<()> {
    let original = include_bytes!("../../tests/fixtures/tiled_global.gol");
    let executor = QueryExecutor::new(2)?;
    let mut state = 0x3a1ed53b91b5c7d1u64;
    for _ in 0..512 {
        let mut bytes = original.to_vec();
        for _ in 0..4 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let at = state as usize % bytes.len();
            bytes[at] ^= (state >> 32) as u8;
        }
        let Ok(library) = Library::with_limits(
            bytes,
            Limits {
                max_traversal_steps: 1000,
                max_tree_depth: 16,
                max_tags: 100,
                ..Limits::default()
            },
        ) else {
            continue;
        };
        for text in ["*", "n[name]", "wa[!amenity]"] {
            let query = Query::parse(text)?;
            let Ok(prepared) = query.prepare(&library) else {
                continue;
            };
            for bounds in [None, Some(Bounds::new(-30000, -30000, 30000, 30000)?)] {
                let collect = |features: &mut dyn Iterator<Item = Result<Feature<'_, _>>>| {
                    features
                        .map(|f| f.and_then(|f| Ok((f.id(), f.bounds()?))))
                        .collect::<Result<Vec<_>>>()
                };
                let mut serial = match bounds {
                    None => collect(&mut prepared.iter()),
                    Some(bounds) => collect(&mut prepared.in_bounds(bounds)),
                };
                let mut parallel =
                    executor.inspect_results(&prepared, bounds, Some(0), |results| {
                        let result = collect(results);
                        assert!(results.next().is_none());
                        result
                    })?;
                assert_eq!(serial.is_ok(), parallel.is_ok(), "{text} {bounds:?}");
                let counted = executor.count(&prepared, bounds);
                assert_eq!(
                    serial.is_ok(),
                    counted.is_ok(),
                    "damaged count {text} {bounds:?}"
                );
                if let (Ok(expected), Ok(count)) = (&serial, counted) {
                    assert_eq!(expected.len() as u64, count);
                }

                if let (Ok(expected), Ok(actual)) = (&mut serial, &mut parallel) {
                    expected.sort_unstable_by_key(|f| f.0);
                    actual.sort_unstable_by_key(|f| f.0);
                    assert_eq!(actual, expected, "{text} {bounds:?}");
                }
            }
        }
    }
    Ok(())
}
#[test]
fn parallel_queries_and_cancellation_preserve_a_small_shared_cache() -> crate::Result<()> {
    use crate::{Library, Query, QueryExecutor};
    let bytes = include_bytes!("../../tests/fixtures/tiled_global.gol").as_slice();
    let reference = Library::new(bytes)?;
    let cached = Library::new(crate::source::CachedFile::open(
        "tests/fixtures/tiled_global.gol",
        16 * 1024,
    )?)?;
    let executor = QueryExecutor::new(4)?;
    for text in ["*", "*[name]", "*[!amenity]"] {
        let query = Query::parse(text)?;
        let mut expected = query
            .prepare(&reference)?
            .iter()
            .map(|f| f.map(|f| f.id()))
            .collect::<crate::Result<Vec<_>>>()?;
        let prepared = query.prepare(&cached)?;
        for limit in [0, 1, 17, 4096] {
            executor.inspect_results(&prepared, None, Some(0), |results| {
                for feature in results.take(limit) {
                    feature?;
                }
                Ok::<_, crate::Error>(())
            })??;
        }
        let mut actual = executor.inspect_results(&prepared, None, Some(0), |results| {
            results
                .map(|f| f.map(|f| f.id()))
                .collect::<crate::Result<Vec<_>>>()
        })??;
        expected.sort_unstable();
        actual.sort_unstable();
        assert_eq!(expected, actual);
        assert!(cached.source().allocated_bytes()? <= 16 * 1024);
    }
    Ok(())
}

// One populated tile and three empty tile-index entries keep the producer
// on a whole tile while leaving Rayon free to choose job order.
fn batch_fixture(nodes: usize) -> Result<Vec<u8>> {
    const LEAF: usize = 52;
    const RECORD: usize = 20;
    let fixture = include_bytes!("../../tests/fixtures/global.gol").as_slice();
    let original = Library::new(fixture)?;
    let index = original.tile_index as usize;
    let snapshot = 128 + usize::from(original.active_snapshot()) * 64;
    let mut bytes = fixture.to_vec();
    let start = bytes.len().next_multiple_of(4096);
    let length = LEAF + nodes * RECORD;
    let empty = (start + length).next_multiple_of(4096);
    bytes.resize(empty + 2 * 4096 + 28, 0);
    let mut word = |offset: usize, value: u32| {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    };
    word(snapshot + 28, 4);
    word(index, 20);
    word(index + 4, ((start / 4096) as u32) << 2 | 2);
    word(index + 8, 0b111);
    for (i, entry) in [index + 12, index + 16, index + 20].into_iter().enumerate() {
        let empty = empty + i * 4096;
        word(entry, ((empty / 4096) as u32) << 2 | 2);
        word(empty, 24);
    }
    word(start, (length - 4) as u32);
    word(start + 8, 16); // Node root list at 24.
    word(start + 24, 8 | 1); // Last root, branch at 32.
    word(start + 32, 20 | 3); // Last branch, leaf at 52.
    for i in 0..nodes {
        let header = (((i as u64 + 1) << 12) | u64::from(i + 1 == nodes)).to_le_bytes();
        let anchor = start + LEAF + RECORD * i + 8;
        bytes[anchor..anchor + 8].copy_from_slice(&header);
    }
    Ok(bytes)
}

#[test]
fn full_serial_and_parallel_scans_can_teach_each_other_a_key() -> Result<()> {
    const NODES: usize = 100_002;
    let mut bytes = batch_fixture(NODES)?;
    let original = Library::new(bytes.as_slice())?;
    let key = original.dictionary().unwrap().key_code("amenity").unwrap();
    let category = original
        .categories
        .iter()
        .find(|(k, _)| *k == key)
        .unwrap()
        .1;
    let start = original.tile_page(1)? as usize;
    let tag_table = 52 + NODES * 20;
    bytes[start..start + 4].copy_from_slice(&((tag_table + 8 - 4) as u32).to_le_bytes());
    bytes[start + 28..start + 32].copy_from_slice(&category.to_le_bytes());
    for (i, value) in [1u32, 2].into_iter().enumerate() {
        let word = (value << 16) | 0x8001 | (u32::from(key) << 2);
        let at = start + tag_table + i * 4;
        bytes[at..at + 4].copy_from_slice(&word.to_le_bytes());
    }
    for i in 0..NODES {
        let p = 52 + i * 20 + 16;
        let delta = (tag_table + i % 2 * 4 - p) as i32;
        bytes[start + p..start + p + 4].copy_from_slice(&delta.to_le_bytes());
    }
    let executor = QueryExecutor::new(3)?;
    for finish_in_pool in [false, true] {
        let mut library = Library::new(bytes.as_slice())?;
        let yes = Query::parse("n[amenity=yes]")?;
        let no = Query::parse("n[amenity=no]")?;
        let yes = yes.prepare(&library)?;
        let no = no.prepare(&library)?;
        for _ in 0..4 {
            yes.iter().next().unwrap()?;
            executor.inspect_results(&no, None, None, |items| {
                assert_eq!(items.take(3).count(), 3)
            })?;
        }
        assert_eq!(yes.in_bounds(crate::Bounds::WORLD).count(), NODES / 2);
        assert_eq!(yes.iter().count(), NODES / 2);
        assert_eq!(
            executor.inspect_results(&no, None, None, |items| items.count())?,
            NODES / 2
        );
        assert_eq!(library.text_index_bytes(), 0);
        let count = if finish_in_pool {
            executor.inspect_results(&yes, None, None, |items| items.count())?
        } else {
            yes.iter().count()
        };
        assert_eq!(count, NODES / 2);
        assert!(library.text_index_bytes() > 0);
        assert_eq!(no.iter().count(), NODES / 2);
        let bytes = library.text_index_bytes();
        assert!(
            library
                .index_text_key("amenity", crate::Types::NODE, bytes - 1)
                .is_err()
        );
        assert_eq!(
            library.index_text_key("amenity", crate::Types::NODE, bytes)?,
            bytes
        );
        assert_eq!(library.text_index_bytes(), bytes);
        library.set_adaptive_text_index_bytes(0);
        assert_eq!(library.text_index_bytes(), bytes);
    }
    Ok(())
}

#[test]
fn worker_error_after_a_full_batch_reaches_the_consumer_once() -> Result<()> {
    const FULL_BATCH_END: usize = 1 + 1024;
    let bytes = batch_fixture(FULL_BATCH_END + 4)?;
    let library = Library::new(observed(&bytes))?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let expected = prepared.iter().collect::<Result<Vec<_>>>()?;
    // Deliver the singleton and one full batch, then two more matches enter
    // the next batch before the traversal hits the faulty record.
    library
        .source()
        .fault
        .store(expected[FULL_BATCH_END + 2].anchor(), Ordering::Relaxed);

    let executor = QueryExecutor::new(1)?;
    executor.inspect_results(&prepared, None, Some(0), |results| -> Result<()> {
        for expected in &expected[..FULL_BATCH_END] {
            assert_eq!(
                results.next().expect("delivered feature")?.id(),
                expected.id()
            );
        }
        assert!(matches!(
            results.next(),
            Some(Err(Error::Io(error))) if error.to_string() == "injected record failure"
        ));
        assert!(results.next().is_none());
        assert!(results.next().is_none());
        Ok(())
    })??;
    Ok(())
}

#[test]
fn consumer_can_disconnect_at_the_first_full_batch_boundary() -> Result<()> {
    const FULL_BATCH_END: usize = 1 + 1024;
    const NODES: usize = 8192;
    let bytes = batch_fixture(NODES)?;
    let library = Library::new(observed(&bytes))?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let executor = QueryExecutor::new(1)?;

    library.source().reads.store(0, Ordering::Relaxed);
    let consumed = executor.inspect_results(&prepared, None, Some(0), |results| {
        results
            .take(FULL_BATCH_END)
            .try_fold(0, |n, feature| feature.map(|_| n + 1))
    })??;
    assert_eq!(consumed, FULL_BATCH_END);
    // With one worker this tile cannot finish before the callback returns.
    // Allow any amount of run-ahead permitted by the bounded queue.
    assert!(library.source().reads.load(Ordering::Relaxed) < NODES);
    let total = executor.inspect_results(&prepared, None, Some(0), |results| {
        results.try_fold(0, |n, feature| feature.map(|_| n + 1))
    })??;
    assert_eq!(total, NODES);
    Ok(())
}

#[test]
fn indexed_stream_is_lazy_and_worker_errors_fuse_it() -> Result<()> {
    let mut bytes = include_bytes!("../../tests/fixtures/wide_exports.gol").to_vec();
    for p in 0..bytes.len() - 12 {
        if &bytes[p..p + 7] == b"Vertex "
            && bytes[p + 7..p + 11].iter().all(u8::is_ascii_digit)
            && !bytes[p + 11].is_ascii_digit()
        {
            bytes[p + 7..p + 11].copy_from_slice(b"0000");
        }
    }
    let mut library = Library::new(observed(&bytes))?;
    library.index_text_key("name", crate::Types::NODE, 1024 * 1024)?;
    let query = Query::parse("n[name='Vertex 0000']")?;
    let prepared = query.prepare(&library)?;
    let mut expected = prepared
        .iter()
        .map(|matched| Ok(matched?.id()))
        .collect::<Result<Vec<_>>>()?;
    assert!(expected.len() > 4096);
    let first = expected[0];
    expected.sort_unstable();
    let executor = QueryExecutor::new(4)?;
    library.source().reads.store(0, Ordering::Relaxed);
    executor.inspect_results(&prepared, None, None, |_| ())?;
    assert_eq!(library.source().reads.load(Ordering::Relaxed), 0);
    let actual =
        executor.inspect_results(&prepared, None, None, |results| results.next().unwrap())??;
    assert_eq!(actual.id(), first);
    assert_eq!(library.source().reads.load(Ordering::Relaxed), 1);

    let late = prepared.iter().nth(6000).unwrap()?;
    library
        .source()
        .fault
        .store(late.anchor(), Ordering::Relaxed);
    let result = executor.inspect_results(&prepared, None, None, |results| {
        let result = results.by_ref().collect::<Result<Vec<_>>>();
        assert!(results.next().is_none());
        result
    })?;
    assert!(result.is_err());

    library.source().fault.store(u64::MAX, Ordering::Relaxed);
    assert_eq!(
        executor.inspect_results(&prepared, None, None, |results| results.take(2).count())?,
        2
    );
    let mut actual = executor.inspect_results(&prepared, None, None, |results| {
        results
            .map(|matched| Ok(matched?.id()))
            .collect::<Result<Vec<_>>>()
    })??;
    actual.sort_unstable();
    assert_eq!(actual, expected);
    Ok(())
}

#[test]
fn count_once_matches_serial_and_executor_across_thread_counts() -> Result<()> {
    let bytes = include_bytes!("../../tests/fixtures/tiled_global.gol").as_slice();
    let library = Library::new(bytes)?;
    let executor = QueryExecutor::new(4)?;
    for text in ["*", "*[name]", "*[!name]", "n[name=\"*a*\"]"] {
        let query = Query::parse(text)?;
        let prepared = query.prepare(&library)?;
        let expected = prepared.count()?;
        assert_eq!(executor.count(&prepared, None)?, expected);
        for threads in [0, 1, 2, 4] {
            assert_eq!(
                QueryExecutor::count_once(threads, &prepared, None)?,
                expected,
                "threads={threads} query={text}"
            );
        }
        let folded = prepared.try_fold(0u64, |acc, _| Ok(acc + 1))?;
        assert_eq!(folded, expected);
    }
    Ok(())
}

#[test]
fn try_fold_reduce_once_matches_serial_across_prefixes_and_threads() -> Result<()> {
    let bytes = include_bytes!("../../tests/fixtures/tiled_global.gol").as_slice();
    let library = Library::new(bytes)?;
    for text in ["*", "*[name]", "*[!name]"] {
        let query = Query::parse(text)?;
        let prepared = query.prepare(&library)?;
        let mut expected: Vec<_> = prepared
            .iter()
            .map(|f| Ok(f?.id()))
            .collect::<Result<Vec<_>>>()?;
        expected.sort_unstable();
        for threads in [0, 1, 2, 4] {
            for prefix in [0, 2, 16, 4096] {
                let mut actual = QueryExecutor::try_fold_reduce_once(
                    threads,
                    &prepared,
                    None,
                    prefix,
                    Vec::new,
                    |mut ids, f| {
                        ids.push(f.id());
                        Ok(ids)
                    },
                    |mut left, right| {
                        left.extend(right);
                        Ok(left)
                    },
                )?;
                actual.sort_unstable();
                assert_eq!(
                    actual, expected,
                    "threads={threads} prefix={prefix} query={text}"
                );
            }
        }
    }
    Ok(())
}
