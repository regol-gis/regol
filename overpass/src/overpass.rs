//! OSM element and tag semantics for the experimental Overpass QL editor.
mod center;
mod conditions;
pub(crate) mod scan;
mod spatial;
pub(crate) use center::center;
pub(crate) use spatial::Filter as SpatialMatcher;
pub(crate) use spatial::{NodePosition, NodeRadius};

use crate::geometry::{coordinates, geographic_point, project_box};
use regex::{Regex, RegexBuilder};
use regol::{
    Bounds, Feature, FeatureId, FeatureType, Predicate, Query, Selector, Types, source::Source,
};
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    sync::OnceLock,
};

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Node,
    Way,
    Relation,
    Nw,
    Nr,
    Wr,
    Nwr,
}

impl Kind {
    fn path_types(&self) -> Option<Types> {
        // GOL puts both area ways and area relations in the AREA index.
        Some(match self {
            Self::Node => return None,
            Self::Way | Self::Nw => Types::WAY.union(Types::AREA),
            Self::Relation | Self::Nr => Types::RELATION.union(Types::AREA),
            Self::Wr | Self::Nwr => Types::WAY.union(Types::RELATION).union(Types::AREA),
        })
    }

    fn includes(&self, feature: FeatureType) -> bool {
        match feature {
            FeatureType::Node => matches!(self, Self::Node | Self::Nw | Self::Nr | Self::Nwr),
            FeatureType::Way => matches!(self, Self::Way | Self::Nw | Self::Wr | Self::Nwr),
            FeatureType::Relation => {
                matches!(self, Self::Relation | Self::Nr | Self::Wr | Self::Nwr)
            }
        }
    }
}

#[derive(Deserialize)]
struct Tag {
    key: String,
    #[serde(flatten)]
    test: TagTest,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum TagTest {
    Exists,
    Missing,
    Eq { value: String },
    Ne { value: String },
    Re(TagRegex),
    Nre(TagRegex),
}

#[derive(Deserialize)]
struct TagRegex {
    value: String,
    #[serde(default)]
    insensitive: bool,
    line: Option<usize>,
    #[serde(skip)]
    compiled: OnceLock<Result<Regex, String>>,
}

impl TagRegex {
    fn pattern(&self) -> Result<&Regex, &str> {
        self.compiled
            .get_or_init(|| {
                RegexBuilder::new(&self.value)
                    .case_insensitive(self.insensitive)
                    .dot_matches_new_line(true)
                    .size_limit(262144)
                    .dfa_size_limit(262144)
                    .nest_limit(64)
                    .build()
                    .map_err(|error| format!("Unsupported or invalid regular expression: {error}"))
            })
            .as_ref()
            .map_err(String::as_str)
    }
}

#[derive(Deserialize)]
struct Proximity {
    point: Option<[f64; 2]>,
    polyline: Option<Vec<[f64; 2]>>,
    reference: Option<String>,
    meters: f64,
}

impl Proximity {
    fn validate(&self) -> Result<(), String> {
        if !self.meters.is_finite() || self.meters < 0.0 {
            return Err("Radius must be finite and nonnegative.".into());
        }
        let supplied = [
            self.point.is_some(),
            self.polyline.is_some(),
            self.reference.is_some(),
        ];
        if supplied.into_iter().filter(|&present| present).count() != 1 {
            return Err("Supply one reference set, point or polyline for around.".into());
        }
        if let Some(point) = self.point {
            geographic_point(point)?;
        }
        if let Some(points) = &self.polyline {
            if !(2..=10_000).contains(&points.len()) {
                return Err(
                    "An around polyline needs 2 to 10,000 latitude and longitude pairs.".into(),
                );
            }
            for &point in points {
                geographic_point(point)?;
            }
        }
        Ok(())
    }
}

#[derive(Deserialize)]
pub(crate) struct Selection {
    kind: Kind,
    tags: Vec<Tag>,
    #[serde(default)]
    conditions: Vec<conditions::Condition>,
    #[serde(default)]
    windows: Vec<[f64; 4]>,
    bbox: Option<bool>,
    around: Option<Proximity>,
    polygon: Option<Vec<[f64; 2]>>,
    pub(crate) area: Option<String>,
    #[serde(rename = "namedArea")]
    pub(crate) named_area: Option<crate::named_area::Lookup>,
}

impl Selection {
    pub fn is_node_only(&self) -> bool {
        self.kind.path_types().is_none()
    }

    pub(crate) fn includes(&self, feature: FeatureType) -> bool {
        self.kind.includes(feature)
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(lookup) = &self.named_area
            && (!matches!(self.kind, Kind::Wr)
                || !self.tags.iter().any(|tag| {
                    tag.key == "name"
                        && matches!(&tag.test, TagTest::Eq { value } if value == &lookup.name)
                })
                || !self.windows.is_empty()
                || self.polygon.is_some()
                || self.area.is_some()
                || self.around.is_some())
        {
            return Err(
                "Named-area lookup requires an exact name filter and a way/relation source.".into(),
            );
        }
        if let Some(around) = &self.around {
            around.validate()?;
        }
        for tag in &self.tags {
            if let TagTest::Re(regex) | TagTest::Nre(regex) = &tag.test {
                regex.pattern().map_err(|message| {
                    regex.line.map_or_else(
                        || message.to_owned(),
                        |line| format!("Line {line}: {message}"),
                    )
                })?;
            }
        }
        conditions::validate(&self.conditions)
    }

    pub fn query(&self) -> Query {
        if self.named_area.is_some() {
            return self.query_for_types(Types::AREA.union(Types::WAY));
        }
        let Some(paths) = self.kind.path_types() else {
            return self.query_for_types(Types::NODE);
        };
        self.query_for_types(if self.kind.includes(FeatureType::Node) {
            paths.union(Types::NODE)
        } else {
            paths
        })
    }

    pub fn path_query(&self) -> Option<Query> {
        self.kind
            .path_types()
            .map(|types| self.query_for_types(types))
    }

    fn query_for_types(&self, types: Types) -> Query {
        let mut selector = Selector::new(types);
        for tag in &self.tags {
            // Overpass key presence accepts every stored value, including "no".
            if matches!(tag.test, TagTest::Exists) {
                selector = selector.and(Predicate::present(&tag.key));
            } else if let TagTest::Eq { value } = &tag.test
                && !value.is_empty()
                && !value.starts_with('*')
                && !value.ends_with('*')
                && value.parse::<f64>().is_err()
            {
                // Push literal equality only where GOQL gives it the same meaning.
                selector = selector.and(Predicate::equal(&tag.key, value));
            } else if let TagTest::Re(regex) = &tag.test
                && regex.pattern().is_ok_and(|pattern| !pattern.is_match("no"))
            {
                // GOQL truthiness excludes "no" as well as absent keys.
                selector = selector.and(Predicate::truthy(&tag.key));
            }
        }
        Query::new(selector)
    }

    pub fn bounds(&self) -> Result<Vec<Bounds>, String> {
        if self.windows.len() > 32 {
            return Err("An Overpass selection exceeds the search window limit.".into());
        }
        let mut projected = self
            .windows
            .iter()
            .copied()
            .map(project_box)
            .collect::<Result<Vec<_>, _>>()?;
        if self.bbox == Some(false) {
            return Ok(Vec::new());
        }
        // A GOL can store a point on either edge of the projected world.
        // Include the other encoding for an inclusive box ending at the seam.
        for &[west, south, east, north] in &self.windows {
            let opposite = if west == -180.0 && east < 180.0 {
                180.0
            } else if east == 180.0 && west > -180.0 {
                -180.0
            } else {
                continue;
            };
            let edge = project_box([opposite, south, opposite, north])?;
            if !projected
                .iter()
                .any(|b| b.contains(edge.min) && b.contains(edge.max))
            {
                projected.push(edge);
            }
        }
        Ok(projected)
    }

    pub fn is_bounded(&self) -> bool {
        (self.bbox != Some(false) && !self.windows.is_empty())
            || self.around.is_some()
            || self.area.is_some()
            || self.polygon.is_some()
            || self.named_area.is_some()
    }

    pub fn around_reference(&self) -> Option<&str> {
        self.around.as_ref().and_then(|a| a.reference.as_deref())
    }

    pub fn spatial_filters<S: Source>(
        &self,
        stored: &HashMap<&str, Vec<Feature<'_, S>>>,
        mapped: &HashSet<&str>,
        skipped: &mut HashSet<FeatureId>,
        vertices: &mut usize,
        check: &impl Fn() -> Result<(), String>,
    ) -> Result<Vec<SpatialMatcher>, String> {
        let mut filters = Vec::new();
        if let Some(points) = &self.polygon {
            filters.push(SpatialMatcher::Area(spatial::Areas::polygon(points)?));
        }
        if let Some(name) = self.area.as_deref() {
            let areas = spatial::Areas::from_set(
                &stored[name],
                mapped.contains(name),
                skipped,
                vertices,
                check,
            )
            .map_err(|e| format!("${name}: {e}"))?;
            filters.push(SpatialMatcher::Area(areas));
        }
        if let Some(around) = &self.around {
            let proximity = if let Some(point) = around.point {
                spatial::Around::point(around.meters, geographic_point(point)?)
            } else if let Some(points) = &around.polyline {
                spatial::Around::polyline(around.meters, points, vertices, check)?
            } else {
                let name = around
                    .reference
                    .as_deref()
                    .expect("validated around reference");
                spatial::Around::from_set(around.meters, &stored[name], skipped, vertices, check)
                    .map_err(|e| format!("${name}: {e}"))?
            };
            filters.push(SpatialMatcher::Around(proximity));
        }
        Ok(filters)
    }

    pub fn matches<S: Source>(&self, f: Feature<'_, S>, bounds: &[Bounds]) -> regol::Result<bool> {
        if !self.kind.includes(f.kind()) {
            return Ok(false);
        }
        for tag in &self.tags {
            let found = f.tag(&tag.key)?;
            let ok = match &tag.test {
                TagTest::Exists => found.is_some(),
                TagTest::Missing => found.is_none(),
                TagTest::Eq { value } => found.is_some_and(|t| t.to_string() == *value),
                TagTest::Ne { value } => found.is_none_or(|t| t.to_string() != *value),
                TagTest::Re(regex) | TagTest::Nre(regex) => {
                    let pattern = regex.pattern().map_err(std::io::Error::other)?;
                    let matched = found.is_some_and(|value| match value {
                        regol::TagValue::Text(text) => pattern.is_match(&text),
                        number => pattern.is_match(&number.to_string()),
                    });
                    matched != matches!(tag.test, TagTest::Nre(_))
                }
            };
            if !ok {
                return Ok(false);
            }
        }
        for condition in &self.conditions {
            if !condition.matches(f)? {
                return Ok(false);
            }
        }
        if bounds.is_empty() {
            return Ok(true);
        }
        if f.kind() != FeatureType::Relation {
            return self.in_box(f, bounds);
        }
        // Overpass bbox tests direct node and way members of a relation.
        for member in f.members()? {
            let member = member?.feature;
            if member.kind() != FeatureType::Relation && self.in_box(member, bounds)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn in_box<S: Source>(&self, f: Feature<'_, S>, bounds: &[Bounds]) -> regol::Result<bool> {
        let b = f.bounds()?;
        if bounds
            .iter()
            .any(|w| w.contains(b.min) && w.contains(b.max))
        {
            return Ok(true);
        }
        let wrapped = b.min.x.abs_diff(b.max.x) > scan::HALF_WORLD;
        if f.kind() == FeatureType::Node
            || !bounds
                .iter()
                .any(|w| w.intersects(b) || (wrapped && w.max.y >= b.min.y && w.min.y <= b.max.y))
        {
            return Ok(false);
        }
        // A closed way around the viewport qualifies only if its outline touches
        // the box. Use the stored path, including its implicit closing vertex.
        let mut prev = None;
        for p in f.coordinates()? {
            let xy = coordinates(&p?);
            if let Some(start) = prev
                && self
                    .windows
                    .iter()
                    .any(|&window| crosses_window(start, xy, window))
            {
                return Ok(true);
            }
            prev = Some(xy);
        }
        Ok(false)
    }
}

// Clip the segment's parameter interval along each geographic axis.
fn crosses_window(a: [f64; 2], mut b: [f64; 2], [w, s, e, n]: [f64; 4]) -> bool {
    let delta = b[0] - a[0];
    if delta.abs() > 180.0 {
        b[0] -= delta.signum() * 360.0;
    }
    let shift = (((a[0] + b[0]) * 0.5 - (w + e) * 0.5) / 360.0).round() * 360.0;
    let mut lo = 0.0_f64;
    let mut hi = 1.0_f64;
    for (p, q, min, max) in [(a[0], b[0], w + shift, e + shift), (a[1], b[1], s, n)] {
        if p == q {
            if p < min || p > max {
                return false;
            }
        } else {
            let t = (min - p) / (q - p);
            let u = (max - p) / (q - p);
            lo = lo.max(t.min(u));
            hi = hi.min(t.max(u));
        }
    }
    lo <= hi
}
