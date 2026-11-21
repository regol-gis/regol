use super::*;
use crate::{
    Error, Feature, Limits, Query, Result,
    source::mapped_scan_test_support::{force_helpers, mapped_fixture},
};

const TILED: &[u8] = include_bytes!("../../../tests/fixtures/tiled.gol");

#[cfg(feature = "parallel")]
#[test]
fn partition_hints_only_examine_deferred_unbounded_mapped_tiles() -> Result<()> {
    let mapped = Library::new(mapped_fixture(TILED)?)?;
    let buffered = Library::new(TILED)?;
    assert!(
        Features::new(&buffered, None)
            .partition_fault_sample()
            .is_none()
    );
    assert!(
        Features::new(&mapped, Some(Bounds::WORLD))
            .partition_fault_sample()
            .is_none()
    );
    let mut cursor = Features::new(&mapped, None);
    assert!(cursor.partition_fault_sample().is_some());
    cursor.hint_pending_tile(&|| panic!("residual directory work has no tile payload"));

    let first = mapped.features().next().unwrap()?;
    cursor.defer_tile(crate::spatial::DeferredTile {
        start: first.tile.start(),
        bounds: Bounds::WORLD,
    });
    let expected = cursor
        .fork_remaining()
        .map(|f| f.map(Feature::id))
        .collect::<Result<Vec<_>>>()?;
    assert!(!expected.is_empty());
    for active in [false, true] {
        let checks = std::cell::Cell::new(0);
        let mut worker = cursor.fork_remaining();
        worker.hint_pending_tile(&|| {
            checks.set(checks.get() + 1);
            active
        });
        if active {
            assert!(checks.get() > 1);
        } else {
            assert_eq!(checks.get(), 1);
        }
        assert_eq!(
            worker
                .by_ref()
                .map(|f| f.map(Feature::id))
                .collect::<Result<Vec<_>>>()?,
            expected
        );
    }
    Ok(())
}

#[test]
fn cold_hinting_leaves_ids_order_and_terminal_error_to_demand() -> Result<()> {
    let source = Library::new(mapped_fixture(TILED)?)?;
    let query = Query::all();
    let prepared = query.prepare(&source)?;
    let expected = prepared
        .iter()
        .map(|feature| feature.map(Feature::id))
        .collect::<Result<Vec<_>>>()?;

    let mut demand = prepared.features(None).enable_mapped_ahead();
    force_helpers(demand.read_ahead.as_mut().unwrap());
    let first = prepared.next_in(&mut demand, || Ok(true)).unwrap()?.id();
    demand.maybe_start_mapped_ahead();
    let scan = demand
        .mapped_ahead
        .as_ref()
        .expect("tiled cold query")
        .scan
        .clone();
    let mut ids = vec![first];
    while let Some(feature) = prepared.next_in(&mut demand, || Ok(true)) {
        ids.push(feature?.id());
    }
    assert_eq!(ids, expected);
    assert!(scan.cancelled.load(Ordering::Relaxed));

    let mut demand = prepared.features(None).enable_mapped_ahead();
    force_helpers(demand.read_ahead.as_mut().unwrap());
    prepared.next_in(&mut demand, || Ok(true)).unwrap()?;
    demand.maybe_start_mapped_ahead();
    let scan = demand.mapped_ahead.as_ref().unwrap().scan.clone();
    demand.work = vec![Work::Branch {
        p: demand.tile.end(),
        node: true,
        depth: 0,
    }];
    demand.leaf = None;
    let error = prepared
        .next_in(&mut demand, || Ok(true))
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, Error::Invalid { .. }));
    assert!(prepared.next_in(&mut demand, || Ok(true)).is_none());
    assert!(scan.cancelled.load(Ordering::Relaxed));
    Ok(())
}

#[test]
fn bounded_warm_and_budgeted_cursors_do_not_escalate() -> Result<()> {
    let library = Library::new(mapped_fixture(TILED)?)?;
    let query = Query::all();
    let prepared = query.prepare(&library)?;
    for bounds in [Some(Bounds::WORLD), None] {
        let mut cursor = prepared.features(bounds).enable_mapped_ahead();
        prepared.next_in(&mut cursor, || Ok(true)).unwrap()?;
        cursor.maybe_start_mapped_ahead();
        assert!(cursor.mapped_ahead.is_none());
    }
    let limits = Limits {
        max_traversal_steps: 1024,
        ..Limits::default()
    };
    let library = Library::with_limits(mapped_fixture(TILED)?, limits)?;
    let prepared = query.prepare(&library)?;
    let mut cursor = prepared.features(None).enable_mapped_ahead();
    force_helpers(cursor.read_ahead.as_mut().unwrap());
    prepared.next_in(&mut cursor, || Ok(true)).unwrap()?;
    cursor.maybe_start_mapped_ahead();
    assert!(cursor.mapped_ahead.is_none());
    Ok(())
}

#[test]
fn bounded_hinting_keeps_exact_bounds_including_duplicate_suppression() -> Result<()> {
    let source = Library::new(mapped_fixture(TILED)?)?;
    let query = Query::all();
    let prepared = query.prepare(&source)?;
    let bounds = Bounds::new(-1_000_000_000, -1_000_000_000, 1_000_000_000, 1_000_000_000)?;
    let expected = prepared
        .in_bounds(bounds)
        .map(|feature| feature.map(Feature::id))
        .collect::<Result<Vec<_>>>()?;

    let mut demand = prepared.features(Some(bounds)).enable_mapped_ahead();
    force_helpers(demand.read_ahead.as_mut().unwrap());
    demand.maybe_start_mapped_ahead();
    assert!(demand.mapped_ahead.is_none());
    let first = prepared.next_in(&mut demand, || Ok(true)).unwrap()?.id();
    demand.maybe_start_mapped_ahead();
    let scan = &demand
        .mapped_ahead
        .as_ref()
        .expect("eligible bounded cursor")
        .scan;
    assert_eq!(scan.bounds, Some(bounds));
    let mut ids = vec![first];
    while let Some(feature) = prepared.next_in(&mut demand, || Ok(true)) {
        ids.push(feature?.id());
    }
    assert_eq!(ids, expected);
    Ok(())
}

#[test]
fn malformed_tag_offset_remains_a_checked_demand_error() -> Result<()> {
    let library = Library::new(TILED)?;
    let original = library.features().next().unwrap()?;
    let offset = (original.anchor() + 8) as usize;
    let order = |library: &Library<MappedFile>| {
        Query::all()
            .prepare(library)?
            .iter()
            .map(|feature| feature.map(Feature::id))
            .collect::<Result<Vec<_>>>()
    };
    let mapped = Library::new(mapped_fixture(TILED)?)?;
    let expected = order(&mapped)?;
    let mut broken = TILED.to_vec();
    broken[offset..offset + 4].copy_from_slice(&i32::MAX.to_le_bytes());
    let mapped = Library::new(mapped_fixture(&broken)?)?;
    assert_eq!(order(&mapped)?, expected);
    let first = mapped.features().next().unwrap()?;
    assert!(first.checked_tag_table().is_err());
    assert!(first.tags().is_err());
    let worker = mapped.source().worker_library().unwrap();
    let scan = Arc::new(Scan {
        library: worker,
        bounds: None,
        cancelled: AtomicBool::new(false),
        remaining_steps: AtomicU64::new(256),
    });
    complete(Job {
        scan: Arc::downgrade(&scan),
        tile: first.tile.start(),
        bounds: Bounds::WORLD,
        types: 15,
        required: 0,
        alternative: 0,
        _permit: Permit::acquire().unwrap(),
    });
    assert!(scan.remaining_steps.load(Ordering::Relaxed) < 256);
    assert_eq!(order(&mapped)?, expected);
    Ok(())
}
