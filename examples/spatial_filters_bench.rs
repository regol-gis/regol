//! Fixture-scale measurements for spatial selection. Run in release mode.
use regol::units::{Length, length::meter};
use regol::{
    Error, FeatureId, FeatureType, Geometry, Library, Point, Polygon, PreparedQuery, Query, Result,
    SpatialFilter, SpatialPredicate, source::Source,
};
use std::{hint::black_box, time::Instant};

fn measure(
    name: &str,
    iterations: u32,
    expected: u64,
    mut run: impl FnMut() -> Result<u64>,
) -> Result<()> {
    assert_eq!(run()?, expected, "unexpected match count for {name}");
    let start = Instant::now();
    for _ in 0..iterations {
        assert_eq!(
            black_box(run()?),
            expected,
            "unexpected match count for {name}"
        );
    }
    println!(
        "{name},{iterations},{},{expected}",
        start.elapsed().as_nanos() / u128::from(iterations),
    );
    Ok(())
}

fn rectangle(min: Point, max: Point) -> Geometry {
    Geometry::Polygon(Polygon {
        exterior: vec![
            min,
            Point { x: max.x, y: min.y },
            max,
            Point { x: min.x, y: max.y },
            min,
        ],
        interiors: Vec::new(),
    })
}

fn count_matches(
    query: &PreparedQuery<'_, '_, impl Source>,
    filter: &SpatialFilter,
) -> Result<u64> {
    query
        .spatial(black_box(filter))
        .try_fold(0, |count, feature| {
            black_box(feature?);
            Ok(count + 1)
        })
}

fn main() -> Result<()> {
    let iterations = std::env::args()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(100);
    if iterations == 0 {
        return Err(Error::Geometry("benchmark iterations must be positive"));
    }
    let bytes = include_bytes!("../tests/fixtures/stress.gol");
    let library = Library::new(bytes.as_slice())?;
    let operand = library
        .select(&Query::parse(r#"a[name="Area 81"]"#)?)
        .next()
        .transpose()?
        .ok_or(Error::Geometry("benchmark fixture has no building"))?;
    let geometry = operand.geometry()?;
    let bounds = operand.bounds()?;
    // Expand the building partway into neighboring paths to get positive
    // crossings from the same fixture.
    let dx = (bounds.max.x - bounds.min.x) / 2;
    let dy = (bounds.max.y - bounds.min.y) / 2;
    let min = Point {
        x: bounds.min.x - dx,
        y: bounds.min.y - dy,
    };
    let max = Point {
        x: bounds.max.x + dx,
        y: bounds.max.y + dy,
    };
    let expanded = rectangle(min, max);
    let shifted = rectangle(
        Point {
            x: bounds.min.x + dx,
            y: bounds.min.y,
        },
        Point {
            x: bounds.max.x + dx,
            y: bounds.max.y,
        },
    );
    let query = Query::parse("nwa")?;
    let prepared = query.prepare(&library)?;

    println!("kernel,iterations,ns_per_iteration,features_per_iteration");
    measure("bbox_candidates", iterations, 10, || {
        prepared.count_in_bounds(black_box(bounds))
    })?;
    // The projected predicate counts were checked with GEOS on the fixture.
    for (kernel, predicate, operand, expected) in [
        ("intersects", SpatialPredicate::Intersects, &geometry, 10),
        ("within", SpatialPredicate::Within, &geometry, 1),
        ("contains", SpatialPredicate::Contains, &geometry, 1),
        ("crosses", SpatialPredicate::Crosses, &expanded, 4),
        ("covered_by", SpatialPredicate::CoveredBy, &geometry, 5),
        ("touches", SpatialPredicate::Touches, &geometry, 9),
        ("overlaps", SpatialPredicate::Overlaps, &shifted, 1),
        ("disjoint", SpatialPredicate::Disjoint, &geometry, 1_116),
        (
            "within_30_meters",
            SpatialPredicate::WithinDistance(Length::new::<meter>(30.0)),
            &geometry,
            61,
        ),
    ] {
        let filter = SpatialFilter::new(predicate, operand)?;
        measure(kernel, iterations, expected, || {
            count_matches(&prepared, &filter)
        })?;
    }

    let route = library
        .feature_by_id(FeatureId {
            kind: FeatureType::Relation,
            id: 3000,
        })?
        .ok_or(Error::Geometry("benchmark fixture has no route 3000"))?;
    let touches = SpatialFilter::from_feature(SpatialPredicate::Touches, route)?;
    let routes = Query::parse("r[type=route]")?;
    let routes = routes.prepare(&library)?;
    // GEOS finds boundary-only contact with routes 3001, 3002 and 3006.
    measure("touches_route_collection", iterations, 3, || {
        count_matches(&routes, &touches)
    })?;

    // A long route and one detached stop keep both line and point components.
    // A short crossing way exercises segment lookup in the route interior.
    let route = Geometry::Collection(vec![
        Geometry::LineString((0..=2_048).map(|x| Point { x, y: x % 2 }).collect()),
        Geometry::Point(Point { x: 0, y: 4 }),
    ]);
    let crosses_route = SpatialFilter::new(SpatialPredicate::Crosses, &route)?;
    let crossing = Geometry::LineString(vec![Point { x: 1_024, y: -2 }, Point { x: 1_024, y: 3 }]);
    measure("crosses_long_route", iterations, 1, || {
        Ok(u64::from(
            crosses_route.matches_geometry(black_box(&crossing))?,
        ))
    })?;

    // A densely sampled diamond intersects the 32-by-32 fixture grid. The
    // 1,024 segments exercise the reused operand across hundreds of nodes.
    let vertices = [
        Point {
            x: -16_984,
            y: 709_655_000,
        },
        Point {
            x: -600,
            y: 709_626_328,
        },
        Point {
            x: 15_784,
            y: 709_655_000,
        },
        Point {
            x: -600,
            y: 709_683_672,
        },
        Point {
            x: -16_984,
            y: 709_655_000,
        },
    ];
    let mut exterior = Vec::with_capacity(1_025);
    for edge in vertices.windows(2) {
        let dx = (edge[1].x - edge[0].x) / 256;
        let dy = (edge[1].y - edge[0].y) / 256;
        exterior.extend((0..256).map(|step| Point {
            x: edge[0].x + step * dx,
            y: edge[0].y + step * dy,
        }));
    }
    exterior.push(vertices[0]);
    let diamond = Geometry::Polygon(Polygon {
        exterior,
        interiors: Vec::new(),
    });
    let within = SpatialFilter::new(SpatialPredicate::Within, &diamond)?;
    let grid = Query::parse("n[test:group]")?;
    let prepared = grid.prepare(&library)?;
    // Counted directly from the fixture node positions and the diamond's
    // linear inequalities in projected coordinates.
    measure("within_large_polygon", iterations, 417, || {
        count_matches(&prepared, &within)
    })?;
    Ok(())
}
