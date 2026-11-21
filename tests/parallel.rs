#![cfg(feature = "parallel")]
#![forbid(unsafe_code)]

use regol::{Bounds, Library, Limits, Query, QueryExecutor, Result};

#[test]
fn parallel_queries_match_serial_sets_and_windows() -> Result<()> {
    let executor = QueryExecutor::new(4)?;
    for name in [
        "basic",
        "tiled_global",
        "stress",
        "query_semantics",
        "wide_exports",
    ] {
        let library = Library::new(std::fs::read(format!("tests/fixtures/{name}.gol"))?)?;
        for text in [
            "*",
            "n",
            "w",
            "a",
            "r",
            "na[amenity]",
            "*[name]",
            "*[!name]",
            "*[amenity=cafe],*[highway]",
            "*[name~\".*a.*\"]",
            "*[capacity>=12]",
            "*[missing_key=missing_value]",
            "*[value]",
            "*[value!=12]",
        ] {
            let query = Query::parse(text)?;
            let prepared = query.prepare(&library)?;
            for bounds in [
                None,
                Some(Bounds::new(-30000, -30000, 30000, 30000)?),
                Some(Bounds::WORLD),
            ] {
                let mut serial = match bounds {
                    None => prepared
                        .iter()
                        .map(|f| f.map(|f| f.id()))
                        .collect::<Result<Vec<_>>>()?,
                    Some(bounds) => prepared
                        .in_bounds(bounds)
                        .map(|f| f.map(|f| f.id()))
                        .collect::<Result<Vec<_>>>()?,
                };
                let mut parallel = executor.with_query(&prepared, bounds, |features| {
                    features
                        .map(|f| f.map(|f| f.id()))
                        .collect::<Result<Vec<_>>>()
                })??;
                let count = match bounds {
                    None => prepared.count()?,
                    Some(bounds) => prepared.count_in_bounds(bounds)?,
                };
                assert_eq!(count, serial.len() as u64, "serial count {text}");
                assert_eq!(
                    executor.count(&prepared, bounds)?,
                    count,
                    "parallel count {text}"
                );
                serial.sort_unstable();
                parallel.sort_unstable();
                assert_eq!(serial, parallel, "{name} {text} {bounds:?}");
            }
        }
    }
    Ok(())
}

#[test]
fn shared_step_limit_matches_serial_at_every_boundary() -> Result<()> {
    for workers in [1, 2, 4] {
        let executor = QueryExecutor::new(workers)?;
        for limit in 0..200 {
            let library = Library::with_limits(
                include_bytes!("fixtures/tiled_global.gol").as_slice(),
                Limits {
                    max_traversal_steps: limit,
                    ..Limits::default()
                },
            )?;
            let query = Query::parse("*")?;
            let prepared = query.prepare(&library)?;
            let serial = prepared.iter().collect::<Result<Vec<_>>>();
            let parallel = executor
                .with_query(&prepared, None, |features| {
                    features.collect::<Result<Vec<_>>>()
                })
                .and_then(|result| result);
            assert_eq!(
                serial.is_ok(),
                parallel.is_ok(),
                "workers={workers} limit={limit}"
            );
            let counted = executor.count(&prepared, None);
            let folded = executor.try_fold_reduce(
                &prepared,
                None,
                || 0u64,
                |n, _| Ok(n + 1),
                |a, b| Ok(a + b),
            );
            assert_eq!(serial.is_ok(), folded.is_ok());
            if let (Ok(serial), Ok(folded)) = (&serial, &folded) {
                assert_eq!(serial.len() as u64, *folded);
            }
            assert_eq!(
                serial.is_ok(),
                counted.is_ok(),
                "count workers={workers} limit={limit}"
            );
            if let (Ok(serial), Ok(count)) = (&serial, &counted) {
                assert_eq!(serial.len() as u64, *count);
            }
            if let (Ok(serial), Ok(parallel)) = (serial, parallel) {
                assert_eq!(serial.len(), parallel.len());
            }
        }
    }
    Ok(())
}

#[test]
fn early_consumer_exit_releases_workers_for_reuse() -> Result<()> {
    for workers in [1, 4] {
        let executor = QueryExecutor::new(workers)?;
        let library = Library::new(include_bytes!("fixtures/wide_exports.gol").as_slice())?;
        let query = Query::all();
        let prepared = query.prepare(&library)?;
        for take in [0, 1, 7, 4097, 0] {
            let count = executor.with_query(&prepared, None, |features| {
                features
                    .take(take)
                    .collect::<Result<Vec<_>>>()
                    .map(|v| v.len())
            })??;
            assert_eq!(count, take);
        }
        let count = executor.with_query(&prepared, None, |features| {
            features.try_fold(0usize, |n, f| f.map(|_| n + 1))
        })??;
        assert_eq!(count, prepared.iter().count());
    }
    Ok(())
}

#[test]
fn consumer_panic_joins_workers_and_pool_remains_usable() -> Result<()> {
    let executor = QueryExecutor::new(4)?;
    let library = Library::new(include_bytes!("fixtures/wide_exports.gol").as_slice())?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = executor.with_query(&prepared, None, |features| {
            for _ in 0..4097 {
                assert!(features.next().is_some());
            }
            panic!("consumer stopped unexpectedly");
        });
    }));
    assert!(panic.is_err());
    let count = executor.with_query(&prepared, None, |features| {
        features.try_fold(0usize, |n, feature| feature.map(|_| n + 1))
    })??;
    assert_eq!(count, prepared.iter().count());
    Ok(())
}

#[test]
fn damaged_queries_terminate_and_leave_executor_reusable() -> Result<()> {
    let executor = QueryExecutor::new(4)?;
    let original = include_bytes!("fixtures/tiled_global.gol");
    let mut state = 0x853c49e6748fea9bu64;
    let queries: Vec<_> = ["*", "*[name]", "*[!amenity]", "*[name=\"A*\"]"]
        .into_iter()
        .map(Query::parse)
        .collect::<Result<_>>()?;
    for _ in 0..128 {
        let mut bytes = original.to_vec();
        for _ in 0..4 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let at = state as usize % bytes.len();
            bytes[at] ^= (state >> 32) as u8;
        }
        let limits = Limits {
            max_traversal_steps: 1000,
            max_tree_depth: 16,
            max_tags: 100,
            ..Limits::default()
        };
        let Ok(library) = Library::with_limits(bytes, limits) else {
            continue;
        };
        for query in &queries {
            let Ok(prepared) = query.prepare(&library) else {
                continue;
            };
            let serial = prepared
                .iter()
                .map(|f| f.map(|f| f.id()))
                .collect::<Result<Vec<_>>>();
            let parallel = executor
                .with_query(&prepared, None, |features| {
                    let result = features
                        .by_ref()
                        .map(|f| f.map(|f| f.id()))
                        .collect::<Result<Vec<_>>>();
                    assert!(features.next().is_none(), "results must be fused");
                    result
                })
                .and_then(|result| result);
            // Corruption may be found in a different order, but successful
            // complete traversals must return the same multiset of identities.
            if let (Ok(mut serial), Ok(mut parallel)) = (serial, parallel) {
                serial.sort_unstable();
                parallel.sort_unstable();
                assert_eq!(serial, parallel);
            }
        }
    }
    Ok(())
}
