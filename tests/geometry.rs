use regol::units::angle::degree;
use regol::{FeatureType, Geometry, Library, Point, Polygon, Result};
use std::collections::BTreeMap;

// Compare undirected edges to allow ring rotation/orientation, while preserving
// every vertex from the original OSM input. No upstream assembly is consulted.
fn edges(ring: &[Point]) -> Vec<((i32, i32), (i32, i32))> {
    let mut edges: Vec<_> = ring
        .windows(2)
        .map(|e| {
            let a = (e[0].x, e[0].y);
            let b = (e[1].x, e[1].y);
            if a < b { (a, b) } else { (b, a) }
        })
        .collect();
    edges.sort_unstable();
    edges
}

#[test]
fn original_rings_holes_and_nested_collections() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/geometry.gol").as_slice())?;
    let mut nodes = BTreeMap::new();
    let mut geometries = BTreeMap::new();
    for feature in library.features() {
        let feature = feature?;
        if feature.kind() == FeatureType::Node {
            nodes.insert(feature.id().id, feature.point()?.unwrap());
        } else if feature.kind() == FeatureType::Way && feature.id().id == 112 {
            let points: Vec<_> = feature.coordinates()?.collect::<Result<_>>()?;
            assert_eq!(points.len(), 2);
            for (p, (lon, lat)) in points.iter().zip([(-1.0, -80.0), (1.0, 80.0)]) {
                let position = p.lon_lat();
                let x = position.longitude.get::<degree>();
                let y = position.latitude.get::<degree>();
                assert!((x - lon).abs() < 5e-8 && (y - lat).abs() < 5e-8);
            }
            assert!(i64::from(points[1].y) - i64::from(points[0].y) > i64::from(i32::MAX));
        } else if feature.kind() == FeatureType::Relation {
            geometries.insert(feature.id().id, feature.geometry()?);
        }
    }
    let ring = |ids: &[u64]| ids.iter().map(|id| nodes[id]).collect::<Vec<_>>();
    let exterior = ring(&[1, 2, 3, 4, 1]);
    let second = ring(&[9, 10, 11, 12, 9]);
    let hole = ring(&[5, 6, 7, 8, 5]);
    let polygons = |g: &Geometry| -> Vec<Polygon> {
        match g {
            Geometry::Polygon(p) => vec![p.clone()],
            Geometry::MultiPolygon(p) => p.clone(),
            _ => panic!("expected area"),
        }
    };
    let mut actual = polygons(&geometries[&200]);
    assert_eq!(actual.len(), 2);
    actual.sort_by_key(|p| p.interiors.len());
    assert_eq!(edges(&actual[0].exterior), edges(&second));
    assert_eq!(edges(&actual[1].exterior), edges(&exterior));
    assert_eq!(actual[1].interiors.len(), 1);
    assert_eq!(edges(&actual[1].interiors[0]), edges(&hole));

    let touching = polygons(&geometries[&207]);
    assert_eq!(touching.len(), 2);
    let mut actual: Vec<_> = touching.iter().map(|p| edges(&p.exterior)).collect();
    let mut expected = vec![edges(&exterior), edges(&ring(&[3, 13, 14, 15, 3]))];
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    assert!(touching.iter().all(|p| p.interiors.is_empty()));

    // Nested relation members do not participate in an area relation boundary.
    let nested_area = polygons(&geometries[&209]);
    assert_eq!(nested_area.len(), 1);
    assert_eq!(edges(&nested_area[0].exterior), edges(&second));
    // Adjacent inner rings share an edge; that edge must be removed.
    let merged = polygons(&geometries[&213]);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].interiors.len(), 1);
    assert_eq!(
        edges(&merged[0].interiors[0]),
        edges(&ring(&[5, 6, 17, 18, 7, 8, 5]))
    );

    assert_eq!(geometries[&201], geometries[&204]);
    assert_eq!(
        geometries[&202],
        Geometry::Collection(vec![
            Geometry::Point(nodes[&1]),
            Geometry::Point(nodes[&2]),
            Geometry::Point(nodes[&1])
        ])
    );
    let Geometry::Collection(mut points) = geometries[&202].clone() else {
        panic!()
    };
    points.push(Geometry::Point(nodes[&3]));
    assert_eq!(geometries[&205], Geometry::Collection(points));
    let Geometry::Collection(mut lines) = geometries[&201].clone() else {
        panic!()
    };
    lines.push(Geometry::Point(nodes[&3]));
    assert_eq!(geometries[&206], Geometry::Collection(lines));
    // The pinned producer breaks the original 211 <-> 212 reference cycle and
    // records geodesk:removed_refcycles. Geometry reflects the stored members.
    assert_eq!(
        geometries[&212],
        Geometry::Collection(vec![Geometry::Point(nodes[&1])])
    );
    assert_eq!(
        geometries[&211],
        Geometry::Collection(vec![
            Geometry::Point(nodes[&16]),
            Geometry::Point(nodes[&1])
        ])
    );
    Ok(())
}
