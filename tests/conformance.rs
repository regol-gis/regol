use regol::units::angle::degree;
use regol::{
    Bounds, FeatureType, Library, Point, Result,
    source::{FileSource, Source},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(format!("{name}.gol"))
}

fn check_basic<S: Source>(lib: &Library<S>) -> Result<()> {
    let input: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/basic.osm.json")).unwrap();
    let mut actual = BTreeMap::new();
    for f in lib.features() {
        let f = f?;
        let tags: BTreeMap<String, String> = f
            .tags()?
            .map(|t| t.map(|t| (t.key.into_owned(), t.value.to_string())))
            .collect::<Result<_>>()?;
        assert!(
            actual.insert(f.id().to_string(), tags).is_none(),
            "duplicate feature {}",
            f.id()
        );
        if f.kind() == FeatureType::Way {
            let expected = input["ways"]
                .as_array()
                .unwrap()
                .iter()
                .find(|w| w["id"].as_u64() == Some(f.id().id))
                .unwrap();
            let coords: Vec<Point> = f.coordinates()?.collect::<Result<_>>()?;
            assert_eq!(
                coords.len(),
                expected["nodes"].as_array().unwrap().len(),
                "{} coordinates",
                f.id()
            );
            for (coord, id) in coords.iter().zip(expected["nodes"].as_array().unwrap()) {
                let node = input["nodes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|n| n["id"] == *id)
                    .unwrap();
                let position = coord.lon_lat();
                let lon = position.longitude.get::<degree>();
                let lat = position.latitude.get::<degree>();
                assert!((lon - node["lon"].as_f64().unwrap()).abs() < 5e-8);
                assert!((lat - node["lat"].as_f64().unwrap()).abs() < 5e-8);
            }
            let vertices: Vec<_> = f.way_nodes()?.collect::<Result<_>>()?;
            assert_eq!(vertices.len(), coords.len());
            for (vertex, id) in vertices.iter().zip(expected["nodes"].as_array().unwrap()) {
                let id = id.as_u64().unwrap();
                let want = if lib.has_way_node_ids() || [1, 5].contains(&id) {
                    Some(id)
                } else {
                    None
                };
                assert_eq!(vertex.id, want, "{} vertex identity", f.id());
            }
        }
        if f.kind() == FeatureType::Relation {
            let expected = input["relations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"].as_u64() == Some(f.id().id))
                .unwrap();
            let members: Vec<_> = f
                .members()?
                .map(|m| m.map(|m| (m.feature.id().to_string(), m.role.into_owned())))
                .collect::<Result<_>>()?;
            let want: Vec<_> = expected["members"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| {
                    (
                        format!(
                            "{}{}",
                            m[0].as_str().unwrap()[..1].to_uppercase(),
                            m[1].as_u64().unwrap()
                        ),
                        m[2].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            assert_eq!(members, want, "{} member order and roles", f.id());
        }
        let parents: BTreeSet<_> = f
            .parent_relations()?
            .map(|p| p.map(|p| p.id().id))
            .collect::<Result<_>>()?;
        let kind = match f.kind() {
            FeatureType::Node => "node",
            FeatureType::Way => "way",
            FeatureType::Relation => "relation",
        };
        let want: BTreeSet<_> = input["relations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| {
                r["members"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m[0] == kind && m[1].as_u64() == Some(f.id().id))
            })
            .map(|r| r["id"].as_u64().unwrap())
            .collect();
        assert_eq!(parents, want, "{} parent relations", f.id());
        if f.kind() == FeatureType::Node {
            let parents: BTreeSet<_> = f
                .parent_ways()?
                .map(|p| p.map(|p| p.id().id))
                .collect::<Result<_>>()?;
            let want: BTreeSet<_> = input["ways"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|w| {
                    w["nodes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|id| id.as_u64() == Some(f.id().id))
                })
                .map(|w| w["id"].as_u64().unwrap())
                .collect();
            assert_eq!(parents, want, "{} parent ways", f.id());
        }
    }
    let expected: BTreeSet<_> = ["N1", "N5", "W10", "W11", "W12", "R20", "R21", "R22"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(actual.keys().cloned().collect::<BTreeSet<_>>(), expected);
    for (plural, prefix) in [("nodes", "N"), ("ways", "W"), ("relations", "R")] {
        for item in input[plural].as_array().unwrap() {
            let id = format!("{prefix}{}", item["id"].as_u64().unwrap());
            if let Some(tags) = actual.get(&id) {
                let expected: BTreeMap<_, _> = item["tags"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
                    .collect();
                assert_eq!(*tags, expected, "{id} tags");
            }
        }
    }
    Ok(())
}

#[test]
fn metadata_and_alternate_snapshot() -> Result<()> {
    let bytes = include_bytes!("fixtures/basic.gol");
    let lib = Library::new(bytes.as_slice())?;
    assert_eq!(lib.active_snapshot(), 0);
    assert_eq!(lib.tile_count(), 1);
    let properties: BTreeMap<_, _> = lib.properties()?.collect::<Result<_>>()?;
    assert_eq!(properties.get("source").map(|v| v.as_ref()), Some(""));
    assert_eq!(
        properties.get("license").map(|v| v.as_ref()),
        Some("Open Database License 1.0")
    );
    let mut alternate = bytes.to_vec();
    alternate.copy_within(128..192, 192);
    alternate[16] = 1;
    let alternate = Library::new(alternate)?;
    assert_eq!(alternate.active_snapshot(), 1);
    check_basic(&alternate)
}

#[cfg(feature = "mmap")]
#[test]
fn mapped_reader_conforms() -> Result<()> {
    // SAFETY: This test only reads the checked-in fixture; fixture regeneration
    // must not be run concurrently with tests, as documented in the test workflow.
    let mapping = unsafe { regol::source::MappedFile::open(fixture("basic"))? };
    check_basic(&Library::new(mapping)?)
}

#[test]
fn original_data_semantics() -> Result<()> {
    check_basic(&Library::new(
        include_bytes!("fixtures/basic.gol").as_slice(),
    )?)
}

#[test]
fn positioned_io_matches_memory() -> Result<()> {
    check_basic(&Library::new(FileSource::open(fixture("basic"))?)?)
}

#[test]
fn global_strings() -> Result<()> {
    check_basic(&Library::new(std::fs::read(fixture("global"))?)?)
}

#[test]
fn cross_tile_references() -> Result<()> {
    check_basic(&Library::new(std::fs::read(fixture("tiled"))?)?)?;
    check_basic(&Library::new(std::fs::read(fixture("tiled_global"))?)?)
}

#[test]
fn optional_ids_do_not_change_features() -> Result<()> {
    check_basic(&Library::new(std::fs::read(fixture("ids"))?)?)
}

#[test]
fn index_matches_scan() -> Result<()> {
    for name in ["basic", "global", "tiled", "ids", "tiled_global"] {
        let lib = Library::new(std::fs::read(fixture(name))?)?;
        for x in [-30000, -11930, -1, 0, 11930, 18000, 23861, 30000] {
            for y in [-30000, -11930, -1, 0, 11930, 18000, 23861, 30000] {
                let bounds = Bounds::new(x, y, x + 10000, y + 10000)?;
                let scan: BTreeSet<_> = lib
                    .features()
                    .filter_map(|f| match f {
                        Ok(f) => match f.bounds() {
                            Ok(b) if b.intersects(bounds) => Some(Ok(f.id())),
                            Ok(_) => None,
                            Err(e) => Some(Err(e)),
                        },
                        Err(e) => Some(Err(e)),
                    })
                    .collect::<Result<_>>()?;
                let indexed: Vec<_> = lib
                    .in_bounds(bounds)
                    .map(|f| f.map(|f| f.id()))
                    .collect::<Result<_>>()?;
                let unique: BTreeSet<_> = indexed.iter().copied().collect();
                assert_eq!(unique.len(), indexed.len(), "duplicates in {name}");
                assert_eq!(scan, unique, "{name} at {x},{y}");
            }
        }
    }
    Ok(())
}

#[test]
fn goql_differential_matrix() -> Result<()> {
    for name in [
        "basic",
        "global",
        "tiled",
        "ids",
        "tiled_global",
        "geometry",
        "coincident",
        "coincident_ids",
        "wide_exports",
    ] {
        let lib = Library::new(std::fs::read(fixture(name))?)?;
        let expected: BTreeMap<String, Vec<String>> = serde_json::from_slice(&std::fs::read(
            fixture(name).with_extension("queries.json"),
        )?)
        .unwrap();
        for (text, mut want) in expected {
            let query = regol::Query::parse(&text)?;
            let mut actual: Vec<_> = lib
                .select(&query)
                .map(|f| f.map(|f| f.id().to_string()))
                .collect::<Result<_>>()?;
            actual.sort();
            want.sort();
            assert_eq!(actual, want, "{name}: {text}");
            let prepared = query.prepare(&lib)?;
            let mut optimized: Vec<_> = prepared
                .iter()
                .map(|f| f.map(|f| f.id().to_string()))
                .collect::<Result<_>>()?;
            optimized.sort();
            assert_eq!(optimized, want, "prepared {name}: {text}");
            assert_eq!(prepared.count()?, want.len() as u64, "count {name}: {text}");
        }
    }
    Ok(())
}

#[test]
fn union_is_duplicate_free() -> Result<()> {
    let lib = Library::new(include_bytes!("fixtures/basic.gol").as_slice())?;
    let query = regol::Query::parse("n,n,n[amenity]")?;
    let ids: Vec<_> = lib
        .select(&query)
        .map(|f| f.map(|f| f.id()))
        .collect::<Result<_>>()?;
    assert_eq!(ids.len(), 2);
    assert_eq!(ids.iter().copied().collect::<BTreeSet<_>>().len(), 2);
    Ok(())
}

#[test]
fn builder_matches_parser() -> Result<()> {
    use regol::{Predicate, Query, Selector, Types};
    let lib = Library::new(include_bytes!("fixtures/basic.gol").as_slice())?;
    let built = Query::new(
        Selector::new(Types::NODE.union(Types::AREA))
            .and(Predicate::equal("amenity", "cafe"))
            .and(Predicate::at_least("height", 3.0)),
    );
    let parsed = Query::parse("na[amenity=cafe][height>=3]")?;
    let ids = |q: &Query| {
        lib.select(q)
            .map(|f| f.map(|f| f.id()))
            .collect::<Result<Vec<_>>>()
    };
    assert_eq!(ids(&built)?, ids(&parsed)?);
    Ok(())
}

#[test]
fn unsupported_queries_are_errors() {
    for text in [
        "",
        "node",
        "n[",
        "n[name=]",
        "n[name=x",
        "n[!name=x]",
        "n[name==x]",
        "n[name=a*b]",
        "n[name~\"(?=a)\"]",
        "n[name~\"(a)\\1\"]",
        "n[name>NaN]",
        "n,",
        "n junk",
    ] {
        assert!(regol::Query::parse(text).is_err(), "accepted {text:?}");
    }
}

#[test]
fn complete_geometry_matches_reference() -> Result<()> {
    use regol::{Geometry, Polygon};
    fn point(p: Point, expected: &serde_json::Value) {
        let position = p.lon_lat();
        let lon = position.longitude.get::<degree>();
        let lat = position.latitude.get::<degree>();
        assert!((lon - expected[0].as_f64().unwrap()).abs() < 1e-11);
        assert!((lat - expected[1].as_f64().unwrap()).abs() < 1e-11);
    }
    fn line(points: &[Point], expected: &serde_json::Value) {
        let expected = expected.as_array().unwrap();
        assert_eq!(points.len(), expected.len());
        for (p, e) in points.iter().zip(expected) {
            point(*p, e);
        }
    }
    fn polygon(p: &Polygon, expected: &serde_json::Value) {
        assert_eq!(p.interiors.len() + 1, expected.as_array().unwrap().len());
        line(&p.exterior, &expected[0]);
        for (i, ring) in p.interiors.iter().enumerate() {
            line(ring, &expected[i + 1]);
        }
    }
    fn compare(g: &Geometry, expected: &serde_json::Value) {
        match g {
            Geometry::Point(p) => {
                assert_eq!(expected["type"], "Point");
                point(*p, &expected["coordinates"]);
            }
            Geometry::LineString(p) => {
                assert_eq!(expected["type"], "LineString");
                line(p, &expected["coordinates"]);
            }
            Geometry::Polygon(p) => {
                assert_eq!(expected["type"], "Polygon");
                polygon(p, &expected["coordinates"]);
            }
            Geometry::MultiPolygon(p) => {
                assert_eq!(expected["type"], "MultiPolygon");
                assert_eq!(p.len(), expected["coordinates"].as_array().unwrap().len());
                for (p, e) in p.iter().zip(expected["coordinates"].as_array().unwrap()) {
                    polygon(p, e);
                }
            }
            Geometry::Collection(gs) => {
                assert_eq!(expected["type"], "GeometryCollection");
                // CLI export preserves nesting; the Python geometry API flattens
                // non-area relation members, as does this reader.
                fn flatten<'a>(g: &'a serde_json::Value, out: &mut Vec<&'a serde_json::Value>) {
                    if g["type"] == "GeometryCollection" {
                        for child in g["geometries"].as_array().unwrap() {
                            flatten(child, out);
                        }
                    } else {
                        out.push(g);
                    }
                }
                let mut leaves = Vec::new();
                flatten(expected, &mut leaves);
                assert_eq!(gs.len(), leaves.len());
                for (g, e) in gs.iter().zip(leaves) {
                    compare(g, e);
                }
            }
        }
    }
    for name in ["basic", "global", "tiled", "ids", "tiled_global"] {
        let lib = Library::new(std::fs::read(fixture(name))?)?;
        let oracle: serde_json::Value = serde_json::from_slice(&std::fs::read(
            fixture(name).with_extension("oracle.geojson"),
        )?)
        .unwrap();
        let expected: BTreeMap<_, _> = oracle["features"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| (f["id"].as_str().unwrap(), &f["geometry"]))
            .collect();
        for f in lib.features() {
            let f = f?;
            compare(&f.geometry()?, expected[f.id().to_string().as_str()]);
        }
    }
    Ok(())
}

#[test]
fn reference_ids_and_point_geometry() -> Result<()> {
    let lib = Library::new(include_bytes!("fixtures/basic.gol").as_slice())?;
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/basic.oracle.geojson")).unwrap();
    let oracle: BTreeMap<_, _> = oracle["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["id"].as_str().unwrap(), f))
        .collect();
    let features: Vec<_> = lib.features().collect::<Result<_>>()?;
    assert_eq!(features.len(), oracle.len());
    for f in features {
        let expected = oracle[f.id().to_string().as_str()];
        if let Some(point) = f.point()? {
            let position = point.lon_lat();
            let lon = position.longitude.get::<degree>();
            let lat = position.latitude.get::<degree>();
            assert!((lon - expected["geometry"]["coordinates"][0].as_f64().unwrap()).abs() < 1e-11);
            assert!((lat - expected["geometry"]["coordinates"][1].as_f64().unwrap()).abs() < 1e-11);
        }
    }
    Ok(())
}

#[test]
fn completed_snapshot_selects_distinct_tile_generation() -> Result<()> {
    // Model a completed append-only update: two immutable tile generations and
    // two distinct tile directories remain in the same file. Only the active
    // snapshot byte determines which generation is visible to a new reader.
    let mut data = include_bytes!("fixtures/basic.gol").to_vec();
    fn word(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }
    let old_index = word(&data, 152) as usize * 4096;
    let old_tile = (word(&data, old_index + 4) >> 2) as usize * 4096;
    let length = word(&data, old_tile) as usize + 4;
    let new_tile = data.len().next_multiple_of(4096);
    let mut tile = data[old_tile..old_tile + length].to_vec();
    let mut changed = 0;
    for anchor in (8..tile.len() - 12).step_by(4) {
        let header = u64::from_le_bytes(tile[anchor..anchor + 8].try_into().unwrap());
        if header >> 12 == 1 && (header >> 3) & 3 == 0 {
            tile[anchor..anchor + 8]
                .copy_from_slice(&((1001u64 << 12) | (header & 0xfff)).to_le_bytes());
            changed += 1;
        }
    }
    assert_eq!(changed, 1);
    data.resize(new_tile, 0);
    data.extend(tile);
    let new_index = data.len().next_multiple_of(4096);
    data.resize(new_index + 12, 0);
    data[new_index..new_index + 4].copy_from_slice(&8u32.to_le_bytes());
    data[new_index + 4..new_index + 8]
        .copy_from_slice(&(((new_tile / 4096) as u32 * 4) | 2).to_le_bytes());
    data.copy_within(128..192, 192);
    data[216..220].copy_from_slice(&((new_index / 4096) as u32).to_le_bytes());
    for active in [0, 1] {
        data[16] = active;
        let lib = Library::new(data.as_slice())?;
        let ids: BTreeSet<_> = lib
            .features()
            .map(|f| f.map(|f| f.id().to_string()))
            .collect::<Result<_>>()?;
        assert_eq!(ids.len(), 8);
        assert_eq!(ids.contains("N1"), active == 0);
        assert_eq!(ids.contains("N1001"), active == 1);
        // Local member pointers continue to resolve in the chosen generation.
        for f in lib.features() {
            let f = f?;
            if f.id().to_string() == "R22" {
                let ids: Vec<_> = f
                    .members()?
                    .map(|m| m.map(|m| m.feature.id().to_string()))
                    .collect::<Result<_>>()?;
                assert_eq!(ids[1], if active == 0 { "N1" } else { "N1001" });
            }
        }
    }
    Ok(())
}

#[cfg(feature = "build")]
#[test]
fn native_pbf_build_round_trip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("library.gol");
    regol::build::build_pbf(fixture("basic").with_extension("osm.pbf"), &path)?;
    let bytes = std::fs::read(&path)?;
    let word = |pos: usize| u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
    assert_eq!(word(504), crc32c::crc32c(&bytes[..504]));
    assert_eq!(
        word(112),
        crc32c::crc32c(&bytes[4096..4096 + word(20) as usize])
    );
    let directory = word(152) as usize * 4096;
    let tile = (word(directory + 4) >> 2) as usize * 4096;
    let end = tile + word(tile) as usize;
    assert_eq!(word(end), crc32c::crc32c(&bytes[tile..end]));
    let lib = Library::open(path)?;
    assert!(lib.has_way_node_ids());
    check_basic(&lib)?;
    for q in [
        "na[amenity=cafe]",
        "n[capacity=12][height>3][large>65535]",
        "a[building]",
        "a[type=multipolygon]",
    ] {
        assert_eq!(regol::Query::parse(q)?.prepare(&lib)?.count()?, 1, "{q}");
    }
    let areas = regol::Query::parse("a[type=multipolygon]")?;
    let prepared = areas.prepare(&lib)?;
    let f = prepared.iter().next().unwrap()?;
    let regol::Geometry::Polygon(p) = f.geometry()? else {
        panic!("expected polygon")
    };
    assert_eq!(
        (p.exterior.len(), p.interiors.len(), p.interiors[0].len()),
        (5, 1, 5)
    );
    Ok(())
}

#[cfg(feature = "build")]
#[test]
fn failed_build_leaves_destination_and_directory_intact() -> anyhow::Result<()> {
    use regol::build::build_pbf;
    use std::fs;
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("truncated.pbf");
    let bytes = include_bytes!("fixtures/basic.osm.pbf");
    fs::write(&input, &bytes[..bytes.len() - 1])?;
    let output = dir.path().join("out.gol");
    assert!(build_pbf(&input, &output).is_err());
    assert_eq!(dir.path().read_dir()?.count(), 1);
    fs::write(&output, b"existing output")?;
    assert!(build_pbf(fixture("basic").with_extension("osm.pbf"), &output).is_err());
    assert_eq!(fs::read(&output)?, b"existing output");
    Ok(())
}
