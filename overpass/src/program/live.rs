//! Prepare Live's bounded map JSON in traversal order.
use super::{
    candidates::Candidates,
    outputs::{Counts, OutputDetail, OutputMode, OutputOrder, OutputStatement},
};
use crate::{
    geometry::{self, GeoGeometry},
    relationship::assembled,
    usable_geometry, with_geometry_reads,
};
use regol::{Feature, FeatureId, Geometry, source::Source};
use serde::Serialize;
use serde_json::value::{RawValue, to_raw_value};
use std::{
    collections::{BTreeMap, HashSet},
    ops::ControlFlow,
};

const MAX_OUTPUT_FEATURES: usize = 100_000;
pub(super) const MAX_DISPLAY_VERTICES: usize = 1_500_000;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Output {
    pub(crate) features: Vec<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) counts: Option<Counts>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) outputs: Vec<OutputGroup>,
    pub(crate) limited: Option<&'static str>,
    pub(crate) skipped_geometries: usize,
    pub(crate) skipped_references: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) references: Option<Vec<GeoGeometry>>,
}

#[derive(Serialize)]
pub(crate) struct OutputGroup {
    name: String,
    mode: OutputMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<OutputDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    order: Option<OutputOrder>,
    #[serde(skip_serializing_if = "Option::is_none")]
    counts: Option<Counts>,
}

#[derive(Serialize)]
struct MapFeature<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    properties: MapProperties<'a>,
    geometry: GeoGeometry,
    tags: &'a BTreeMap<String, String>,
}

#[derive(Serialize)]
struct MapProperties<'a> {
    fid: &'a str,
    group: usize,
    name: Option<&'a str>,
}

fn serialize_feature<S: Source>(
    f: Feature<'_, S>,
    geometry: GeoGeometry,
    group: usize,
    include_tags: bool,
    scoped_id: bool,
) -> Result<Box<RawValue>, String> {
    let mut tags = BTreeMap::new();
    if include_tags {
        for tag in f.tags().map_err(|e| e.to_string())? {
            let tag = tag.map_err(|e| e.to_string())?;
            tags.insert(tag.key.into_owned(), tag.value.to_string());
        }
    }
    let id = f.id().to_string();
    let distinct_id = scoped_id.then(|| format!("{group}:{id}"));
    to_raw_value(&MapFeature {
        kind: "Feature",
        id: &id,
        properties: MapProperties {
            fid: distinct_id.as_deref().unwrap_or(&id),
            group,
            name: tags.get("name").map(String::as_str),
        },
        geometry,
        tags: &tags,
    })
    .map_err(|e| e.to_string())
}

pub(super) enum Rendered {
    VertexLimit,
    Feature {
        vertices: usize,
        json: Result<Box<RawValue>, String>,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct Display {
    pub(super) group: usize,
    pub(super) center: bool,
    pub(super) tags: bool,
    pub(super) scoped_id: bool,
}

impl Display {
    pub(super) fn new(mode: OutputMode, group: usize, tags: bool, scoped_id: bool) -> Option<Self> {
        (mode != OutputMode::Count).then_some(Self {
            group,
            center: mode == OutputMode::Center,
            tags,
            scoped_id,
        })
    }

    pub(super) fn render<S: Source>(
        self,
        f: Feature<'_, S>,
        mut geometry: Option<Geometry>,
        remaining_vertices: usize,
    ) -> regol::Result<Rendered> {
        let (vertices, geometry) = if self.center {
            if remaining_vertices == 0 {
                return Ok(Rendered::VertexLimit);
            }
            (1, with_geometry_reads(f, crate::overpass::center)?)
        } else {
            with_geometry_reads(f, |f| assembled(f, &mut geometry).map(|_| ()))?;
            let geometry = geometry.as_ref().unwrap();
            let vertices = geometry::vertices(geometry);
            if vertices > remaining_vertices {
                return Ok(Rendered::VertexLimit);
            }
            (vertices, GeoGeometry::from_regol(geometry))
        };
        Ok(Rendered::Feature {
            vertices,
            json: serialize_feature(f, geometry, self.group, self.tags, self.scoped_id),
        })
    }
}

pub(crate) struct LiveOutput {
    pub(super) output: Output,
    vertices: usize,
    bytes: usize,
}

impl LiveOutput {
    pub(super) fn append<S: Source>(
        &mut self,
        f: Feature<'_, S>,
        g: Option<Geometry>,
        prepared: Option<regol::Result<Rendered>>,
        display: Display,
        bad: &mut HashSet<FeatureId>,
    ) -> Result<bool, String> {
        let out = &mut self.output;
        if out.features.len() == MAX_OUTPUT_FEATURES {
            out.limited = Some("feature");
            return Ok(false);
        }
        let rendered =
            prepared.unwrap_or_else(|| display.render(f, g, MAX_DISPLAY_VERTICES - self.vertices));
        let Some(rendered) = usable_geometry(f.id(), rendered, bad)? else {
            return Ok(false);
        };
        let Rendered::Feature { vertices, json } = rendered else {
            out.limited = Some("vertex");
            return Ok(false);
        };
        self.vertices += vertices;
        if self.vertices > MAX_DISPLAY_VERTICES {
            out.limited = Some("vertex");
            return Ok(false);
        }
        let feature = json?;
        self.bytes += feature.get().len();
        if self.bytes > MAX_OUTPUT_BYTES {
            out.limited = Some("output size");
            return Ok(false);
        }
        out.features.push(feature);
        Ok(true)
    }
}

impl LiveOutput {
    pub(crate) fn new(legacy_overlay: bool) -> Self {
        Self {
            output: Output {
                features: Vec::new(),
                counts: None,
                outputs: Vec::new(),
                limited: None,
                skipped_geometries: 0,
                skipped_references: 0,
                references: legacy_overlay.then(Vec::new),
            },
            vertices: 0,
            bytes: 0,
        }
    }

    pub(crate) fn into_output(self) -> Output {
        self.output
    }

    pub(super) fn start_group(
        &mut self,
        statement: &OutputStatement,
        counts: Option<Counts>,
    ) -> Result<Option<Display>, String> {
        let group = OutputGroup {
            name: statement.name.clone(),
            mode: statement.mode,
            detail: statement.detail,
            order: statement.order,
            counts,
        };
        self.bytes += serde_json::to_vec(&group).map_err(|e| e.to_string())?.len() + 1;
        if self.bytes > MAX_OUTPUT_BYTES {
            self.output.limited = Some("output size");
            return Ok(None);
        }
        let display = Display::new(
            statement.mode,
            self.output.outputs.len(),
            statement.detail != Some(OutputDetail::Skel),
            true,
        );
        self.output.outputs.push(group);
        Ok(display)
    }

    pub(super) fn append_group<S: Source, C: Fn() -> Result<(), String>>(
        &mut self,
        features: &[Feature<'_, S>],
        display: Display,
        skipped: &mut HashSet<FeatureId>,
        check: &C,
        executor: &impl Candidates<S, C>,
    ) -> Result<(), String> {
        executor.visit(
            features.iter().copied().map(Ok),
            &[],
            Some(display),
            check,
            |candidate, prepared| {
                check()?;
                let feature = candidate.map_err(|e| e.to_string())?;
                self.append(
                    feature,
                    None,
                    prepared.and_then(|work| work.display),
                    display,
                    skipped,
                )?;
                Ok(if self.output.limited.is_some() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            },
        )
    }
}
