use crate::source::FaultSample;
use std::sync::atomic::{AtomicBool, Ordering};

/// Each tile group samples its own worker. Once any worker stalls, all
/// remaining workers in this query can overlap reads with filtering.
pub(super) struct PartitionProbe<'a> {
    active: &'a AtomicBool,
    baseline: Option<FaultSample>,
    tiles_started: usize,
}

impl<'a> PartitionProbe<'a> {
    pub(super) fn new(active: &'a AtomicBool) -> Self {
        Self {
            active,
            baseline: if active.load(Ordering::Relaxed) {
                None
            } else {
                FaultSample::now()
            },
            tiles_started: 0,
        }
    }

    pub(super) fn before_tile(&mut self) -> bool {
        if self.active.load(Ordering::Relaxed) {
            return true;
        }
        // Check after the first completed tile, then every four tiles.
        let sample_due = self.tiles_started % 4 == 1;
        self.tiles_started += 1;
        sample_due && self.observe()
    }

    pub(super) fn finish(self) {
        if !self.active.load(Ordering::Relaxed) {
            self.observe();
        }
    }

    fn observe(&self) -> bool {
        let stalled = self
            .baseline
            .as_ref()
            .is_some_and(FaultSample::observed_stall);
        if stalled {
            self.active.store(true, Ordering::Relaxed);
        }
        stalled
    }
}
