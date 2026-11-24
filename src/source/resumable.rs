//! Optional cooperative scheduling for sources whose range reads overlap.
use super::Source;
use crate::Feature;
use std::io;

/// Select which tile payloads a bounded query visits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TileVisit {
    /// Traverse this payload after enumerating its children.
    Scan,
    /// Continue with its children without opening this payload.
    Skip,
    /// Warm checked index ranges without emitting features from this payload.
    PlanOnly,
}

/// Release order for features that finished their range probes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TraversalOrder {
    /// Release in the same tile and record order as a synchronous traversal.
    #[default]
    Original,
    /// Release as soon as a complete feature is available. Use the ordinal
    /// notified by [`RangeQuery::feature_ready`] to reconstruct the order.
    /// A caller that stops early may observe a different result prefix.
    Ready,
}

/// A feature's original sequence within one bounded scan, ordered by tile
/// then by the order of matching features in that tile.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TraversalOrdinal {
    pub tile: u32,
    pub feature: u32,
}

/// Observe progress of a resumable bounded scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum RangeQueryEvent {
    /// A checked index pass found the bounds of a payload in the byte source.
    TileExtent { start: u64, length: u64 },
    /// The payload traversal and its output read probes have completed.
    TileCompleted { start: u64 },
    /// Number of entries waiting for a probe or for delivery to the caller.
    Buffered { entries: usize },
}

/// Scheduling hooks for a [`Source`] with a shared range cache.
///
/// During a probe, an uncached read may queue a range and report an I/O error.
/// The source must then report [`Self::pending`] for that probe. The engine
/// ends the probe and retries from its last checkpoint once [`Self::begin`]
/// succeeds again. Unmarked read failures are ordinary query errors. A source
/// must retain or validate completed ranges across retries.
///
/// Prepared queries use resumable probes for bounded scans. Opening reads,
/// queries without bounds, and unprepared scans use ordinary source reads.
/// [`Self::width`] and [`Self::plan_width`] choose which part of the protocol
/// is active for the current request. Keep the same adapter, width, and
/// release order until its scan completes or is
/// dropped. Manage the lifetime of any outstanding tickets in the source.
pub trait RangeQuery<S: Source> {
    /// Maximum number of active tile cursors. Zero requests a regular scan.
    fn width(&self) -> usize;

    /// Allocate an identifier distinct from every outstanding probe in this source.
    fn allocate_probe(&self) -> usize;

    /// Enter the probe, or return false if its previous read is still pending.
    fn begin(&self, probe: usize) -> bool;

    /// Return true after an uncached read queued a ticket in the active probe.
    fn pending(&self) -> bool;

    /// End the probe, retaining any queued ticket for future attempts.
    fn end(&self);

    /// Wait until at least one outstanding probe can retry. Errors terminate
    /// the query, including cancellation and a failing underlying range.
    fn wait(&self) -> io::Result<()>;

    /// Choose how completed features are released to the caller.
    fn order(&self) -> TraversalOrder {
        TraversalOrder::Original
    }

    /// Let the caller associate the selected feature with its traversal ordinal.
    /// Called immediately before returning a successful ready-first item.
    fn feature_ready(&self, _feature: Feature<'_, S>, _ordinal: TraversalOrdinal) {}

    /// Optionally touch fields the caller expects to read after selection.
    /// A pending probe parks this feature and allows other work to continue.
    fn prepare_feature(&self, _feature: Feature<'_, S>) {}

    /// Choose whether to scan, skip or warm this tile payload.
    fn visit_tile(&self, _start: u64) -> TileVisit {
        TileVisit::Scan
    }

    /// Maximum number of neighboring tiles in a checked index prefetch group.
    fn plan_width(&self) -> usize {
        0
    }

    /// Include discovered tag table pages in an index prefetch group.
    fn plan_tags(&self) -> bool {
        false
    }

    /// Number of aligned bytes represented by each prefetch offset.
    fn plan_unit(&self, _headers: bool, _tiles: usize) -> u64 {
        128 * 1024
    }

    /// Complete any required prefetch reads before returning.
    fn warm_pages(&self, _pages: &[u64], _unit: u64) -> io::Result<()> {
        Ok(())
    }

    /// Receive an optional progress notification.
    fn observe(&self, _event: RangeQueryEvent) {}
}
