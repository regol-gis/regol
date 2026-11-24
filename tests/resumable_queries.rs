use regol::{
    Bounds, Feature, FeatureId, Library, Query,
    source::{RangeQuery, RangeQueryEvent, Source, TileVisit, TraversalOrder, TraversalOrdinal},
};
use std::{
    cell::{Cell, RefCell},
    io,
};

const GOL: &[u8] = include_bytes!("fixtures/stress.gol");

struct Scheduled<'a> {
    bytes: &'a [u8],
    order: TraversalOrder,
    delay: Option<FeatureId>,
    cancel: bool,
    plan: bool,
    active: Cell<Option<usize>>,
    pending: Cell<Option<usize>>,
    next_probe: Cell<usize>,
    waits: Cell<usize>,
    fail_read: Cell<Option<usize>>,
    probe_reads: Cell<usize>,
    warmed: Cell<usize>,
    ready: RefCell<Vec<(TraversalOrdinal, FeatureId)>>,
}

impl<'a> Scheduled<'a> {
    fn new(bytes: &'a [u8], order: TraversalOrder) -> Self {
        Self {
            bytes,
            order,
            delay: None,
            cancel: false,
            plan: false,
            active: Cell::new(None),
            pending: Cell::new(None),
            next_probe: Cell::new(0),
            waits: Cell::new(0),
            fail_read: Cell::new(None),
            probe_reads: Cell::new(0),
            warmed: Cell::new(0),
            ready: RefCell::new(Vec::new()),
        }
    }
}

impl Source for Scheduled<'_> {
    fn range_query(&self) -> Option<&dyn RangeQuery<Self>> {
        Some(self)
    }

    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        if let Some(active) = self.active.get() {
            let reads = self.probe_reads.get() + 1;
            self.probe_reads.set(reads);
            if self.fail_read.get() == Some(reads) {
                self.fail_read.set(None);
                assert!(self.pending.replace(Some(active)).is_none());
                return Err(io::ErrorKind::WouldBlock.into());
            }
        }
        self.bytes.read_exact_at(offset, output)
    }
}

impl<'a> RangeQuery<Scheduled<'a>> for Scheduled<'a> {
    fn width(&self) -> usize {
        if self.plan { 0 } else { 4 }
    }

    fn allocate_probe(&self) -> usize {
        let probe = self.next_probe.get();
        self.next_probe.set(probe + 1);
        probe
    }

    fn begin(&self, id: usize) -> bool {
        if self.pending.get() == Some(id) {
            return false;
        }
        assert!(self.active.replace(Some(id)).is_none());
        true
    }

    fn pending(&self) -> bool {
        self.active.get().is_some() && self.active.get() == self.pending.get()
    }

    fn end(&self) {
        assert!(self.active.replace(None).is_some());
    }

    fn wait(&self) -> io::Result<()> {
        assert!(self.active.get().is_none());
        self.waits.set(self.waits.get() + 1);
        if self.cancel {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "test cancellation",
            ));
        }
        assert!(self.pending.replace(None).is_some());
        Ok(())
    }

    fn order(&self) -> TraversalOrder {
        self.order
    }

    fn feature_ready(&self, feature: Feature<'_, Self>, ordinal: TraversalOrdinal) {
        self.ready.borrow_mut().push((ordinal, feature.id()));
    }

    fn prepare_feature(&self, feature: Feature<'_, Self>) {
        if Some(feature.id()) == self.delay && self.waits.get() == 0 {
            let active = self.active.get().expect("active probe");
            assert!(self.pending.replace(Some(active)).is_none());
        }
    }

    fn visit_tile(&self, _start: u64) -> TileVisit {
        if self.plan {
            TileVisit::PlanOnly
        } else {
            TileVisit::Scan
        }
    }

    fn plan_width(&self) -> usize {
        if self.plan { 4 } else { 0 }
    }

    fn warm_pages(&self, pages: &[u64], unit: u64) -> io::Result<()> {
        assert!(unit > 0);
        assert!(pages.iter().all(|offset| offset % unit == 0));
        Ok(())
    }

    fn observe(&self, event: RangeQueryEvent) {
        if let RangeQueryEvent::TileExtent { start, length } = event {
            assert!(length > 0 && start + length <= self.len());
            self.warmed.set(self.warmed.get() + 1);
        }
    }
}

#[test]
fn ready_order_uses_restorable_ordinals_and_preserves_original_mode() {
    let query = Query::parse("n").unwrap();
    let ordinary = Library::new(GOL).unwrap();
    assert!(ordinary.source().range_query().is_none());
    let expected: Vec<_> = query
        .prepare(&ordinary)
        .unwrap()
        .in_bounds(Bounds::WORLD)
        .map(|f| f.unwrap().id())
        .collect();
    assert!(expected.len() > 1);

    for order in [TraversalOrder::Original, TraversalOrder::Ready] {
        let mut source = Scheduled::new(GOL, order);
        source.delay = Some(expected[0]);
        let library = Library::new(source).unwrap();
        let actual: Vec<_> = query
            .prepare(&library)
            .unwrap()
            .in_bounds(Bounds::WORLD)
            .map(|f| f.unwrap().id())
            .collect();
        let schedule = library.source();
        assert_eq!(schedule.waits.get(), 1);
        assert!(schedule.active.get().is_none());
        assert!(schedule.pending.get().is_none());
        if order == TraversalOrder::Original {
            assert_eq!(actual, expected);
            assert!(schedule.ready.borrow().is_empty());
        } else {
            assert_ne!(actual, expected);
            let mut ranked = schedule.ready.borrow().clone();
            assert!(ranked.iter().map(|entry| entry.1).eq(actual));
            ranked.sort_unstable();
            assert!(ranked.windows(2).all(|pair| pair[0].0 < pair[1].0));
            assert!(
                ranked
                    .into_iter()
                    .map(|entry| entry.1)
                    .eq(expected.iter().copied())
            );
        }
    }
}

#[test]
fn wait_cancellation_ends_the_active_probe() {
    let query = Query::parse("n").unwrap();
    let first = Library::new(GOL)
        .unwrap()
        .select_in(&query, Bounds::WORLD)
        .next()
        .unwrap()
        .unwrap()
        .id();
    let mut source = Scheduled::new(GOL, TraversalOrder::Original);
    source.delay = Some(first);
    source.cancel = true;
    let library = Library::new(source).unwrap();
    let error = query
        .prepare(&library)
        .unwrap()
        .in_bounds(Bounds::WORLD)
        .next()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("test cancellation"));
    assert!(library.source().active.get().is_none());
    assert_eq!(library.source().waits.get(), 1);
}

#[test]
fn checked_planning_warms_payloads_without_releasing_features() {
    let mut source = Scheduled::new(GOL, TraversalOrder::Original);
    source.plan = true;
    let library = Library::new(source).unwrap();
    let query = Query::parse("n").unwrap();
    assert!(library.select_in(&query, Bounds::WORLD).next().is_none());
    assert!(library.source().warmed.get() > 0);
    assert_eq!(library.source().next_probe.get(), 0);
}

#[test]
fn a_pending_traversal_read_retries_without_losing_or_repeating_features() {
    let query = Query::parse("n").unwrap();
    let ordinary = Library::new(GOL).unwrap();
    let expected: Vec<_> = query
        .prepare(&ordinary)
        .unwrap()
        .in_bounds(Bounds::WORLD)
        .map(|item| item.unwrap().id())
        .collect();
    let source = Scheduled::new(GOL, TraversalOrder::Original);
    source.fail_read.set(Some(6));
    let lib = Library::new(source).unwrap();
    let prepared = query.prepare(&lib).unwrap();
    for _ in 0..2 {
        let actual: Vec<_> = prepared
            .in_bounds(Bounds::WORLD)
            .map(|item| item.unwrap().id())
            .collect();
        assert_eq!(actual, expected);
    }
    assert_eq!(lib.source().waits.get(), 1);
    assert!(lib.source().active.get().is_none());
}
