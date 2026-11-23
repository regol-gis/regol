use super::*;
use crate::geometry::geographic_point;

fn point(lon: f64, lat: f64) -> Point {
    geographic_point([lon, lat]).unwrap().project()
}

fn check(candidate: &Geometry, reference: &Geometry, meters: f64, expected: bool) {
    let threshold = Length::new::<meter>(meters);
    let refs = vec![Geometry::Collection(vec![]), reference.clone()];
    let oracle = SpatialFilter::new(
        SpatialPredicate::WithinDistance(threshold),
        &Geometry::Collection(refs.clone()),
    )
    .unwrap();
    assert_eq!(oracle.matches_geometry(candidate).unwrap(), expected);
    let index = DistanceIndex::new(refs, threshold, &|| Ok(())).unwrap();
    assert_eq!(index.matches(candidate).unwrap(), expected);
}

#[test]
fn exact_threshold_and_zero_distance_across_the_dateline() {
    let east = Geometry::Point(Point {
        x: i32::MAX - 10,
        y: 0,
    });
    let west = Geometry::Point(Point {
        x: i32::MIN + 10,
        y: 0,
    });
    let meters = west.distance(&east).unwrap().get::<meter>();
    check(&west, &east, meters, true);
    check(&west, &east, meters.next_down(), false);
    check(
        &Geometry::Point(Point { x: i32::MIN, y: 0 }),
        &Geometry::Point(Point { x: i32::MAX, y: 0 }),
        0.0,
        true,
    );
}

#[test]
fn spherical_arcs_and_polygon_interiors_survive_pruning() {
    let arc = Geometry::LineString(vec![point(-45.0, 80.0), point(45.0, 80.0)]);
    // The midpoint of the shorter arc is nearly three degrees north of both endpoints.
    let latitude = (80_f64.to_radians().tan() / 45_f64.to_radians().cos())
        .atan()
        .to_degrees();
    let middle = Geometry::Point(point(0.0, latitude));
    check(&middle, &arc, 1.0, true);
    check(&arc, &middle, 1.0, true);

    let ring = |d: f64| {
        vec![
            point(-d, -d),
            point(d, -d),
            point(d, d),
            point(-d, d),
            point(-d, -d),
        ]
    };
    let area = Geometry::MultiPolygon(vec![Polygon {
        exterior: ring(2.0),
        interiors: vec![ring(1.0)],
    }]);
    let inside = Geometry::Point(point(1.5, 0.0));
    let hole = Geometry::Point(point(0.0, 0.0));
    check(&inside, &area, 0.0, true);
    check(&area, &inside, 0.0, true);
    check(&hole, &area, 100.0, false);
    check(&Geometry::Collection(vec![]), &area, f64::MAX, false);

    let long_arc = Geometry::LineString(vec![point(-80.0, 0.0), point(80.0, 0.0)]);
    check(&hole, &long_arc, 0.0, true);
}

#[test]
fn sparse_points_only_visit_local_references() {
    let stops = (-1000..=1000)
        .map(|i| Geometry::Point(point(f64::from(i) * 0.01, 51.5)))
        .collect();
    let index = DistanceIndex::new(stops, Length::new::<meter>(100.0), &|| Ok(())).unwrap();
    let cafe = Geometry::Point(point(0.0, 51.5004));
    assert_eq!(index.nearby(bounds(&cafe).unwrap()).count(), 1);
    assert!(index.matches(&cafe).unwrap());
}
