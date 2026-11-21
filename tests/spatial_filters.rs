use regol::SpatialPredicate::{
    Contains, CoveredBy, Crosses, Disjoint, Intersects, Overlaps, Touches, Within, WithinDistance,
};
use regol::units::{
    Length,
    length::{centimeter, meter},
};
use regol::{
    Bounds, Error, FeatureId, FeatureType, Geometry, Library, Limits, Point, Polygon, Query,
    Result, SpatialFilter,
};

fn p(x: i32, y: i32) -> Point {
    Point { x, y }
}

fn point(x: i32, y: i32) -> Geometry {
    Geometry::Point(p(x, y))
}

fn line(coords: &[(i32, i32)]) -> Geometry {
    Geometry::LineString(coords.iter().map(|&(x, y)| p(x, y)).collect())
}

fn ring(x: i32, y: i32, size: i32) -> Vec<Point> {
    vec![
        p(x, y),
        p(x + size, y),
        p(x + size, y + size),
        p(x, y + size),
        p(x, y),
    ]
}

fn square(x: i32, y: i32, size: i32) -> Geometry {
    Geometry::Polygon(Polygon {
        exterior: ring(x, y, size),
        interiors: Vec::new(),
    })
}

#[test]
fn polygon_holes_and_shared_boundaries() -> Result<()> {
    let area = Geometry::Polygon(Polygon {
        exterior: ring(0, 0, 12),
        interiors: vec![ring(4, 4, 4)],
    });
    let within = SpatialFilter::new(Within, &area)?;
    let intersecting = SpatialFilter::new(Intersects, &area)?;
    for (candidate, is_within, intersects) in [
        (point(1, 1), true, true),
        (point(0, 1), false, true),
        (point(5, 4), false, true),
        (point(5, 5), false, false),
        (point(15, 5), false, false),
        (line(&[(0, 1), (11, 1)]), true, true),
        (line(&[(4, 6), (8, 6)]), false, true),
        (line(&[(4, 4), (8, 4)]), false, true),
        (line(&[(1, 6), (11, 6)]), false, true),
        (area.clone(), true, true),
    ] {
        assert_eq!(
            within.matches_geometry(&candidate)?,
            is_within,
            "{candidate:?}"
        );
        assert_eq!(
            intersecting.matches_geometry(&candidate)?,
            intersects,
            "{candidate:?}"
        );
        let contains = SpatialFilter::new(Contains, &candidate)?;
        assert_eq!(
            contains.matches_geometry(&area)?,
            is_within,
            "{candidate:?}"
        );
    }
    assert_eq!(within.candidate_bounds(), Some(Bounds::new(0, 0, 12, 12)?));

    let triangle = Geometry::Polygon(Polygon {
        exterior: vec![p(0, 0), p(8, 0), p(0, 8), p(0, 0)],
        interiors: Vec::new(),
    });
    let filter = SpatialFilter::new(Intersects, &triangle)?;
    assert!(filter.candidate_bounds().unwrap().contains(p(7, 7)));
    assert!(!filter.matches_geometry(&point(7, 7))?);
    Ok(())
}

#[test]
fn crossing_requires_an_interior_intersection() -> Result<()> {
    let horizontal = line(&[(0, 0), (12, 0)]);
    let crosses = SpatialFilter::new(Crosses, &horizontal)?;
    assert!(crosses.matches_geometry(&line(&[(2, -2), (2, 2)]))?);
    assert!(!crosses.matches_geometry(&line(&[(2, 0), (2, 2)]))?);
    assert!(!crosses.matches_geometry(&line(&[(2, 0), (6, 0)]))?);

    let area = Geometry::Polygon(Polygon {
        exterior: ring(0, 0, 12),
        interiors: vec![ring(4, 4, 4)],
    });
    let crosses = SpatialFilter::new(Crosses, &area)?;
    assert!(crosses.matches_geometry(&line(&[(3, 6), (9, 6)]))?);
    assert!(crosses.matches_geometry(&line(&[(-1, 6), (13, 6)]))?);
    assert!(!crosses.matches_geometry(&line(&[(-1, 6), (0, 6)]))?);
    assert!(!crosses.matches_geometry(&line(&[(0, 1), (11, 1)]))?);
    assert!(!crosses.matches_geometry(&area)?);
    Ok(())
}

#[test]
fn covered_by_touches_and_overlaps_distinguish_boundaries() -> Result<()> {
    let area = square(0, 0, 12);
    let edge = line(&[(2, 0), (9, 0)]);
    let touching = SpatialFilter::new(Touches, &area)?;
    let overlapping = SpatialFilter::new(Overlaps, &area)?;
    let covered = SpatialFilter::new(CoveredBy, &area)?;

    assert!(touching.matches_geometry(&point(0, 5))?);
    assert!(touching.matches_geometry(&square(12, 0, 3))?);
    assert!(touching.matches_geometry(&edge)?);
    assert!(!touching.matches_geometry(&line(&[(2, 0), (2, 9)]))?);
    assert!(!touching.matches_geometry(&square(14, 0, 3))?);
    assert!(overlapping.matches_geometry(&square(9, 9, 5))?);
    assert!(!overlapping.matches_geometry(&square(9, 9, 2))?);
    assert!(!overlapping.matches_geometry(&square(12, 0, 3))?);
    assert!(covered.matches_geometry(&point(0, 5))?);
    assert!(covered.matches_geometry(&edge)?);
    assert!(covered.matches_geometry(&area)?);
    assert!(!covered.matches_geometry(&line(&[(2, 0), (14, 0)]))?);
    assert!(!covered.matches_geometry(&Geometry::Collection(Vec::new()))?);
    assert!(!SpatialFilter::new(Within, &area)?.matches_geometry(&edge)?);
    Ok(())
}

#[test]
fn disjoint_keeps_candidates_outside_the_operand_bounds() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/basic.gol").as_slice())?;
    let building = library
        .select(&Query::parse("a[building]")?)
        .next()
        .transpose()?
        .unwrap();
    let disjoint = SpatialFilter::from_feature(Disjoint, building)?;
    let query = Query::parse("n")?;
    let prepared = query.prepare(&library)?;
    assert_eq!(disjoint.candidate_bounds(), None);
    let streamed = library
        .select_spatial(&query, &disjoint)
        .collect::<Result<Vec<_>>>()?;
    let compiled = prepared.spatial(&disjoint).collect::<Result<Vec<_>>>()?;
    assert_eq!(streamed.iter().map(|f| f.id().id).collect::<Vec<_>>(), [5]);
    assert_eq!(compiled.iter().map(|f| f.id().id).collect::<Vec<_>>(), [5]);

    let empty = Geometry::LineString(Vec::new());
    assert!(disjoint.matches_geometry(&empty)?);
    assert!(!disjoint.matches(building)?);
    let disjoint_from_empty = SpatialFilter::new(Disjoint, &empty)?;
    assert!(disjoint_from_empty.matches_geometry(&empty)?);
    assert!(disjoint_from_empty.matches(building)?);
    assert_eq!(
        prepared
            .spatial(&disjoint_from_empty)
            .collect::<Result<Vec<_>>>()?
            .len(),
        2
    );
    Ok(())
}

#[test]
fn multipolygon_components_and_nested_collections() -> Result<()> {
    let area = Geometry::MultiPolygon(vec![
        Polygon {
            exterior: ring(0, 0, 4),
            interiors: Vec::new(),
        },
        Polygon {
            exterior: ring(8, 0, 4),
            interiors: Vec::new(),
        },
    ]);
    let inside = Geometry::Collection(vec![
        point(1, 1),
        Geometry::Collection(vec![
            line(&[(9, 1), (10, 1)]),
            Geometry::LineString(Vec::new()),
        ]),
    ]);
    let mixed = Geometry::Collection(vec![inside.clone(), point(6, 1)]);
    let within = SpatialFilter::new(Within, &area)?;
    let intersects = SpatialFilter::new(Intersects, &area)?;
    assert!(within.matches_geometry(&inside)?);
    assert!(!within.matches_geometry(&mixed)?);
    assert!(intersects.matches_geometry(&mixed)?);
    assert!(!intersects.matches_geometry(&point(6, 1))?);
    assert!(!intersects.matches_geometry(&Geometry::Collection(Vec::new()))?);
    assert!(SpatialFilter::new(Contains, &inside)?.matches_geometry(&area)?);
    assert!(SpatialFilter::new(Crosses, &area)?.matches_geometry(&mixed)?);
    assert!(SpatialFilter::new(Within, &inside)?.matches_geometry(&point(1, 1))?);
    assert!(!SpatialFilter::new(Within, &inside)?.matches_geometry(&point(9, 1))?);

    let joined = Geometry::Collection(vec![line(&[(-10, 0), (0, 0)]), line(&[(0, 0), (10, 0)])]);
    let within_lines = SpatialFilter::new(Within, &joined)?;
    assert!(within_lines.matches_geometry(&point(0, 0))?);
    assert!(within_lines.matches_geometry(&line(&[(-5, 0), (5, 0)]))?);
    Ok(())
}

#[test]
fn complex_topology_rejects_overlapping_or_edge_adjacent_areas() -> Result<()> {
    let surrounding = square(0, 0, 30);
    let within = SpatialFilter::new(Within, &surrounding)?;
    for east_x in [10, 11] {
        let collection = Geometry::Collection(vec![square(1, 1, 10), square(east_x, 1, 10)]);
        assert!(matches!(
            SpatialFilter::new(Within, &collection),
            Err(Error::Geometry(_))
        ));
        assert!(matches!(
            within.matches_geometry(&collection),
            Err(Error::Geometry(_))
        ));
        assert!(SpatialFilter::new(Intersects, &collection)?.matches_geometry(&point(east_x, 5))?);
    }
    let crossing_ring = Geometry::Polygon(Polygon {
        exterior: [p(0, 0), p(10, 10), p(0, 8), p(12, 0), p(0, 0)].to_vec(),
        interiors: Vec::new(),
    });
    assert!(matches!(
        SpatialFilter::new(Touches, &crossing_ring),
        Err(Error::Geometry(_))
    ));
    for holes in [vec![ring(30, 30, 5)], vec![ring(3, 3, 5), ring(5, 5, 5)]] {
        let malformed = Geometry::Polygon(Polygon {
            exterior: ring(0, 0, 20),
            interiors: holes,
        });
        assert!(matches!(
            SpatialFilter::new(Within, &malformed),
            Err(Error::Geometry(_))
        ));
        assert!(matches!(
            within.matches_geometry(&malformed),
            Err(Error::Geometry(_))
        ));
    }
    Ok(())
}

#[test]
fn route_stops_on_line_members_keep_the_line_topology() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/ids.gol").as_slice())?;
    let node = library
        .feature_by_id(FeatureId {
            kind: FeatureType::Node,
            id: 5,
        })?
        .unwrap();
    for id in [21, 22] {
        let route = library
            .feature_by_id(FeatureId {
                kind: FeatureType::Relation,
                id,
            })?
            .unwrap();
        assert!(!SpatialFilter::from_feature(Within, route)?.matches(node)?);
        assert!(!SpatialFilter::from_feature(Contains, node)?.matches(route)?);
        assert!(SpatialFilter::from_feature(CoveredBy, route)?.matches(node)?);
        assert!(SpatialFilter::from_feature(Touches, route)?.matches(node)?);
        assert!(SpatialFilter::from_feature(Touches, node)?.matches(route)?);
        assert!(!SpatialFilter::from_feature(Crosses, node)?.matches(route)?);
    }
    Ok(())
}

#[test]
fn mixed_collections_relate_as_the_union_of_their_members() -> Result<()> {
    let area = square(0, 0, 10);
    let cases = [
        // The added point meets the second geometry on its boundary. The
        // detached line does not turn that contact into a line crossing.
        (
            Geometry::Collection(vec![line(&[(10, 0), (20, 0)]), point(5, 0)]),
            line(&[(5, 0), (5, 5)]),
            [false, false, true, false, false],
        ),
        // A line end covered by the polygon interior is interior to the
        // union. The exterior portion of the line remains in the union.
        (
            Geometry::Collection(vec![line(&[(5, -5), (5, 5)]), area.clone()]),
            point(5, 5),
            [true, false, false, false, false],
        ),
        (
            Geometry::Collection(vec![line(&[(5, -5), (5, 5)]), area.clone()]),
            area.clone(),
            [true, false, false, false, false],
        ),
        (
            Geometry::Collection(vec![line(&[(5, -5), (5, 5)]), area.clone()]),
            line(&[(3, -3), (7, -3)]),
            [false, false, false, false, true],
        ),
        (
            Geometry::Collection(vec![line(&[(0, 0), (11, 3)]), line(&[(3, 10), (7, -2)])]),
            line(&[(0, 0), (11, 3)]),
            [true, false, false, false, false],
        ),
        (
            Geometry::Collection(vec![point(15, 2), area.clone()]),
            area,
            [true, false, false, false, false],
        ),
    ];
    for (a, b, [contains, within, touches, overlaps, crosses]) in cases {
        for (predicate, inverse, expected) in [
            (Contains, Within, contains),
            (Within, Contains, within),
            (Touches, Touches, touches),
            (Overlaps, Overlaps, overlaps),
            (Crosses, Crosses, crosses),
        ] {
            assert_eq!(
                SpatialFilter::new(predicate, &b)?.matches_geometry(&a)?,
                expected,
                "{predicate:?}: {a:?}, {b:?}"
            );
            assert_eq!(
                SpatialFilter::new(inverse, &a)?.matches_geometry(&b)?,
                expected,
                "{inverse:?}: {b:?}, {a:?}"
            );
        }
    }
    Ok(())
}

#[test]
fn mixed_collection_crossing_preserves_fractional_intersections_at_world_edges() -> Result<()> {
    let west = i32::MAX - 2;
    let east = i32::MAX - 1;
    let south = i32::MIN + 1;
    let north = i32::MAX;
    let area = Geometry::Polygon(Polygon {
        exterior: vec![
            p(west, south),
            p(east, south),
            p(east, north),
            p(west, north),
            p(west, south),
        ],
        interiors: Vec::new(),
    });
    let collection = Geometry::Collection(vec![area, point(0, 0)]);
    let path = line(&[(i32::MIN, i32::MAX / 2), (i32::MAX, i32::MIN / 2)]);
    assert!(SpatialFilter::new(Crosses, &collection)?.matches_geometry(&path)?);
    assert!(SpatialFilter::new(Crosses, &path)?.matches_geometry(&collection)?);
    assert!(!SpatialFilter::new(Touches, &collection)?.matches_geometry(&path)?);
    Ok(())
}

#[test]
fn point_predicates_validate_polygon_candidates() -> Result<()> {
    let bad = Geometry::Polygon(Polygon {
        exterior: vec![p(0, 0), p(10, 10), p(0, 8), p(12, 0), p(0, 0)],
        interiors: Vec::new(),
    });
    let orphan_hole = Geometry::Polygon(Polygon {
        exterior: ring(0, 0, 20),
        interiors: vec![ring(30, 30, 5)],
    });
    let at = point(5, 5);
    for bad in [bad, orphan_hole] {
        for predicate in [Contains, Within, CoveredBy, Touches, Crosses, Overlaps] {
            assert!(matches!(
                SpatialFilter::new(predicate, &at)?.matches_geometry(&bad),
                Err(Error::Geometry(_))
            ));
        }
    }
    Ok(())
}

#[test]
fn distance_uses_meters_and_inclusive_threshold() -> Result<()> {
    let east = point(i32::MAX - 10, 0);
    let west = point(i32::MIN + 10, 0);
    for threshold in [f64::NAN, f64::INFINITY, -1.0] {
        assert!(
            SpatialFilter::new(WithinDistance(Length::new::<meter>(threshold)), &east).is_err()
        );
    }
    let separation = west.distance(&east).unwrap();
    assert!(
        separation > Length::new::<centimeter>(10.0)
            && separation < Length::new::<centimeter>(30.0)
    );
    let filter = SpatialFilter::new(WithinDistance(separation), &east)?;
    assert_eq!(filter.candidate_bounds(), None);
    assert!(filter.matches_geometry(&west)?);
    assert!(!SpatialFilter::new(WithinDistance(separation / 2.0), &east)?.matches_geometry(&west)?);
    assert!(!filter.matches_geometry(&Geometry::LineString(Vec::new()))?);
    assert!(!SpatialFilter::new(Intersects, &east)?.matches_geometry(&west)?);
    Ok(())
}

#[test]
fn feature_operand_and_spatial_query_entry_points() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/basic.gol").as_slice())?;
    let query = Query::parse("wa[name]")?;
    let prepared = query.prepare(&library)?;
    let building = library
        .select(&Query::parse("a[building]")?)
        .next()
        .transpose()?
        .unwrap();
    let within = SpatialFilter::from_feature(Within, building)?;
    let intersecting = SpatialFilter::from_feature(Intersects, building)?;

    let collect_ids = |results: Vec<regol::Feature<'_, &[u8]>>| {
        let mut ids: Vec<_> = results.iter().map(|f| f.id().id).collect();
        ids.sort_unstable();
        ids
    };
    for (filter, expected) in [(&within, vec![11, 20]), (&intersecting, vec![10, 11, 20])] {
        let streamed = library
            .select_spatial(&query, filter)
            .collect::<Result<Vec<_>>>()?;
        let compiled = prepared.spatial(filter).collect::<Result<Vec<_>>>()?;
        assert!(
            streamed
                .iter()
                .zip(&compiled)
                .all(|(a, b)| a.id() == b.id())
        );
        assert_eq!(collect_ids(streamed), expected);
        assert_eq!(collect_ids(compiled), expected);
    }

    let first = library.features().next().transpose()?.unwrap();
    let from_feature =
        SpatialFilter::from_feature(WithinDistance(Length::new::<meter>(0.0)), first)?;
    assert!(from_feature.matches(first)?);
    assert!(
        library
            .select_spatial(&Query::all(), &from_feature)
            .any(|candidate| candidate.is_ok_and(|f| f.id() == first.id()))
    );
    Ok(())
}

#[test]
fn stored_bounds_reject_before_assembly_and_stream_errors_are_terminal() -> Result<()> {
    let library = Library::with_limits(
        include_bytes!("fixtures/basic.gol").as_slice(),
        Limits {
            max_coordinates: 0,
            ..Limits::default()
        },
    )?;
    let query = Query::parse("a[building]")?;
    let building = library.select(&query).next().transpose()?.unwrap();
    let prepared = query.prepare(&library)?;

    let disjoint = SpatialFilter::new(Intersects, &point(i32::MIN, i32::MIN))?;
    assert!(!disjoint.matches(building)?);
    assert!(library.select_spatial(&query, &disjoint).next().is_none());
    assert!(prepared.spatial(&disjoint).next().is_none());

    let overlapping = SpatialFilter::new(Intersects, &Geometry::Point(building.bounds()?.min))?;
    assert!(matches!(
        overlapping.matches(building),
        Err(Error::Limit(_))
    ));
    let mut streamed = library.select_spatial(&query, &overlapping);
    assert!(matches!(streamed.next(), Some(Err(Error::Limit(_)))));
    assert!(streamed.next().is_none());
    let mut compiled = prepared.spatial(&overlapping);
    assert!(matches!(compiled.next(), Some(Err(Error::Limit(_)))));
    assert!(compiled.next().is_none());

    let empty = SpatialFilter::new(Intersects, &Geometry::MultiPolygon(Vec::new()))?;
    assert!(!empty.matches(building)?);
    assert!(prepared.spatial(&empty).next().is_none());
    Ok(())
}
