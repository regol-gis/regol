use regol::units::angle::degree;
use regol::{FeatureType, Library, Result};
use std::collections::BTreeSet;

#[test]
fn large_foreign_export_deltas_and_backward_node_tables() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/wide_exports.gol").as_slice())?;
    let order: Vec<u64> = (1..=5000)
        .zip((5001..=10000).rev())
        .flat_map(|(a, b)| [a, b])
        .collect();
    let mut seen = BTreeSet::new();
    for feature in library.features() {
        let feature = feature?;
        assert!(seen.insert(feature.id()));
        match feature.kind() {
            FeatureType::Node => {
                let id = feature.id().id;
                let position = feature.point()?.unwrap().lon_lat();
                let lon = position.longitude.get::<degree>();
                let lat = position.latitude.get::<degree>();
                let delta = ((id - 1) % 5000) as f64 * 0.000001;
                let expected_lon = if id <= 5000 { 10.0 } else { 20.0 } + delta;
                assert!((lon - expected_lon).abs() < 5e-8);
                assert!((lat - 50.0 - delta).abs() < 5e-8);
                let parents: Vec<_> = feature
                    .parent_relations()?
                    .map(|f| f.map(|f| f.id().id))
                    .collect::<Result<_>>()?;
                assert_eq!(parents, [20001]);
            }
            FeatureType::Way => {
                let ids: Vec<_> = feature
                    .way_nodes()?
                    .map(|n| n.map(|n| n.id.unwrap()))
                    .collect::<Result<_>>()?;
                assert_eq!(ids, order);
                // Stored feature-node tables run backwards and have separate
                // wide-word flags and tile-delta layout from relation members.
                let ids: Vec<_> = feature
                    .feature_nodes()?
                    .map(|n| n.map(|n| n.id().id))
                    .collect::<Result<_>>()?;
                assert_eq!(ids, order);
            }
            FeatureType::Relation => {
                let ids: Vec<_> = feature
                    .members()?
                    .map(|m| {
                        m.map(|m| {
                            assert!(m.role.is_empty());
                            m.feature.id().id
                        })
                    })
                    .collect::<Result<_>>()?;
                let mut expected = order.clone();
                expected.push(10001);
                assert_eq!(ids, expected);
            }
        }
    }
    assert_eq!(seen.len(), 10002);
    Ok(())
}

#[test]
fn coincident_vertices_keep_distinct_identities() -> Result<()> {
    for bytes in [
        include_bytes!("fixtures/coincident.gol").as_slice(),
        include_bytes!("fixtures/coincident_ids.gol").as_slice(),
    ] {
        let library = Library::new(bytes)?;
        for feature in library.features() {
            let feature = feature?;
            if feature.kind() == FeatureType::Node {
                let expected = match feature.id().id {
                    100 => vec![110, 112],
                    101 => vec![110, 111, 112],
                    102 => vec![111, 112],
                    _ => continue,
                };
                let mut parents: Vec<_> = feature
                    .parent_ways()?
                    .map(|f| f.map(|f| f.id().id))
                    .collect::<Result<_>>()?;
                parents.sort_unstable();
                assert_eq!(parents, expected, "node {}", feature.id());
            } else if feature.kind() == FeatureType::Way && feature.id().id == 112 {
                let vertices: Vec<_> = feature.way_nodes()?.collect::<Result<_>>()?;
                assert_eq!(
                    vertices.iter().map(|v| v.id).collect::<Vec<_>>(),
                    [Some(100), Some(102), Some(101)]
                );
                assert_eq!(vertices[0].point, vertices[1].point);
            }
        }
    }
    Ok(())
}
