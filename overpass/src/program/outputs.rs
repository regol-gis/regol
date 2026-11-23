use regol::{Feature, FeatureType, source::Source};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

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

    pub(super) fn group<'a, S: Source>(&self, data: GroupData<'a, S>) -> ExecutedGroup<'a, S> {
        ExecutedGroup {
            name: self.name.clone(),
            mode: self.mode,
            detail: self.detail,
            order: self.order,
            data,
        }
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

// The evaluator commits the same ordered results through either output adapter.
use super::{
    Plan,
    candidates::Candidates,
    live::{Display, LiveOutput, Rendered},
};
use crate::geometry::GeoGeometry;
use regol::{FeatureId, Geometry};
use std::collections::HashSet;

pub(crate) enum Sink<'a, S: Source> {
    Live(LiveOutput),
    Groups(Vec<ExecutedGroup<'a, S>>),
}

impl<'a, S: Source> Sink<'a, S> {
    pub(crate) fn groups(plan: &Plan) -> Self {
        let groups = if plan.output == OutputMode::Count {
            plan.show
                .first()
                .map(|name| ExecutedGroup {
                    name: plan.output_name.as_ref().unwrap_or(name).clone(),
                    mode: plan.output,
                    detail: None,
                    order: None,
                    data: GroupData::Count(Counts::default()),
                })
                .into_iter()
                .collect()
        } else {
            plan.show
                .iter()
                .map(|name| ExecutedGroup {
                    name: plan.output_name.as_ref().unwrap_or(name).clone(),
                    mode: plan.output,
                    detail: None,
                    order: None,
                    data: GroupData::Features(Vec::new()),
                })
                .collect()
        };
        Self::Groups(groups)
    }

    pub(super) fn display(&self, mode: OutputMode, group: usize) -> Option<Display> {
        match self {
            Self::Live(_) => Display::new(mode, group, true, false),
            Self::Groups(_) => None,
        }
    }

    pub(super) fn limited(&self) -> bool {
        matches!(self, Self::Live(live) if live.output.limited.is_some())
    }

    pub(super) fn references(&mut self) -> Option<&mut Vec<GeoGeometry>> {
        match self {
            Self::Live(live) => live.output.references.as_mut(),
            Self::Groups(_) => None,
        }
    }

    pub(super) fn append(
        &mut self,
        group: usize,
        feature: Feature<'a, S>,
        geometry: Option<Geometry>,
        prepared: Option<regol::Result<Rendered>>,
        display: Option<Display>,
        skipped: &mut HashSet<FeatureId>,
    ) -> Result<bool, String> {
        match self {
            Self::Live(live) => live.append(feature, geometry, prepared, display.unwrap(), skipped),
            Self::Groups(groups) => {
                let GroupData::Features(features) = &mut groups[group].data else {
                    unreachable!()
                };
                features.push(feature);
                Ok(true)
            }
        }
    }

    pub(super) fn emit<C: Fn() -> Result<(), String>>(
        &mut self,
        statement: &OutputStatement,
        features: &[Feature<'a, S>],
        mapped: bool,
        skipped: &mut HashSet<FeatureId>,
        check: &C,
        executor: &impl Candidates<S, C>,
    ) -> Result<(), String> {
        check()?;
        let counts = statement.counts(features, mapped, check)?;
        match self {
            Self::Live(live) => {
                if let Some(display) = live.start_group(statement, counts)? {
                    let ordered = statement.ordered_features(features, check)?;
                    live.append_group(&ordered, display, skipped, check, executor)?;
                }
            }
            Self::Groups(groups) => {
                let data = if let Some(counts) = counts {
                    GroupData::Count(counts)
                } else {
                    GroupData::Features(statement.ordered_features(features, check)?.into_owned())
                };
                groups.push(statement.group(data));
            }
        }
        Ok(())
    }

    pub(super) fn finish(&mut self, counts: Option<Counts>, skipped: usize, skipped_refs: usize) {
        match self {
            Self::Live(live) => {
                live.output.counts = counts;
                live.output.skipped_geometries = skipped;
                live.output.skipped_references = skipped_refs;
            }
            Self::Groups(groups) => {
                if let Some(counts) = counts {
                    groups[0].data = GroupData::Count(counts);
                }
            }
        }
    }
}
