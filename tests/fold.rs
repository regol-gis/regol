#![forbid(unsafe_code)]
use regol::{Bounds, Error, Feature, FeatureId, Library, Query, Result, source::Source};

fn collect<'a, S: Source>(
    mut output: Vec<(FeatureId, usize)>,
    feature: Feature<'a, S>,
) -> Result<Vec<(FeatureId, usize)>> {
    let mut tags = 0;
    for tag in feature.tags()? {
        tag?;
        tags += 1;
    }
    output.push((feature.id(), tags));
    Ok(output)
}

#[test]
fn reductions_match_iterator_fields_and_bounds() -> Result<()> {
    for fixture in ["basic", "query_semantics", "wide_exports"] {
        let library = Library::new(std::fs::read(format!("tests/fixtures/{fixture}.gol"))?)?;
        for text in [
            "*",
            "*[name]",
            "*[name][!ref]",
            "*[amenity=cafe],*[highway]",
            "*[no_such_key]",
        ] {
            let query = Query::parse(text)?;
            let prepared = query.prepare(&library)?;
            for bounds in [None, Some(Bounds::new(-30000, -30000, 30000, 30000)?)] {
                let mut expected = match bounds {
                    None => prepared
                        .iter()
                        .try_fold(Vec::new(), |v, f| collect(v, f?))?,
                    Some(b) => prepared
                        .in_bounds(b)
                        .try_fold(Vec::new(), |v, f| collect(v, f?))?,
                };
                let mut serial = match bounds {
                    None => prepared.try_fold(Vec::new(), collect)?,
                    Some(b) => prepared.try_fold_in_bounds(b, Vec::new(), collect)?,
                };
                // Serial folds preserve order and need no identity element.
                assert_eq!(serial, expected);
                expected.sort_unstable();
                serial.sort_unstable();
                #[cfg(feature = "parallel")]
                for workers in [1, 4] {
                    let mut parallel = regol::QueryExecutor::new(workers)?.try_fold_reduce(
                        &prepared,
                        bounds,
                        Vec::new,
                        collect,
                        |mut a, b| {
                            a.extend(b);
                            Ok(a)
                        },
                    )?;
                    parallel.sort_unstable();
                    assert_eq!(
                        parallel, expected,
                        "{fixture} {text} {bounds:?} workers={workers}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn serial_callback_errors_stop_immediately_and_owned_values_drop() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/basic.gol").as_slice())?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let drops = std::cell::Cell::new(0);
    struct Owned<'a>(&'a std::cell::Cell<usize>);
    impl Drop for Owned<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    let mut calls = 0;
    let result = prepared.try_fold(Owned(&drops), |value, _| {
        calls += 1;
        if calls == 3 {
            Err(Error::Limit("callback"))
        } else {
            Ok(value)
        }
    });
    assert!(matches!(result, Err(Error::Limit("callback"))));
    assert_eq!(calls, 3);
    assert_eq!(drops.get(), 1);
    assert_eq!(
        prepared.try_fold(10, |n, _| Ok(n + 1))?,
        prepared.count()? + 10
    );
    Ok(())
}

#[cfg(feature = "parallel")]
#[test]
fn parallel_callback_and_reduce_errors_join_and_allow_reuse() -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let library = Library::new(include_bytes!("fixtures/wide_exports.gol").as_slice())?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let executor = regol::QueryExecutor::new(4)?;
    let late = prepared.iter().nth(10000).unwrap()?.id();
    let calls = AtomicUsize::new(0);
    let result = executor.try_fold_reduce(
        &prepared,
        None,
        || 0,
        |n, f| {
            calls.fetch_add(1, Ordering::Relaxed);
            if f.id() == late {
                Err(Error::Limit("callback"))
            } else {
                Ok(n + 1)
            }
        },
        |a, b| Ok(a + b),
    );
    assert!(matches!(result, Err(Error::Limit("callback"))));
    let after = calls.load(Ordering::Relaxed);
    // A successful second run also establishes that the first has joined.
    assert_eq!(
        executor.try_fold_reduce(&prepared, None, || 0, |n, _| Ok(n + 1), |a, b| Ok(a + b))?,
        prepared.count()?
    );
    assert_eq!(calls.load(Ordering::Relaxed), after);
    assert!(matches!(
        executor.try_fold_reduce(
            &prepared,
            None,
            || 0,
            |n, _| Ok(n + 1),
            |_, _| Err(Error::Limit("reduce"))
        ),
        Err(Error::Limit("reduce"))
    ));
    // Send, but deliberately not Sync, accumulators are supported.
    let result = executor.try_fold_reduce(
        &prepared,
        None,
        || std::cell::Cell::new(0u64),
        |n, _| {
            n.set(n.get() + 1);
            Ok(n)
        },
        |a, b| {
            a.set(a.get() + b.get());
            Ok(a)
        },
    )?;
    assert_eq!(result.get(), prepared.count()?);
    Ok(())
}

#[test]
fn owned_geometry_reduction_preserves_complete_shapes() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/geometry.gol").as_slice())?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let expected = prepared
        .iter()
        .map(|f| {
            let f = f?;
            Ok((f.id(), f.geometry()?))
        })
        .collect::<Result<Vec<_>>>()?;
    let collect = |mut output: Vec<_>, feature: Feature<'_, _>| {
        output.push((feature.id(), feature.geometry()?));
        Ok(output)
    };
    assert_eq!(prepared.try_fold(Vec::new(), collect)?, expected);
    #[cfg(feature = "parallel")]
    {
        let mut actual = regol::QueryExecutor::new(4)?.try_fold_reduce(
            &prepared,
            None,
            Vec::new,
            collect,
            |mut a, b| {
                a.extend(b);
                Ok(a)
            },
        )?;
        let mut expected = expected;
        actual.sort_by_key(|f| f.0);
        expected.sort_by_key(|f| f.0);
        assert_eq!(actual, expected);
    }
    Ok(())
}

#[test]
fn callback_field_read_limits_are_not_bypassed() -> Result<()> {
    let library = Library::with_limits(
        include_bytes!("fixtures/basic.gol").as_slice(),
        regol::Limits {
            max_tags: 0,
            ..Default::default()
        },
    )?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    let expected = prepared.iter().try_fold(Vec::new(), |v, f| collect(v, f?));
    assert!(expected.is_err());
    assert!(prepared.try_fold(Vec::new(), collect).is_err());
    #[cfg(feature = "parallel")]
    assert!(
        regol::QueryExecutor::new(4)?
            .try_fold_reduce(&prepared, None, Vec::new, collect, |mut a, b| {
                a.extend(b);
                Ok(a)
            })
            .is_err()
    );
    Ok(())
}

#[cfg(feature = "parallel")]
#[test]
fn configured_reduction_prefixes_preserve_fields_empty_queries_and_errors() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/wide_exports.gol").as_slice())?;
    for workers in [1, 4] {
        for prefix in [0, 1, 256, 4096, u64::MAX] {
            let executor = regol::QueryExecutor::new(workers)?.with_reduction_prefix_steps(prefix);
            for text in ["*", "*[name]", "*[no_such_key]"] {
                let query = Query::parse(text)?;
                let prepared = query.prepare(&library)?;
                let mut expected = prepared
                    .iter()
                    .try_fold(Vec::new(), |v, f| collect(v, f?))?;
                let mut actual =
                    executor.try_fold_reduce(&prepared, None, Vec::new, collect, |mut a, b| {
                        a.extend(b);
                        Ok(a)
                    })?;
                actual.sort_unstable();
                expected.sort_unstable();
                assert_eq!(actual, expected);
                if !expected.is_empty() {
                    assert!(
                        executor
                            .try_fold_reduce(
                                &prepared,
                                None,
                                || 0,
                                |_, _| Err(Error::Limit("callback")),
                                |a: u64, b| Ok(a + b)
                            )
                            .is_err()
                    );
                }
                assert_eq!(executor.count(&prepared, None)?, expected.len() as u64);
            }
        }
    }
    Ok(())
}

#[cfg(feature = "parallel")]
#[test]
fn adaptive_pilot_consumes_fields_once_and_preserves_callback_errors() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/wide_exports.gol").as_slice())?;
    for workers in [1, 4] {
        let executor = regol::QueryExecutor::new(workers)?.with_adaptive_reductions();
        for text in ["*", "*[name]", "*[no_such_key]"] {
            let query = Query::parse(text)?;
            let prepared = query.prepare(&library)?;
            let mut expected = prepared
                .iter()
                .try_fold(Vec::new(), |v, f| collect(v, f?))?;
            let mut actual =
                executor.try_fold_reduce(&prepared, None, Vec::new, collect, |mut a, b| {
                    a.extend(b);
                    Ok(a)
                })?;
            actual.sort_unstable();
            expected.sort_unstable();
            assert_eq!(actual, expected);
            if !expected.is_empty() {
                assert!(
                    executor
                        .try_fold_reduce(
                            &prepared,
                            None,
                            || 0,
                            |_, _| Err(Error::Limit("callback")),
                            |a: u64, b| Ok(a + b)
                        )
                        .is_err()
                );
            }
        }
    }
    Ok(())
}
