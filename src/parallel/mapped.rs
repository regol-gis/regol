use crate::source::FaultSample;
use std::sync::OnceLock;

/// The serial prefix and tile groups share the first conclusive observation.
pub(super) struct PartitionProbe<'a> {
    decision: &'a OnceLock<bool>,
    baseline: Option<FaultSample>,
    tiles_started: usize,
}

impl<'a> PartitionProbe<'a> {
    pub(super) fn new(decision: &'a OnceLock<bool>) -> Self {
        Self {
            decision,
            baseline: if decision.get().is_some() {
                None
            } else {
                FaultSample::now()
            },
            tiles_started: 0,
        }
    }

    pub(super) fn before_tile(&mut self) -> bool {
        if let Some(&enabled) = self.decision.get() {
            return enabled;
        }
        // Check after the first completed tile, then every four tiles.
        let sample_due = self.tiles_started % 4 == 1;
        self.tiles_started += 1;
        sample_due && self.observe()
    }

    pub(super) fn finish(self) {
        if self.decision.get().is_none() {
            self.observe();
        }
    }

    fn observe(&self) -> bool {
        if let Some(enabled) = self.baseline.as_ref().and_then(FaultSample::speculate) {
            return *self.decision.get_or_init(|| enabled);
        }
        self.decision.get().copied().unwrap_or(false)
    }
}
