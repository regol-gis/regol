use regol::{Feature, FeatureType, Point, TagValue, source::Source};
use rustc_hash::FxHashSet;
use std::{io::Write, str::FromStr};

mod antimeridian;
mod pbf;
mod projection;
pub(super) mod text;
mod xml;

type ExportResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy)]
pub(super) enum Format {
    Xml,
    Pbf,
}

#[derive(Default)]
pub(super) enum Completion {
    #[default]
    Multipolygon,
    All,
    None,
    Types(Vec<String>),
}

impl FromStr for Completion {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "multipolygon" => Ok(Self::Multipolygon),
            "all" => Ok(Self::All),
            "none" => Ok(Self::None),
            value => {
                let types: Vec<_> = value.split(',').map(str::trim).map(str::to_owned).collect();
                if types.iter().any(String::is_empty) {
                    return Err("expected all, none, or a comma-separated list of relation types");
                }
                Ok(Self::Types(types))
            }
        }
    }
}

impl Completion {
    fn includes(&self, relation: Feature<'_, impl Source>) -> regol::Result<bool> {
        let types = match self {
            Self::All => return Ok(true),
            Self::None => return Ok(false),
            Self::Multipolygon => {
                return Ok(
                    matches!(relation.tag("type")?, Some(TagValue::Text(value)) if value == "multipolygon"),
                );
            }
            Self::Types(types) => types,
        };
        Ok(match relation.tag("type")? {
            Some(TagValue::Text(value)) => types.iter().any(|kind| kind == value.as_ref()),
            Some(number) => types.contains(&number.to_string()),
            None => false,
        })
    }
}

pub(super) type AreaFilter<'a, 'f, S> = dyn Fn(Feature<'a, S>) -> regol::Result<bool> + 'f;

pub(super) fn export<'a, S: Source + Sync + 'a>(
    features: impl IntoIterator<Item = regol::Result<Feature<'a, S>>>,
    mut writer: impl Write,
    format: Format,
    has_way_node_ids: bool,
    completion: &Completion,
    within_area: Option<&AreaFilter<'a, '_, S>>,
    threads: Option<usize>,
) -> ExportResult<()> {
    // The native XML writer always expands relation members. Its PBF
    // writer applies completion only when the query has a spatial filter.
    let within_area = match format {
        Format::Xml => None,
        Format::Pbf => within_area,
    };
    let export = Export::collect(features, has_way_node_ids, completion, within_area)?;
    match format {
        Format::Xml => xml::write(&export, &mut writer, threads)?,
        Format::Pbf => pbf::write(&export, &mut writer, threads)?,
    }
    writer.flush()?;
    Ok(())
}

struct Export<'a, S> {
    nodes: Vec<Feature<'a, S>>,
    vertices: Vec<(u64, Point)>,
    ways: Vec<Feature<'a, S>>,
    relations: Vec<Feature<'a, S>>,
    // Flat references for libraries that store every way node ID.
    // ID-less libraries stream their locations directly from the way.
    refs: Vec<u64>,
    ref_ends: Vec<usize>,
    has_way_node_ids: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Expansion {
    Selected,
    Complete,
    Filtered,
}

#[derive(Default)]
struct Seen {
    nodes: FxHashSet<u64>,
    ways: FxHashSet<u64>,
    relations: FxHashSet<u64>,
}

impl Seen {
    fn of_kind(&mut self, kind: FeatureType) -> &mut FxHashSet<u64> {
        match kind {
            FeatureType::Node => &mut self.nodes,
            FeatureType::Way => &mut self.ways,
            FeatureType::Relation => &mut self.relations,
        }
    }
}

impl<'a, S: Source> Export<'a, S> {
    fn collect(
        features: impl IntoIterator<Item = regol::Result<Feature<'a, S>>>,
        has_way_node_ids: bool,
        completion: &Completion,
        within_area: Option<&AreaFilter<'a, '_, S>>,
    ) -> regol::Result<Self> {
        let mut nodes = Vec::new();
        let mut ways = Vec::new();
        let mut relations = Vec::new();
        let mut seen = Seen::default();
        let mut complete = FxHashSet::default();
        let mut queue = Vec::new();

        let mut add =
            |feature: Feature<'a, S>, expansion, queue: &mut Vec<_>| -> regol::Result<()> {
                let id = feature.id();
                let known = seen.of_kind(id.kind);
                let existing = known.contains(&id.id);
                if !existing {
                    if let (Expansion::Filtered, Some(in_scope)) = (expansion, within_area)
                        && !in_scope(feature)?
                    {
                        return Ok(());
                    }
                    known.insert(id.id);
                    match id.kind {
                        FeatureType::Node => nodes.push(feature),
                        FeatureType::Way => ways.push(feature),
                        FeatureType::Relation => relations.push(feature),
                    }
                }

                if id.kind == FeatureType::Relation {
                    // A relation first visited with filtering can later appear
                    // under a completed parent. Expand it once more in that case.
                    let full = within_area.is_none()
                        || expansion == Expansion::Complete
                        || (!existing && completion.includes(feature)?);
                    if (full && complete.insert(id.id)) || (!full && !existing) {
                        let children = if full {
                            Expansion::Complete
                        } else {
                            Expansion::Filtered
                        };
                        queue.push((feature, children));
                    }
                }
                Ok(())
            };

        for feature in features {
            add(feature?, Expansion::Selected, &mut queue)?;
        }

        // Each expansion is queued instead of recursing, including cycles.
        let mut index = 0;
        while let Some(&(relation, expansion)) = queue.get(index) {
            for member in relation.members()? {
                add(member?.feature, expansion, &mut queue)?;
            }
            index += 1;
        }

        // Sort before gathering references so the ID list is in output order.
        ways.sort_unstable_by_key(|feature| feature.id().id);
        relations.sort_unstable_by_key(|feature| feature.id().id);
        let mut vertices = Vec::new();
        let mut refs = Vec::new();
        let mut ref_ends = Vec::new();
        for &way in &ways {
            for node in way.feature_nodes()? {
                let node = node?;
                if seen.nodes.insert(node.id().id) {
                    nodes.push(node);
                }
            }

            if has_way_node_ids {
                for vertex in way.way_nodes()? {
                    let vertex = vertex?;
                    // Every way vertex has an ID in this library.
                    let id = vertex.id.expect("way vertex with stored ID");
                    refs.push(id);
                    if !seen.nodes.contains(&id) {
                        vertices.push((id, vertex.point));
                    }
                }
                ref_ends.push(refs.len());
            }
        }
        nodes.sort_unstable_by_key(|feature| feature.id().id);
        vertices.sort_unstable_by_key(|vertex| vertex.0);
        vertices.dedup_by_key(|vertex| vertex.0);

        Ok(Self {
            nodes,
            vertices,
            ways,
            relations,
            refs,
            ref_ends,
            has_way_node_ids,
        })
    }

    fn visit_nodes(
        &self,
        mut visit: impl FnMut(u64, Point, Option<Feature<'a, S>>) -> ExportResult<()>,
    ) -> ExportResult<()> {
        let mut vertices = self.vertices.iter().copied().peekable();
        for &feature in &self.nodes {
            let id = feature.id().id;
            while let Some((id, point)) = vertices.next_if(|vertex| vertex.0 < id) {
                visit(id, point, None)?;
            }
            // A later way may have added a feature node already present as a
            // geometry vertex. Preserve the feature record and all its tags.
            vertices.next_if(|vertex| vertex.0 == id);
            let point = feature.point()?.expect("node point");
            visit(id, point, Some(feature))?;
        }
        for (id, point) in vertices {
            visit(id, point, None)?;
        }
        Ok(())
    }

    fn way_refs(&self, index: usize) -> Option<&[u64]> {
        let &end = self.ref_ends.get(index)?;
        let start = if index == 0 {
            0
        } else {
            self.ref_ends[index - 1]
        };
        Some(&self.refs[start..end])
    }
}

fn osm_coordinates(point: Point) -> (i64, i64) {
    projection::osm_coordinates_fast(point)
}

pub(crate) fn id_prefix(kind: FeatureType) -> u8 {
    match kind {
        FeatureType::Node => b'N',
        FeatureType::Way => b'W',
        FeatureType::Relation => b'R',
    }
}

fn kind_name(kind: FeatureType) -> &'static str {
    match kind {
        FeatureType::Node => "node",
        FeatureType::Way => "way",
        FeatureType::Relation => "relation",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regol::{Library, Query};

    #[test]
    fn xml_export_includes_recursive_dependencies_and_original_ids() {
        let library = Library::new(include_bytes!("../tests/fixtures/ids.gol").as_slice()).unwrap();
        let query = Query::parse("r[name=Nested]").unwrap();
        let prepared = query.prepare(&library).unwrap();
        let mut output = Vec::new();
        export(
            prepared.iter(),
            &mut output,
            Format::Xml,
            library.has_way_node_ids(),
            &Completion::default(),
            None,
            None,
        )
        .unwrap();
        let xml = std::str::from_utf8(&output).unwrap();

        assert!(xml.contains("<node id=\"2\" lat=\"0.001\" lon=\"0.002\" version=\"1\"/>"));
        assert!(xml.contains("<way id=\"10\" version=\"1\">\n    <nd ref=\"5\"/>\n    <nd ref=\"1\"/>\n    <nd ref=\"2\"/>\n    <nd ref=\"3\"/>"));
        assert!(xml.contains("<member type=\"node\" ref=\"1\" role=\"custom rôle\"/>"));
        assert!(xml.contains("<member type=\"relation\" ref=\"21\" role=\"part\"/>"));
        assert_eq!(xml.matches("<node id=\"5\"").count(), 1);
        assert_eq!(xml.matches("<relation id=").count(), 2);
        assert_eq!(xml.matches("<way id=").count(), 1);
    }

    #[test]
    fn pbf_spatial_completion_expands_selected_relation_types() {
        let library = Library::new(include_bytes!("../tests/fixtures/ids.gol").as_slice()).unwrap();
        let query = Query::parse("r[name=Nested]").unwrap();
        let prepared = query.prepare(&library).unwrap();
        let within_area = |feature: Feature<'_, _>| Ok(feature.id().id == 1);

        let spatial =
            Export::collect(prepared.iter(), true, &Completion::None, Some(&within_area)).unwrap();
        assert_eq!(spatial.relations.len(), 1);
        assert!(spatial.ways.is_empty());
        assert_eq!(spatial.nodes.len(), 1);

        let complete = Export::collect(
            prepared.iter(),
            true,
            &"collection".parse().unwrap(),
            Some(&within_area),
        )
        .unwrap();
        assert_eq!(complete.relations.len(), 2);
        assert_eq!(complete.ways.len(), 1);
        assert_eq!(complete.nodes.len(), 2);
        assert_eq!(complete.vertices.len(), 2);
    }

    #[test]
    fn xml_export_keeps_inline_way_locations_without_ids() {
        let library =
            Library::new(include_bytes!("../tests/fixtures/basic.gol").as_slice()).unwrap();
        let query = Query::parse("w[highway=residential]").unwrap();
        let prepared = query.prepare(&library).unwrap();
        let mut output = Vec::new();
        export(
            prepared.iter(),
            &mut output,
            Format::Xml,
            library.has_way_node_ids(),
            &Completion::default(),
            None,
            None,
        )
        .unwrap();
        let xml = std::str::from_utf8(&output).unwrap();

        assert!(xml.contains("<nd lat=\"0.001\" lon=\"0.002\"/>"));
        assert_eq!(xml.matches("<node id=").count(), 2);
        assert_eq!(xml.matches("<nd ref=").count(), 2);
        assert_eq!(xml.matches("<nd lat=").count(), 2);
    }
}
