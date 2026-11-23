use super::candidates::Candidates;
use regol::{Feature, FeatureId, FeatureType, Geometry, source::Source};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, collections::HashSet, ops::ControlFlow};

/// Geometry or count requested by an Overpass `out` statement.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OutputMode {
    #[default]
    Geom,
    Center,
    Count,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OutputDetail {
    Body,
    Skel,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum OutputOrder {
    Asc,
    Qt,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub nodes: usize,
    pub ways: usize,
    pub relations: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub areas: Option<usize>,
    pub total: usize,
}

impl Counts {
    pub fn add(&mut self, kind: FeatureType, mapped_relation: bool) {
        match kind {
            FeatureType::Node => self.nodes += 1,
            FeatureType::Way => self.ways += 1,
            FeatureType::Relation if mapped_relation => *self.areas.get_or_insert(0) += 1,
            FeatureType::Relation => self.relations += 1,
        }
        self.total += 1;
    }
}

/// A single output group in statement order, with its original name and modifiers.
pub struct ExecutedGroup<'a, S: Source> {
    pub name: String,
    pub mode: OutputMode,
    pub detail: Option<OutputDetail>,
    pub order: Option<OutputOrder>,
    pub data: GroupData<'a, S>,
}

/// Feature handles let consumers choose whether to read the query's output geometry or tags.
pub enum GroupData<'a, S: Source> {
    Features(Vec<Feature<'a, S>>),
    Count(Counts),
}

/// Metadata available when consuming a group or one of its features.
#[derive(Clone, Copy)]
pub struct OutputGroup<'a> {
    pub index: usize,
    pub name: &'a str,
    pub mode: OutputMode,
    pub detail: Option<OutputDetail>,
    pub order: Option<OutputOrder>,
    /// Whether this group comes from an explicit ordered output statement.
    pub ordered: bool,
}

/// What the executor should do after the consumer sees a matched feature.
pub enum FeatureStatus {
    Accepted,
    SkippedGeometry,
    Stop,
}

/// Work committed by one executed set in the original plan. Collected when a
/// structured request asks for `setStats`. Early-stopped queries report work so far.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetStats {
    pub set_index: usize,
    /// Candidate iterator reads, before pipeline filters and deduplication.
    pub candidates_read: usize,
    /// Matches accepted for output or reuse, counted once if used for both.
    pub matches_kept: usize,
    /// Feature handles stored for later sets or ordered output.
    pub retained: usize,
}

/// Distinct feature IDs whose geometry could not be used during execution.
#[derive(Default)]
pub struct ExecutionStats {
    pub sets: Vec<SetStats>,
    pub skipped_geometries: usize,
    pub skipped_references: usize,
}

/// Optional preprocessing for a matched feature. The weight bounds the bytes
/// retained by the executor while it waits to commit a batch in scan order.
pub trait FeaturePreparation<'a, S: Source>: Sync {
    type Prepared: Send;

    fn prepare(
        &self,
        feature: Feature<'a, S>,
        geometry: Option<Geometry>,
    ) -> (Self::Prepared, usize);
}

impl<S: Source> FeaturePreparation<'_, S> for () {
    type Prepared = ();

    fn prepare(&self, _: Feature<'_, S>, _: Option<Geometry>) -> ((), usize) {
        ((), 0)
    }
}

/// Consume query output in traversal order and optionally stop before reading
/// more results. Parallel preparation is optional and the final callbacks run
/// in the same order as the serial executor.
pub trait OutputSink<'a, S: Source> {
    type Prepared: Send;
    type Preparation: FeaturePreparation<'a, S, Prepared = Self::Prepared>;

    fn preparation(&self, _group: OutputGroup<'_>) -> Option<Self::Preparation> {
        None
    }

    /// Introduce a group. Count groups already contain their complete totals.
    fn group(
        &mut self,
        group: OutputGroup<'_>,
        counts: Option<Counts>,
    ) -> Result<ControlFlow<()>, String>;

    /// Consume a feature. Geometry already read by a spatial filter is passed
    /// here in serial execution and to `prepare` in parallel execution.
    fn feature(
        &mut self,
        group: OutputGroup<'_>,
        feature: Feature<'a, S>,
        geometry: Option<Geometry>,
        prepared: Option<Self::Prepared>,
    ) -> Result<FeatureStatus, String>;

    /// Observe usable geometry from a set used as a relationship operand.
    fn reference(&mut self, _geometry: &Geometry) {}
}

pub(crate) struct GroupCollector<'a, S: Source>(pub Vec<ExecutedGroup<'a, S>>);

impl<'a, S: Source> OutputSink<'a, S> for GroupCollector<'a, S> {
    type Prepared = ();
    type Preparation = ();

    fn group(
        &mut self,
        group: OutputGroup<'_>,
        counts: Option<Counts>,
    ) -> Result<ControlFlow<()>, String> {
        self.0.push(ExecutedGroup {
            name: group.name.into(),
            mode: group.mode,
            detail: group.detail,
            order: group.order,
            data: counts
                .map(GroupData::Count)
                .unwrap_or_else(|| GroupData::Features(Vec::new())),
        });
        Ok(ControlFlow::Continue(()))
    }

    fn feature(
        &mut self,
        group: OutputGroup<'_>,
        feature: Feature<'a, S>,
        _: Option<Geometry>,
        _: Option<()>,
    ) -> Result<FeatureStatus, String> {
        let GroupData::Features(features) = &mut self.0[group.index].data else {
            unreachable!()
        };
        features.push(feature);
        Ok(FeatureStatus::Accepted)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OutputStatement {
    pub(super) after: usize,
    pub(super) source: String,
    pub(super) name: String,
    pub(super) mode: OutputMode,
    pub(super) detail: Option<OutputDetail>,
    pub(super) order: Option<OutputOrder>,
}

impl OutputStatement {
    pub(super) fn validate(&self) -> Result<(), String> {
        let mut chars = self.name.bytes();
        if !chars
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
            || !chars.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err("Invalid Overpass output name.".into());
        }
        if self.mode == OutputMode::Count && (self.detail.is_some() || self.order.is_some()) {
            return Err("Use out count without other output modifiers.".into());
        }
        Ok(())
    }

    pub(super) fn counts<S: Source>(
        &self,
        features: &[Feature<'_, S>],
        mapped: bool,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Option<Counts>, String> {
        if self.mode != OutputMode::Count {
            return Ok(None);
        }
        let mut counts = Counts {
            areas: mapped.then_some(0),
            ..Counts::default()
        };
        for feature in features {
            check()?;
            counts.add(feature.kind(), mapped);
        }
        Ok(Some(counts))
    }

    pub(super) fn ordered_features<'f, 'a, S: Source>(
        &self,
        features: &'f [Feature<'a, S>],
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Cow<'f, [Feature<'a, S>]>, String> {
        match self.order {
            None => Ok(Cow::Borrowed(features)),
            Some(OutputOrder::Asc) => {
                let mut sorted = features.to_vec();
                sorted.sort_unstable_by_key(|f| f.id());
                check()?;
                Ok(Cow::Owned(sorted))
            }
            Some(OutputOrder::Qt) => {
                let mut sorted = Vec::with_capacity(features.len());
                for feature in features {
                    check()?;
                    let tile = quadtile(*feature).map_err(|e| format!("{}: {e}", feature.id()))?;
                    sorted.push(((feature.kind(), tile, feature.id()), *feature));
                }
                sorted.sort_unstable_by_key(|(key, _)| *key);
                check()?;
                Ok(Cow::Owned(sorted.into_iter().map(|(_, f)| f).collect()))
            }
        }
    }

    pub(super) fn emit<'a, S: Source, C: Fn() -> Result<(), String>>(
        &self,
        index: usize,
        features: &[Feature<'a, S>],
        mapped: bool,
        sink: &mut impl OutputSink<'a, S>,
        check: &C,
        executor: &impl Candidates<S, C>,
    ) -> Result<(bool, HashSet<FeatureId>), String> {
        check()?;
        let mut skipped = HashSet::new();
        let group = OutputGroup {
            index,
            name: &self.name,
            mode: self.mode,
            detail: self.detail,
            order: self.order,
            ordered: true,
        };
        let counts = self.counts(features, mapped, check)?;
        if sink.group(group, counts)?.is_break() {
            return Ok((true, skipped));
        }
        if self.mode == OutputMode::Count {
            return Ok((false, skipped));
        }
        let ordered = self.ordered_features(features, check)?;
        let preparation = sink.preparation(group);
        let mut stopped = false;
        executor.visit(
            ordered.iter().copied().map(Ok),
            &[],
            preparation.as_ref(),
            check,
            |candidate, prepared| {
                check()?;
                let feature = candidate.map_err(|error| error.to_string())?;
                match sink.feature(group, feature, None, prepared.and_then(|work| work.output))? {
                    FeatureStatus::Accepted => {}
                    FeatureStatus::SkippedGeometry => {
                        skipped.insert(feature.id());
                    }
                    FeatureStatus::Stop => {
                        stopped = true;
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Ok(ControlFlow::Continue(()))
            },
        )?;
        Ok((stopped, skipped))
    }
}

// Interleave the projected center bits for GOL-local quadtile order.
fn quadtile<S: Source>(f: Feature<'_, S>) -> regol::Result<u64> {
    fn spread(mut v: u64) -> u64 {
        v = (v | (v << 16)) & 0x0000_ffff_0000_ffff;
        v = (v | (v << 8)) & 0x00ff_00ff_00ff_00ff;
        v = (v | (v << 4)) & 0x0f0f_0f0f_0f0f_0f0f;
        v = (v | (v << 2)) & 0x3333_3333_3333_3333;
        (v | (v << 1)) & 0x5555_5555_5555_5555
    }
    fn center(min: i32, max: i32) -> u64 {
        (i64::from(min).midpoint(i64::from(max)) - i64::from(i32::MIN)) as u64
    }
    let b = f.bounds()?;
    Ok(spread(center(b.min.x, b.max.x)) | spread(center(b.min.y, b.max.y)) << 1)
}
