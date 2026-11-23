pub(crate) mod candidates;
mod control;
pub(crate) mod live;
mod membership;
pub(crate) mod outputs;
mod recurse;
use crate::{
    Counts, MAX_INTERMEDIATE_BYTES, MAX_INTERMEDIATE_FEATURES, MAX_VERTICES, QueryProgress,
    geometry::{self, GeoGeometry},
    overpass::{
        SpatialMatcher,
        scan::{CandidateScan, node_area_windows},
    },
    relationship::{Matcher, predicate},
    usable_geometry,
};
use candidates::{Candidates, matches};
use membership::Membership;
use outputs::{OutputMode, OutputStatement, Sink};
use regol::{Bounds, Feature, FeatureId, Library, Query, SpatialFilter, source::Source};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

// Bound the hash table for counts that merge overlapping candidate streams.
const MAX_COUNTED_IDS: usize = 750_000;
const AREA_SET_USAGE: &str =
    "Use area sets in an (area) filter, area union or difference, or with out count.";
const COUNT_MEMORY_ERROR: &str = "Counting exceeds the deduplication memory budget. Narrow the selectors or use a smaller bounding box.";

#[derive(Deserialize)]
pub(crate) struct Plan {
    pub sets: Vec<Set>,
    #[serde(default)]
    pub show: Vec<String>,
    pub geometry: Option<GeoGeometry>,
    #[serde(default)]
    pub output: OutputMode,
    #[serde(rename = "outputName")]
    pub output_name: Option<String>,
    #[serde(default)]
    pub outputs: Vec<OutputStatement>,
    pub timeout: Option<u64>,
}
#[derive(Deserialize)]
pub(crate) struct Set {
    pub name: String,
    pub source: String,
    pub overpass: Option<crate::overpass::Selection>,
    #[serde(default)]
    pub union: Vec<String>,
    pub difference: Option<[String; 2]>,
    #[serde(default)]
    pub intersect: Vec<String>,
    pub membership: Option<Membership>,
    pub recurse: Option<String>,
    pub control: Option<control::Loop>,
    #[serde(default)]
    pub map_to_area: bool,
    #[serde(default)]
    pub filters: Vec<Filter>,
}
impl Set {
    pub fn new(name: &str, source: String) -> Self {
        Self {
            name: name.into(),
            source,
            filters: Vec::new(),
            overpass: None,
            union: Vec::new(),
            difference: None,
            intersect: Vec::new(),
            membership: None,
            recurse: None,
            control: None,
            map_to_area: false,
        }
    }
    fn set_inputs(&self) -> impl Iterator<Item = &str> {
        self.source
            .strip_prefix('$')
            .into_iter()
            .chain(self.union.iter().map(String::as_str))
            .chain(self.difference.iter().flatten().map(String::as_str))
            .chain(self.intersect.iter().map(String::as_str))
            .chain(self.membership.iter().map(|m| m.reference.as_str()))
            .chain(self.recurse.as_deref())
            .chain(self.control.iter().flat_map(control::Loop::inputs))
    }
    fn inputs(&self) -> impl Iterator<Item = &str> {
        self.feature_inputs()
            .chain(self.overpass.iter().filter_map(|s| s.area.as_deref()))
    }
    fn feature_inputs(&self) -> impl Iterator<Item = &str> {
        self.set_inputs()
            .chain(self.filters.iter().map(|f| f.reference.as_str()))
            .chain(self.overpass.iter().filter_map(|s| s.around_reference()))
    }
}
#[derive(Deserialize)]
pub(crate) struct Filter {
    pub predicate: String,
    pub reference: String,
    pub meters: Option<f64>,
}

fn feature_ids<S: Source>(
    features: &[Feature<'_, S>],
    check: &impl Fn() -> Result<(), String>,
) -> Result<HashSet<FeatureId>, String> {
    let mut ids = HashSet::with_capacity(features.len());
    for feature in features {
        check()?;
        ids.insert(feature.id());
    }
    Ok(ids)
}

fn remember_counted_id(
    ids: &mut HashSet<(bool, FeatureId)>,
    id: (bool, FeatureId),
    limit: usize,
) -> Result<(), String> {
    if ids.len() >= limit {
        return Err(COUNT_MEMORY_ERROR.into());
    }
    ids.try_reserve(1).map_err(|_| COUNT_MEMORY_ERROR)?;
    ids.insert(id);
    Ok(())
}

impl Plan {
    fn parse_selectors<'a>(
        &'a self,
        mapped_areas: &mut HashSet<&'a str>,
    ) -> Result<Vec<Option<Query>>, String> {
        // Validate every declaration, even when show() leaves a branch unused.
        let mut names = HashMap::new();
        let mut queries = Vec::new();
        for (index, set) in self.sets.iter().enumerate() {
            if set.name.is_empty()
                || set.name == "geometry"
                || names.contains_key(set.name.as_str())
            {
                return Err(format!("Invalid or repeated set name ${}.", set.name));
            }
            let block = !set.union.is_empty() || set.difference.is_some();
            let traversal = set.membership.is_some();
            let recurse = set.recurse.is_some();
            let looping = set.control.is_some();
            let from_set = set.source.starts_with('$');
            if (!set.union.is_empty() && set.difference.is_some())
                || (block && (set.overpass.is_some() || traversal))
                || ((block || traversal || recurse || looping) && !set.source.is_empty())
                || (recurse && (block || traversal || set.overpass.is_some()))
                || (looping
                    && (block
                        || traversal
                        || recurse
                        || set.overpass.is_some()
                        || !set.filters.is_empty()))
                || (traversal && set.overpass.is_none())
                || (!set.intersect.is_empty() && !from_set && !traversal)
                || (set.overpass.is_some() && !traversal && set.source != "*" && !from_set)
                || (set.map_to_area
                    && (!from_set
                        || set.overpass.is_some()
                        || !set.intersect.is_empty()
                        || !set.filters.is_empty()))
            {
                return Err("Invalid Overpass set source.".into());
            }
            for input in set.inputs() {
                if input != "geometry" && !names.contains_key(input) {
                    return Err(format!("${} uses undefined set ${input}.", set.name));
                }
            }
            if set.set_inputs().any(|input| input == "geometry") {
                return Err("Use $geometry as a spatial reference or in show().".into());
            }
            let area_block = block && set.set_inputs().any(|input| mapped_areas.contains(input));
            if area_block && !set.set_inputs().all(|input| mapped_areas.contains(input)) {
                return Err(
                    "A set block must contain either all area sets or all ordinary element sets."
                        .into(),
                );
            }
            if (area_block && !set.filters.is_empty())
                || (!set.map_to_area
                    && !area_block
                    && set
                        .feature_inputs()
                        .any(|input| mapped_areas.contains(input)))
            {
                return Err(AREA_SET_USAGE.into());
            }
            for filter in &set.filters {
                predicate(&filter.predicate, filter.meters)?;
            }
            if let Some(selection) = &set.overpass {
                selection.validate()?;
                if let Some(membership) = &set.membership {
                    membership.validate(selection)?;
                }
                if !from_set && !traversal && !selection.is_bounded() {
                    return Err("An Overpass selection needs a bounded search extent.".into());
                }
                if selection.area.as_deref() == Some("geometry")
                    || selection.around_reference() == Some("geometry")
                {
                    return Err("Overpass spatial filters use OSM sets.".into());
                }
            }
            if let Some(control) = &set.control {
                control.validate()?;
            }
            let q = if from_set || block || traversal || recurse || looping {
                None
            } else if let Some(selection) = &set.overpass {
                Some(selection.query())
            } else {
                Some(Query::parse(&set.source).map_err(|e| format!("${}: {e}", set.name))?)
            };
            queries.push(q);
            names.insert(set.name.as_str(), index);
            if set.map_to_area || area_block {
                mapped_areas.insert(set.name.as_str());
            }
        }
        if !self.outputs.is_empty() {
            if !self.show.is_empty() || self.output != OutputMode::Geom {
                return Err("Supply either show() or ordered Overpass outputs.".into());
            }
            let mut previous = 0;
            for output in &self.outputs {
                output.validate()?;
                if output.after < previous || output.after > self.sets.len() {
                    return Err("Invalid Overpass output statement position.".into());
                }
                let name = output.source.as_str();
                if names.get(name).is_none_or(|index| *index >= output.after) {
                    return Err(format!("Undefined set ${name} in out statement."));
                }
                if output.mode != OutputMode::Count && mapped_areas.contains(name) {
                    return Err(AREA_SET_USAGE.into());
                }
                previous = output.after;
            }
        } else if self.show.is_empty() {
            return Err("Finish with show($set).".into());
        }
        let mut needed = HashSet::new();
        for name in &self.show {
            if name != "geometry" && !names.contains_key(name.as_str()) {
                return Err(format!("Undefined set ${name} in show()."));
            }
            if !needed.insert(name.as_str()) {
                return Err(format!("Repeated set ${name} in show()."));
            }
            if self.output != OutputMode::Count && mapped_areas.contains(name.as_str()) {
                return Err(AREA_SET_USAGE.into());
            }
        }
        Ok(queries)
    }
    pub fn execute<'a, S: Source, C: Fn() -> Result<(), String>>(
        &self,
        lib: &'a Library<S>,
        windows: &[Bounds],
        sink: &mut Sink<'a, S>,
        check: &C,
        executor: &impl Candidates<S, C>,
        progress: &impl Fn(QueryProgress),
    ) -> Result<(), String> {
        let mut mapped_areas = HashSet::new();
        let queries = self.parse_selectors(&mut mapped_areas)?;
        let mut needed: HashSet<&str> = self.show.iter().map(String::as_str).collect();
        let mut last_use = HashMap::new();
        for output in &self.outputs {
            needed.insert(output.source.as_str());
            last_use.insert(output.source.as_str(), output.after);
        }
        let mut referenced = HashSet::new();
        for (i, set) in self.sets.iter().enumerate().rev() {
            if needed.contains(set.name.as_str()) {
                for input in set.inputs() {
                    needed.insert(input);
                    referenced.insert(input);
                    last_use
                        .entry(input)
                        .and_modify(|last| *last = (*last).max(i))
                        .or_insert(i);
                }
            }
        }
        let attached = if needed.contains("geometry") {
            Some(
                self.geometry
                    .as_ref()
                    .ok_or("Draw, paste or pick a feature for $geometry.")?
                    .project()?,
            )
        } else {
            None
        };
        let mut counts = (self.output == OutputMode::Count).then(Counts::default);
        let mut stored: HashMap<&str, Vec<Feature<'a, S>>> = HashMap::new();
        let mut shown = HashSet::new();
        let mut counted = HashSet::new();
        let mut skipped = HashSet::new();
        let mut skipped_refs = HashSet::new();
        let mut pending = self.outputs.iter().peekable();
        let mut sets = self.sets.iter().zip(&queries);
        for i in 0..=self.sets.len() {
            check()?;
            while let Some(output) = pending.next_if(|output| output.after == i) {
                sink.emit(
                    output,
                    &stored[output.source.as_str()],
                    mapped_areas.contains(output.source.as_str()),
                    &mut skipped,
                    check,
                    executor,
                )?;
                if sink.limited() {
                    break;
                }
            }
            if sink.limited() {
                break;
            }
            let Some((set, query)) = sets.next() else {
                break;
            };
            if !needed.contains(set.name.as_str()) {
                stored.retain(|name, _| last_use[name] > i);
                continue;
            }
            let mut scan = QueryProgress {
                set_index: i,
                candidates_read: 0,
            };
            progress(scan);
            let keep = last_use.contains_key(set.name.as_str());
            let group = self.show.iter().position(|n| n == &set.name);
            let display = group.and_then(|group| sink.display(self.output, group));
            let mapped = mapped_areas.contains(set.name.as_str());
            if mapped
                && group.is_some()
                && let Some(counts) = &mut counts
            {
                counts.areas.get_or_insert(0);
            }
            let mut filters = Vec::new();
            let selection = set.overpass.as_ref();
            let mut filter_vertices = 0;
            let mut bad_refs = HashSet::new();
            let overpass_boxes = selection.map(|s| s.bounds()).transpose()?;
            let exact_boxes = overpass_boxes.as_deref().unwrap_or_default();
            let mut dynamic_boxes = selection
                .and_then(|s| s.named_area.as_ref())
                .map(|lookup| lookup.windows(lib.source()))
                .transpose()?;
            let mut supplemental = None;
            let mut split_node_scan = false;
            if let Some(selection) = selection {
                let spatial = selection
                    .spatial_filters(
                        &stored,
                        &mapped_areas,
                        &mut bad_refs,
                        &mut filter_vertices,
                        check,
                    )
                    .map_err(|e| format!("${}: {e}", set.name))?;
                let empty = spatial.iter().any(SpatialMatcher::is_empty);
                if !empty && query.is_some() && selection.named_area.is_none() {
                    split_node_scan = selection.is_node_only()
                        && spatial
                            .iter()
                            .any(|matcher| matches!(matcher, SpatialMatcher::Area(_)));
                    if exact_boxes.is_empty()
                        && let Some(reference) = spatial.first()
                    {
                        dynamic_boxes = Some(reference.windows(check)?);
                    }
                    if let Some(paths) = selection.path_query() {
                        let scan = if let Some(boxes) = &mut dynamic_boxes
                            && matches!(spatial.first(), Some(SpatialMatcher::Around(_)))
                            && boxes != &[Bounds::WORLD]
                        {
                            Some(CandidateScan::around(boxes))
                        } else {
                            CandidateScan::planar(dynamic_boxes.as_deref().unwrap_or(exact_boxes))
                        };
                        supplemental = scan.map(|scan| (paths, scan));
                    }
                }
                if empty {
                    filters.push(Matcher::Never);
                } else {
                    filters.extend(spatial.into_iter().map(Matcher::Overpass));
                }
            }
            if set.map_to_area {
                filters.push(Matcher::Overpass(SpatialMatcher::MapToArea));
            }
            let scan_windows = dynamic_boxes
                .as_deref()
                .or(overpass_boxes.as_deref())
                .unwrap_or(windows);
            let node_windows = split_node_scan
                .then(|| node_area_windows(scan_windows, check))
                .transpose()?;
            let local_windows = node_windows.as_deref().unwrap_or(scan_windows);
            for filter in &set.filters {
                let p = predicate(&filter.predicate, filter.meters)?;
                let matcher = if filter.reference == "geometry" {
                    let geometry = attached.as_ref().unwrap();
                    filter_vertices += geometry::vertices(geometry);
                    if filter_vertices > MAX_VERTICES {
                        return Err(format!("${}: reference vertex limit reached.", set.name));
                    }
                    Matcher::Single(
                        SpatialFilter::new(p, geometry).map_err(|e| format!("$geometry: {e}"))?,
                    )
                } else {
                    Matcher::from_set(
                        p,
                        &stored[filter.reference.as_str()],
                        &mut bad_refs,
                        sink.references(),
                        &mut filter_vertices,
                        check,
                    )
                    .map_err(|e| format!("${}: {e}", filter.reference))?
                };
                filters.push(matcher);
            }
            skipped_refs.extend(bad_refs.iter().copied());
            skipped.extend(&bad_refs);
            let mut members = Vec::new();
            if !filters.iter().any(|f| matches!(f, Matcher::Never)) {
                let prepared = query
                    .as_ref()
                    .map(|q| q.prepare(lib))
                    .transpose()
                    .map_err(|e| format!("${}: {e}", set.name))?;
                let supplemental_query = supplemental
                    .as_ref()
                    .map(|(query, _)| query.prepare(lib))
                    .transpose()
                    .map_err(|e| format!("${}: {e}", set.name))?;
                // Regol suppresses physical tile replicas within one bounded scan.
                // The supplemental scan emits each feature once and excludes local candidates.
                let stream_count = self.output == OutputMode::Count
                    && group.is_some()
                    && self.show.len() == 1
                    && prepared.is_some()
                    && scan_windows.len() == 1;
                let parent_ways;
                let candidates: Box<dyn Iterator<Item = regol::Result<Feature<'_, S>>> + '_> =
                    if let Some(q) = &prepared {
                        let local = local_windows.iter().flat_map(|&window| {
                            let cancelled = check()
                                .err()
                                .map(|error| Err(std::io::Error::other(error).into()));
                            cancelled.into_iter().chain(q.in_bounds(window))
                        });
                        if let (Some((_, scan)), Some(paths)) = (&supplemental, &supplemental_query)
                        {
                            let local = local.filter_map(|candidate| {
                                let result = (|| {
                                    check().map_err(std::io::Error::other)?;
                                    let feature = candidate?;
                                    Ok(scan.is_local(feature)?.then_some(feature))
                                })();
                                result.transpose()
                            });
                            Box::new(local.chain(scan.wide_candidates(paths, scan_windows, check)))
                        } else {
                            Box::new(local)
                        }
                    } else if !set.union.is_empty() {
                        Box::new(
                            set.union
                                .iter()
                                .flat_map(|name| stored[name.as_str()].iter().copied().map(Ok)),
                        )
                    } else if let Some([first, second]) = &set.difference {
                        let excluded = feature_ids(&stored[second.as_str()], check)?;
                        Box::new(
                            stored[first.as_str()]
                                .iter()
                                .filter(move |f| !excluded.contains(&f.id()))
                                .copied()
                                .map(Ok),
                        )
                    } else if let Some(membership) = &set.membership {
                        let selection = selection.unwrap();
                        parent_ways = membership
                            .prepare_parent_ways(lib, selection)
                            .map_err(|e| format!("${}: {e}", set.name))?;
                        membership.candidates(
                            &stored[membership.reference.as_str()],
                            selection,
                            parent_ways.as_ref(),
                            lib,
                            check,
                        )?
                    } else if let Some(reference) = &set.recurse {
                        Box::new(recurse::candidates(
                            &stored[reference.as_str()],
                            lib.limits().max_traversal_steps,
                            check,
                        ))
                    } else if let Some(control) = &set.control {
                        Box::new(control.run(&stored, check)?.into_iter().map(Ok))
                    } else {
                        Box::new(stored[&set.source[1..]].iter().copied().map(Ok))
                    };
                let candidates = if let Some((first, others)) = set.intersect.split_first() {
                    let mut ids = feature_ids(&stored[first.as_str()], check)?;
                    for name in others {
                        let mut next = HashSet::new();
                        for f in &stored[name.as_str()] {
                            check()?;
                            if ids.contains(&f.id()) {
                                next.insert(f.id());
                            }
                        }
                        ids = next;
                    }
                    Box::new(candidates.filter(move |f| match f {
                        Ok(f) => ids.contains(&f.id()),
                        Err(_) => true,
                    })) as Box<dyn Iterator<Item = _>>
                } else {
                    candidates
                };
                let candidates = if let Some(selection) = selection {
                    Box::new(candidates.filter_map(|candidate| {
                        if let Err(message) = check() {
                            return Some(Err(std::io::Error::other(message).into()));
                        }
                        match candidate.and_then(|f| {
                            crate::with_geometry_reads(f, |f| selection.matches(f, exact_boxes))
                                .map(|yes| yes.then_some(f))
                        }) {
                            Ok(feature) => feature.map(Ok),
                            Err(error) => Some(Err(error)),
                        }
                    })) as Box<dyn Iterator<Item = _>>
                } else {
                    candidates
                };
                let mut seen = HashSet::new();
                let mut bad = HashSet::new();
                let candidates = candidates.inspect(|_| {
                    scan.candidates_read += 1;
                    if scan.candidates_read == 1 || scan.candidates_read.is_multiple_of(256) {
                        progress(scan);
                    }
                });
                executor.visit(candidates, &filters, display, check, |candidate, prepared| {
                    check()?;
                    let f = candidate.map_err(|e| format!("${}: {e}", set.name))?;
                    let id = f.id();
                    if seen.contains(&id) || bad.contains(&id) || bad_refs.contains(&id) {
                        return Ok(ControlFlow::Continue(()));
                    }
                    let mapped_relation = mapped && id.kind == regol::FeatureType::Relation;
                    let output_group = group.filter(|_| {
                        if self.output == OutputMode::Count {
                            stream_count || !counted.contains(&(mapped_relation, id))
                        } else {
                            !shown.contains(&id)
                        }
                    });
                    if !keep && output_group.is_none() {
                        return Ok(ControlFlow::Continue(()));
                    }
                    let mut g = None;
                    let (matched, prepared_display) = match prepared {
                        Some(work) => (work.matched, work.display),
                        None => (matches(f, &filters, &mut g, check), None),
                    };
                    if usable_geometry(id, matched, &mut bad)? != Some(true) {
                        return Ok(ControlFlow::Continue(()));
                    }
                    if keep && members.len() == MAX_INTERMEDIATE_FEATURES {
                        return Err(format!(
                            "${}: intermediate feature limit reached. Narrow the set or zoom in.",
                            set.name
                        ));
                    }
                    if let Some(group) = output_group {
                        if let Some(counts) = &mut counts {
                            if !stream_count {
                                remember_counted_id(
                                    &mut counted,
                                    (mapped_relation, id),
                                    MAX_COUNTED_IDS,
                                )?;
                            }
                            counts.add(id.kind, mapped_relation);
                        } else {
                            let bad_display = if self.output == OutputMode::Center {
                                &mut skipped
                            } else {
                                &mut bad
                            };
                            if sink.append(group, f, g, prepared_display, display, bad_display)? {
                                shown.insert(id);
                            }
                            if sink.limited() {
                                return Ok(ControlFlow::Break(()));
                            }
                            if bad.contains(&id) {
                                return Ok(ControlFlow::Continue(()));
                            }
                        }
                    }
                    if keep || self.output != OutputMode::Count {
                        seen.insert(id);
                    }
                    if keep {
                        members.push(f);
                    }
                    Ok(ControlFlow::Continue(()))
                })?;
                if referenced.contains(set.name.as_str()) {
                    skipped_refs.extend(bad.iter().copied());
                }
                skipped.extend(bad);
            }
            stored.retain(|name, _| last_use[name] > i);
            if keep {
                let handles = members.len() + stored.values().map(Vec::len).sum::<usize>();
                if handles > MAX_INTERMEDIATE_BYTES / size_of::<Feature<'_, S>>() {
                    return Err("Intermediate sets exceed the query memory budget. Narrow the selectors or zoom in.".into());
                }
                stored.insert(&set.name, members);
            }
            if sink.limited() {
                break;
            }
        }
        sink.finish(counts, skipped.len(), skipped_refs.len());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regol::FeatureType;

    #[test]
    fn count_id_budget_rejects_growth_past_the_limit() {
        let mut ids = HashSet::new();
        let relation = FeatureId {
            kind: FeatureType::Relation,
            id: 1,
        };
        remember_counted_id(&mut ids, (false, relation), 2).unwrap();
        remember_counted_id(&mut ids, (true, relation), 2).unwrap();
        let way = FeatureId {
            kind: FeatureType::Way,
            id: 1,
        };
        let error = remember_counted_id(&mut ids, (false, way), 2).unwrap_err();
        assert!(error.contains("smaller bounding box"));
        assert_eq!(ids, HashSet::from([(false, relation), (true, relation)]));
    }
}
