use regol::units::angle::degree;
use regol::{Bounds, FeatureType, Library, Point, Query, Result};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn dense_indexes_encodings_and_boundaries() -> Result<()> {
    let source: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/stress.osm.json")).unwrap();
    let lib = Library::new(include_bytes!("fixtures/stress.gol").as_slice())?;
    let mut expected = BTreeMap::new();
    let mut nodes = BTreeMap::new();
    for (plural, prefix) in [("nodes", "N"), ("ways", "W"), ("relations", "R")] {
        for item in source[plural].as_array().unwrap() {
            expected.insert(format!("{prefix}{}", item["id"].as_u64().unwrap()), item);
            if prefix == "N" {
                nodes.insert(item["id"].as_u64().unwrap(), item);
            }
        }
    }
    let mut seen = BTreeSet::new();
    for feature in lib.features() {
        let feature = feature?;
        let id = feature.id().to_string();
        assert!(seen.insert(id.clone()), "duplicate {id}");
        let original = expected[&id];
        let tags: BTreeMap<_, _> = feature
            .tags()?
            .map(|t| t.map(|t| (t.key.into_owned(), t.value.to_string())))
            .collect::<Result<_>>()?;
        let want: BTreeMap<_, _> = original["tags"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
            .collect();
        assert_eq!(tags, want, "{id} tags");
        let check_point = |point: Point, node: &serde_json::Value| {
            let position = point.lon_lat();
            let lon = position.longitude.get::<degree>();
            let lat = position.latitude.get::<degree>();
            assert!(
                (lon - node["lon"].as_f64().unwrap()).abs() < 5e-8,
                "longitude {id}"
            );
            assert!(
                (lat - node["lat"].as_f64().unwrap()).abs() < 5e-8,
                "latitude {id}"
            );
        };
        if let Some(p) = feature.point()? {
            check_point(p, original);
        }
        if feature.kind() == FeatureType::Way {
            let vertices: Vec<_> = feature.way_nodes()?.collect::<Result<_>>()?;
            let want = original["nodes"].as_array().unwrap();
            assert_eq!(vertices.len(), want.len());
            for (vertex, id) in vertices.iter().zip(want) {
                let id = id.as_u64().unwrap();
                assert_eq!(vertex.id, Some(id));
                check_point(vertex.point, nodes[&id]);
            }
        }
        if feature.kind() == FeatureType::Relation {
            let members: Vec<_> = feature.members()?.collect::<Result<_>>()?;
            let want = original["members"].as_array().unwrap();
            assert_eq!(members.len(), want.len());
            for (member, want) in members.iter().zip(want) {
                assert_eq!(member.feature.id().id, want[1].as_u64().unwrap());
                assert_eq!(member.role, want[2].as_str().unwrap());
            }
        }
        let _ = feature.geometry()?;
    }
    assert_eq!(seen, expected.keys().cloned().collect());
    Ok(())
}

#[test]
fn larger_query_matrix() -> Result<()> {
    let lib = Library::new(include_bytes!("fixtures/stress.gol").as_slice())?;
    let expected: BTreeMap<String, Vec<String>> =
        serde_json::from_str(include_str!("fixtures/stress.queries.json")).unwrap();
    for (text, want) in expected {
        let query = Query::parse(&text)?;
        let mut actual: Vec<_> = lib
            .select(&query)
            .map(|f| f.map(|f| f.id().to_string()))
            .collect::<Result<_>>()?;
        actual.sort();
        assert_eq!(actual, want, "{text}");
        let prepared = query.prepare(&lib)?;
        let mut optimized: Vec<_> = prepared
            .iter()
            .map(|f| f.map(|f| f.id().to_string()))
            .collect::<Result<_>>()?;
        optimized.sort();
        assert_eq!(optimized, want, "prepared {text}");
    }
    Ok(())
}

#[test]
fn raw_tags_expose_local_keys_that_dictionary_selectors_skip() -> Result<()> {
    let lib = Library::new(include_bytes!("fixtures/stress.gol").as_slice())?;
    let query = Query::parse("*[bench]")?;
    let simple: Vec<_> = lib
        .select(&query)
        .map(|f| f.map(|f| f.id().to_string()))
        .collect::<Result<_>>()?;
    assert!(simple.is_empty());
    let prepared = query.prepare(&lib)?;
    let optimized: Vec<_> = prepared
        .iter()
        .map(|f| f.map(|f| f.id().to_string()))
        .collect::<Result<_>>()?;
    assert_eq!(optimized, simple);
    let mut raw_ids = Vec::new();
    for feature in lib.features() {
        let feature = feature?;
        if let Some(value) = feature.tag("bench")? {
            assert_eq!(value.to_string(), "rare-global-value-as-local-key");
            raw_ids.push(feature.id().to_string());
        }
    }
    assert_eq!(raw_ids, ["N1"]);
    Ok(())
}

#[test]
fn spatial_index_random_windows_and_point_boundaries() -> Result<()> {
    let lib = Library::new(include_bytes!("fixtures/stress.gol").as_slice())?;
    let features: Vec<_> = lib.features().collect::<Result<_>>()?;
    let mut state = 37u64;
    let mut windows = vec![Bounds::WORLD];
    for f in &features {
        let bounds = f.bounds()?;
        windows.push(Bounds::new(
            bounds.min.x,
            bounds.min.y,
            bounds.min.x,
            bounds.min.y,
        )?);
    }
    for _ in 0..500 {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        let x = (state as i32 % 30000) - 15000;
        let y = features[0]
            .bounds()?
            .min
            .y
            .saturating_add((state >> 32) as i32 % 60000);
        windows.push(Bounds::new(x, y, x + 10000, y.saturating_add(10000))?);
    }
    for bounds in windows {
        let want: BTreeSet<_> = features
            .iter()
            .filter_map(|f| {
                f.bounds()
                    .ok()
                    .filter(|b| b.intersects(bounds))
                    .map(|_| f.id())
            })
            .collect();
        let actual: Vec<_> = lib
            .in_bounds(bounds)
            .map(|f| f.map(|f| f.id()))
            .collect::<Result<_>>()?;
        let unique: BTreeSet<_> = actual.iter().copied().collect();
        assert_eq!(actual.len(), unique.len(), "duplicate at {bounds:?}");
        assert_eq!(unique, want, "{bounds:?}");
    }
    Ok(())
}
