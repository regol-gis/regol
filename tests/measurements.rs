use regol::units::{Length, angle::degree, area::square_meter, length::meter};
use regol::{Error, FeatureType, Geometry, Library, Limits, Point, Polygon, Result};
use std::f64::consts::{PI, TAU};

const EARTH_RADIUS: f64 = 6_371_008.8;
const GOL_WIDTH: f64 = 4_294_967_294.999_9;

fn point(lon: f64, lat: f64) -> Point {
    Point {
        x: (lon * GOL_WIDTH / 360.0).round() as i32,
        y: (lat.to_radians().tan().asinh() * GOL_WIDTH / TAU).round() as i32,
    }
}

fn coordinate(x: i32, y: i32) -> Point {
    Point { x, y }
}

fn ring(west: f64, south: f64, east: f64, north: f64) -> Vec<Point> {
    vec![
        point(west, south),
        point(east, south),
        point(east, north),
        point(west, north),
        point(west, south),
    ]
}

fn polygon(exterior: Vec<Point>, interiors: Vec<Vec<Point>>) -> Geometry {
    Geometry::Polygon(Polygon {
        exterior,
        interiors,
    })
}

fn near(actual: f64, expected: f64, tolerance: f64) {
    assert!(
        (actual - expected).abs() <= tolerance,
        "{actual} differs from {expected} by more than {tolerance}"
    );
}

#[test]
fn line_length_and_point_distance_use_great_circle_arcs() {
    let origin = Geometry::Point(point(0.0, 0.0));
    let destination = Geometry::Point(point(1.0, 0.0));
    let degree_length = EARTH_RADIUS * PI / 180.0;
    near(
        origin.distance(&destination).unwrap().get::<meter>(),
        degree_length,
        0.01,
    );

    let line = Geometry::LineString(vec![point(0.0, 0.0), point(1.0, 0.0), point(1.0, 1.0)]);
    near(line.length().get::<meter>(), 2.0 * degree_length, 0.02);
    assert_eq!(line.perimeter().get::<meter>(), 0.0);

    let high_latitude = Geometry::LineString(vec![point(0.0, 60.0), point(1.0, 60.0)]);
    let half_angle_sine = 0.5 * (PI / 360.0).sin();
    near(
        high_latitude.length().get::<meter>(),
        2.0 * EARTH_RADIUS * half_angle_sine.asin(),
        0.01,
    );

    let wrapped = Geometry::LineString(vec![point(179.5, 0.0), point(-179.5, 0.0)]);
    near(wrapped.length().get::<meter>(), degree_length, 0.01);
    assert!(
        wrapped
            .centroid()
            .unwrap()
            .lon_lat()
            .longitude
            .get::<degree>()
            .abs()
            > 179.9999
    );

    let nearly_antipodal = Geometry::Point(point(180.0, 0.0));
    near(
        origin.distance(&nearly_antipodal).unwrap().get::<meter>(),
        PI * EARTH_RADIUS,
        0.01,
    );

    let seam = Geometry::LineString(vec![coordinate(i32::MAX, 0), coordinate(-i32::MAX, 0)]);
    near(
        origin.distance(&seam).unwrap().get::<meter>(),
        PI * EARTH_RADIUS,
        0.01,
    );
}

#[test]
fn spherical_area_counts_holes_and_each_polygon_component() {
    // This spherical triangle has two right angles and a third of 60 degrees.
    let mut triangle = vec![
        point(0.0, 0.0),
        point(90.0, 0.0),
        point(0.0, 60.0),
        point(0.0, 0.0),
    ];
    let expected_area = (PI / 3.0) * EARTH_RADIUS.powi(2);
    near(
        polygon(triangle.clone(), vec![])
            .area()
            .get::<square_meter>(),
        expected_area,
        40_000.0,
    );
    triangle.reverse();
    near(
        polygon(triangle, vec![]).area().get::<square_meter>(),
        expected_area,
        40_000.0,
    );

    let outer = ring(179.0, -1.0, -179.0, 1.0);
    let mut hole = ring(179.5, -0.5, -179.5, 0.5);
    hole.reverse();
    let shell = polygon(outer.clone(), vec![]);
    let hole_only = polygon(hole.clone(), vec![]);
    let with_hole = Polygon {
        exterior: outer,
        interiors: vec![hole],
    };
    let area = shell.area().get::<square_meter>() - hole_only.area().get::<square_meter>();
    let perimeter = shell.perimeter().get::<meter>() + hole_only.perimeter().get::<meter>();
    assert!((3.6e10..3.8e10).contains(&area));
    assert!((1.3e6..1.4e6).contains(&perimeter));

    let point_member = Geometry::Point(point(0.0, 0.0));
    let line_member = Geometry::LineString(vec![point(0.0, 0.0), point(1.0, 0.0)]);
    let line_length = line_member.length().get::<meter>();
    let collection = Geometry::Collection(vec![
        point_member,
        Geometry::Collection(vec![line_member]),
        Geometry::MultiPolygon(vec![with_hole.clone(), with_hole]),
    ]);
    near(collection.area().get::<square_meter>(), 2.0 * area, 0.01);
    near(collection.perimeter().get::<meter>(), 2.0 * perimeter, 0.01);
    near(
        collection.length().get::<meter>(),
        2.0 * perimeter + line_length,
        0.01,
    );

    // At high latitudes, a one-unit Mercator cell still has positive area.
    let x = 1_200_000_000;
    let y = 2_100_000_000;
    let cell = polygon(
        vec![
            coordinate(x, y),
            coordinate(x + 1, y),
            coordinate(x + 1, y + 1),
            coordinate(x, y + 1),
        ],
        vec![],
    );
    assert!((6.0e-7..8.0e-7).contains(&cell.area().get::<square_meter>()));
}

#[test]
fn centroid_uses_dimension_and_projected_weights() {
    let square = |lo: i32, hi: i32| {
        vec![
            coordinate(lo, lo),
            coordinate(hi, lo),
            coordinate(hi, hi),
            coordinate(lo, hi),
            coordinate(lo, lo),
        ]
    };
    let area = polygon(square(0, 100), vec![square(60, 80)]);
    assert_eq!(area.centroid(), Some(coordinate(49, 49)));

    let line = Geometry::LineString(vec![
        coordinate(0, 0),
        coordinate(30, 0),
        coordinate(30, 10),
    ]);
    assert_eq!(line.centroid(), Some(coordinate(19, 1)));
    let point = Geometry::Point(coordinate(5_000, 1_000));
    assert_eq!(
        Geometry::Collection(vec![line, Geometry::Collection(vec![point, area])]).centroid(),
        Some(coordinate(49, 49))
    );

    let wrapped = polygon(ring(179.0, 0.0, -179.0, 1.0), vec![]);
    let position = wrapped.centroid().unwrap().lon_lat();
    let lon = position.longitude.get::<degree>();
    let lat = position.latitude.get::<degree>();
    assert!(lon.abs() > 179.9999 && (0.49..0.51).contains(&lat));

    let westmost = coordinate(i32::MIN, 12);
    assert_eq!(Geometry::Point(westmost).centroid(), Some(westmost));
    assert_eq!(
        Geometry::LineString(vec![westmost]).centroid(),
        Some(westmost)
    );
}

#[test]
fn lower_dimensional_members_do_not_move_a_dateline_centroid() {
    let east = Polygon {
        exterior: ring(178.0, 0.0, 179.0, 1.0),
        interiors: vec![],
    };
    let west = Polygon {
        exterior: ring(-179.0, 0.0, -178.0, 1.0),
        interiors: vec![],
    };
    let areas = Geometry::MultiPolygon(vec![east, west]);
    let lines = Geometry::Collection(vec![
        Geometry::LineString(vec![point(178.0, 0.0), point(179.0, 1.0)]),
        Geometry::LineString(vec![point(-178.0, 0.0), point(-179.0, 1.0)]),
    ]);
    let greenwich = Geometry::Point(point(0.0, 0.0));
    let collapsed_ring = polygon(vec![point(0.0, 0.0); 4], vec![]);
    let erased_shell = ring(0.0, 0.0, 1.0, 1.0);
    let zero_area = polygon(erased_shell.clone(), vec![erased_shell]);
    let with_line = Geometry::Collection(vec![zero_area, areas.clone()]);
    assert_eq!(with_line.centroid(), areas.centroid());
    for geometry in [areas, lines] {
        let expected = geometry.centroid().unwrap();
        assert!(expected.lon_lat().longitude.get::<degree>().abs() > 179.9999);
        let mixed = Geometry::Collection(vec![greenwich.clone(), collapsed_ring.clone(), geometry]);
        assert_eq!(mixed.centroid(), Some(expected));
    }
}

#[test]
fn distance_handles_crossings_containment_holes_and_collections() {
    let east_west = Geometry::LineString(vec![point(-2.0, 0.0), point(2.0, 0.0)]);
    let north_south = Geometry::LineString(vec![point(0.0, -2.0), point(0.0, 2.0)]);
    assert_eq!(
        east_west.distance(&north_south),
        Some(Length::new::<meter>(0.0))
    );
    assert_eq!(
        Geometry::Point(point(0.0, 0.0)).distance(&east_west),
        Some(Length::new::<meter>(0.0))
    );
    let north = Geometry::Point(point(0.0, 0.5));
    near(
        north.distance(&east_west).unwrap().get::<meter>(),
        EARTH_RADIUS * PI / 360.0,
        0.01,
    );

    let seam_area = polygon(
        ring(178.0, -2.0, -178.0, 2.0),
        vec![ring(179.0, -1.0, -179.0, 1.0)],
    );
    let inside = Geometry::Point(point(178.5, 0.0));
    let boundary = Geometry::Point(point(179.0, 0.0));
    let hole = Geometry::Point(point(180.0, 0.0));
    let exterior = Geometry::Point(point(0.0, 0.0));
    assert_eq!(seam_area.distance(&inside), Some(Length::new::<meter>(0.0)));
    assert_eq!(
        boundary.distance(&seam_area),
        Some(Length::new::<meter>(0.0))
    );
    near(
        hole.distance(&seam_area).unwrap().get::<meter>(),
        PI * EARTH_RADIUS / 180.0,
        0.01,
    );
    assert!(exterior.distance(&seam_area).unwrap().get::<meter>() > 19_000_000.0);

    let within_hole = polygon(ring(179.5, -0.5, -179.5, 0.5), vec![]);
    assert!(within_hole.distance(&seam_area).unwrap().get::<meter>() > 50_000.0);
    let overlapping = polygon(ring(177.5, -0.5, 178.5, 0.5), vec![]);
    assert_eq!(
        overlapping.distance(&seam_area),
        Some(Length::new::<meter>(0.0))
    );
    let collection = Geometry::Collection(vec![Geometry::LineString(vec![]), hole]);
    near(
        collection.distance(&seam_area).unwrap().get::<meter>(),
        PI * EARTH_RADIUS / 180.0,
        0.01,
    );
}

#[test]
fn spherical_polygon_containment_handles_the_poles() {
    let north = polygon(
        vec![
            point(-135.0, 70.0),
            point(-45.0, 70.0),
            point(45.0, 70.0),
            point(135.0, 70.0),
        ],
        vec![],
    );
    let inside = Geometry::Point(point(0.0, 80.0));
    let opposite = Geometry::Point(point(180.0, -80.0));
    assert_eq!(north.distance(&inside), Some(Length::new::<meter>(0.0)));
    assert!(north.distance(&opposite).unwrap().get::<meter>() > 16_000_000.0);
}

#[test]
fn empty_and_degenerate_geometries() {
    let empty = Geometry::Collection(vec![
        Geometry::LineString(vec![]),
        polygon(vec![], vec![]),
        Geometry::MultiPolygon(vec![]),
    ]);
    let point = Geometry::Point(coordinate(1, 2));
    assert_eq!(empty.length().get::<meter>(), 0.0);
    assert_eq!(empty.perimeter().get::<meter>(), 0.0);
    assert_eq!(empty.area().get::<square_meter>(), 0.0);
    assert_eq!(empty.centroid(), None);
    assert_eq!(empty.distance(&point), None);
    assert_eq!(point.distance(&empty), None);

    let degenerate = Geometry::LineString(vec![coordinate(1, 2), coordinate(1, 2)]);
    assert_eq!(degenerate.centroid(), Some(coordinate(1, 2)));
    assert_eq!(degenerate.distance(&point), Some(Length::new::<meter>(0.0)));
}

#[test]
fn feature_measurements_use_assembled_geometry() -> Result<()> {
    let fixture = include_bytes!("fixtures/geometry.gol").as_slice();
    let library = Library::new(fixture)?;
    let other = Geometry::Point(point(0.0, 0.0));
    for feature in library.features() {
        let feature = feature?;
        if feature.kind() != FeatureType::Node
            && (feature.kind(), feature.id().id) != (FeatureType::Way, 112)
            && (feature.kind(), feature.id().id) != (FeatureType::Relation, 200)
        {
            continue;
        }
        let geometry = feature.geometry()?;
        assert_eq!(feature.length()?, geometry.length());
        assert_eq!(feature.perimeter()?, geometry.perimeter());
        assert_eq!(feature.area()?, geometry.area());
        assert_eq!(feature.centroid()?, geometry.centroid());
        assert_eq!(feature.distance(&other)?, geometry.distance(&other));
    }

    let restricted = Library::with_limits(
        fixture,
        Limits {
            max_coordinates: 0,
            ..Limits::default()
        },
    )?;
    let feature = restricted.features().next().transpose()?.unwrap();
    assert!(matches!(feature.centroid(), Err(Error::Limit(_))));
    Ok(())
}

#[test]
fn geographic_axes_validate_angles_in_the_supplied_unit() -> Result<()> {
    use regol::{
        Latitude, Longitude,
        units::{
            Angle,
            angle::{radian, revolution},
        },
    };

    let west = Longitude::new::<revolution>(-0.5)?;
    assert_eq!(west.get::<degree>(), -180.0);
    let north = Latitude::try_from(Angle::new::<radian>(PI / 2.0))?;
    assert_eq!(Angle::from(north).get::<degree>(), 90.0);
    assert!(Longitude::new::<degree>(180.0).is_ok());
    assert!(Latitude::new::<degree>(-90.0).is_ok());
    for value in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -180.00001,
        180.00001,
    ] {
        assert!(Longitude::new::<degree>(value).is_err());
        assert!(Latitude::new::<degree>(value).is_err());
    }
    assert!(Latitude::new::<revolution>(-0.2501).is_err());
    assert!(Latitude::new::<revolution>(0.2501).is_err());
    Ok(())
}

#[test]
fn geographic_projection_handles_grid_edges_and_poles() -> Result<()> {
    use regol::{Latitude, LonLat, Longitude};
    for x in [i32::MIN, i32::MIN + 1, -1, 0, 1, i32::MAX] {
        for y in [i32::MIN, -917_450_588, 0, 718_874_329, i32::MAX] {
            let pos = Point { x, y }.lon_lat();
            assert!((-180.0..=180.0).contains(&pos.longitude.get::<degree>()));
            assert_eq!(
                pos.project(),
                Point {
                    x: x.max(i32::MIN + 1),
                    y
                }
            );
        }
    }
    for (degrees, y) in [(-90.0, i32::MIN), (90.0, i32::MAX)] {
        let pos = LonLat {
            longitude: Longitude::new::<degree>(45.0)?,
            latitude: Latitude::new::<degree>(degrees)?,
        };
        assert_eq!(pos.project(), Point { x: 536_870_912, y });
    }
    Ok(())
}

#[test]
fn spherical_measurements_interoperate_with_standard_uom_types() {
    use uom::si::{area::square_kilometer, f64::Area, length::kilometer};
    let side = Geometry::LineString(vec![point(0.0, 0.0), point(1.0, 0.0)]).length();
    let square: Area = side * side;
    near(
        square.get::<square_kilometer>(),
        side.get::<kilometer>().powi(2),
        1e-9,
    );
}

#[test]
fn geographic_bounds_check_corner_order_before_rounding() -> Result<()> {
    use regol::{Bounds, Latitude, LonLat, Longitude};
    let corner = |lon, lat| -> Result<LonLat> {
        Ok(LonLat {
            longitude: Longitude::new::<degree>(lon)?,
            latitude: Latitude::new::<degree>(lat)?,
        })
    };
    assert_eq!(
        Bounds::from_lon_lat(corner(-180., -90.)?, corner(180., 90.)?)?,
        Bounds::WORLD
    );
    let tiny = corner(0.000000001, 0.000000001)?;
    let zero = corner(0., 0.)?;
    assert_eq!(tiny.project(), zero.project());
    assert!(Bounds::from_lon_lat(tiny, zero).is_err());
    assert!(Bounds::from_lon_lat(corner(170., -10.)?, corner(-170., 10.)?).is_err());
    assert!(Bounds::from_lon_lat(corner(-1., 2.)?, corner(1., 1.)?).is_err());
    assert_eq!(Bounds::from_lon_lat(zero, tiny)?, Bounds::new(0, 0, 0, 0)?);
    Ok(())
}
