use regol::units::{area::square_meter, length::meter};
use regol::{
    Bounds, Error, FeatureId, FeatureType, Geometry, Library, Limits, Query, SpatialFilter,
    SpatialPredicate,
};
use std::collections::BTreeSet;
const GOL: &[u8] = include_bytes!("fixtures/ids.gol");
fn id(kind: FeatureType, id: u64) -> FeatureId {
    FeatureId { kind, id }
}

#[test]
fn algebra_lookup_and_error_propagation() -> regol::Result<()> {
    let lib = Library::new(GOL)?;
    let all = lib.feature_set();
    let nodes = all.select(Query::parse("n")?);
    let cafes = all.select(Query::parse("na[amenity=cafe]")?);
    assert_eq!(nodes.intersection(&cafes)?.count()?, 1);
    assert_eq!(nodes.union(&cafes)?.count()?, 2);
    assert_eq!(nodes.difference(&cafes)?.count()?, 1);
    let node = id(FeatureType::Node, 1);
    let cafe = lib.feature_by_id(node)?.unwrap();
    assert!(cafes.contains(cafe)?);
    assert_eq!(cafes.get(node)?.unwrap().id(), node);
    assert!(nodes.get(id(FeatureType::Way, 1))?.is_none());
    let other = Library::new(GOL)?;
    assert!(matches!(
        nodes.intersection(&other.feature_set()),
        Err(Error::DifferentLibraries)
    ));
    assert!(matches!(
        other.feature_set().contains(cafe),
        Err(Error::DifferentLibraries)
    ));
    Ok(())
}

#[test]
fn a_way_can_match_two_disjoint_windows() -> regol::Result<()> {
    let lib = Library::new(GOL)?;
    let all = lib.feature_set();
    let id = id(FeatureType::Way, 10);
    let way = all.get(id)?.unwrap();
    let bounds = way.bounds()?;
    let left = all.in_bounds(Bounds {
        min: bounds.min,
        max: bounds.min,
    });
    let right = all.in_bounds(Bounds {
        min: bounds.max,
        max: bounds.max,
    });
    let both = left.intersection(&right)?;
    assert!(both.get(id)?.is_some());
    assert!(
        both.iter()
            .collect::<regol::Result<Vec<_>>>()?
            .iter()
            .any(|f| f.id() == id)
    );
    Ok(())
}

#[test]
fn roles_parents_and_shared_vertices_include_nested_relations() -> regol::Result<()> {
    use FeatureType::{Node, Relation, Way};
    let lib = Library::new(GOL)?;
    let all = lib.feature_set();
    let route = all.get(id(Relation, 21))?.unwrap();
    assert_eq!(route.members_with_role("stop")?.count()?, 1);
    assert_eq!(route.member_set()?.count()?, 3);
    let stops = route.members_with_role("stop")?;
    assert_eq!(stops.union(&route.member_set()?)?.count()?, 3);
    let other_role = route
        .members_with_role("custom rôle")?
        .select(Query::parse("n")?);
    let both_roles = other_role.union(&stops.select(Query::parse("n")?))?;
    assert_eq!(both_roles.count()?, 2);
    assert!(both_roles.get(id(Node, 1))?.is_some());
    assert!(both_roles.get(id(Node, 5))?.is_some());
    assert_eq!(
        route
            .members_with_role("custom rôle")?
            .get(id(Node, 1))?
            .unwrap()
            .id(),
        id(Node, 1)
    );
    let node = all.get(id(Node, 5))?.unwrap();
    assert_eq!(
        node.parent_set()?
            .iter()
            .map(|f| f.map(|f| f.id()))
            .collect::<regol::Result<BTreeSet<_>>>()?,
        BTreeSet::from([id(Way, 10), id(Relation, 21)])
    );
    let connected = all.connected_to(node)?;
    assert_eq!(
        connected
            .iter()
            .map(|f| f.map(|f| f.id()))
            .collect::<regol::Result<BTreeSet<_>>>()?,
        BTreeSet::from([id(Way, 10), id(Relation, 21), id(Relation, 22)])
    );
    assert_eq!(connected.select(Query::parse("w")?).count()?, 1);
    Ok(())
}

#[test]
fn spatial_refinements_and_measurements_on_sets() -> regol::Result<()> {
    let library = Library::new(GOL)?;
    let all = library.feature_set();
    let building = all.get(id(FeatureType::Way, 11))?.unwrap();
    let touching = SpatialFilter::from_feature(SpatialPredicate::Intersects, building)?;
    let roads = all.select(Query::parse("w[highway]")?).spatial(touching);
    assert_eq!(roads.count()?, 1);
    assert_eq!(roads.length()?, roads.first()?.unwrap().length()?);
    assert_eq!(roads.area()?.get::<square_meter>(), 0.0);
    assert!(
        all.select(Query::parse("a[building]")?)
            .area()?
            .get::<square_meter>()
            > 0.0
    );
    let empty = roads.difference(&roads)?;
    assert_eq!(empty.length()?.get::<meter>(), 0.0);
    assert_eq!(empty.area()?.get::<square_meter>(), 0.0);
    Ok(())
}

#[test]
fn union_of_filtered_members_uses_retained_handles() -> regol::Result<()> {
    let library = Library::with_limits(
        include_bytes!("fixtures/stress.gol").as_slice(),
        Limits {
            max_traversal_steps: 32,
            ..Limits::default()
        },
    )?;
    let query = Query::parse("r")?;
    let prepared = query.prepare(&library)?;
    let route = prepared.iter().next().transpose()?.unwrap();
    assert_eq!(route.id(), id(FeatureType::Relation, 3000));
    let members = route.member_set()?;
    let stops = members.select(Query::parse("n")?);
    let ways = members.select(Query::parse("w")?);
    let union = stops.union(&ways)?;
    let collect_ids = |set: &regol::FeatureSet<'_, &[u8]>| {
        set.iter()
            .map(|f| f.map(|f| f.id()))
            .collect::<regol::Result<BTreeSet<_>>>()
    };
    let expected = collect_ids(&stops)?
        .union(&collect_ids(&ways)?)
        .copied()
        .collect();
    assert_eq!(union.count()?, 3);
    assert_eq!(collect_ids(&union)?, expected);
    let repeated_stop = route.members_with_role("stop")?;
    let nested = union.union(&repeated_stop.select(Query::all()))?;
    assert_eq!(collect_ids(&nested)?, expected);
    for id in expected {
        let feature = nested.get(id)?.unwrap();
        assert!(nested.contains(feature)?);
    }
    // The same member map includes an area way that neither predicate accepts.
    let area = members.select(Query::parse("a")?).first()?.unwrap();
    assert!(nested.get(area.id())?.is_none());
    assert!(!nested.contains(area)?);
    assert!(matches!(
        library.feature_set().count(),
        Err(Error::Limit(_))
    ));
    Ok(())
}

#[test]
fn empty_spatial_selection_never_reads_missing_tiles() -> regol::Result<()> {
    let mut bytes = GOL.to_vec();
    let directory = u32::from_le_bytes(bytes[152..156].try_into().unwrap()) as usize * 4096;
    bytes[directory + 4..directory + 8].fill(0);
    let library = Library::new(bytes)?;
    let all = library.feature_set();
    let empty_geometry = Geometry::Collection(vec![]);
    let empty = all.spatial(SpatialFilter::new(
        SpatialPredicate::Intersects,
        &empty_geometry,
    )?);
    assert_eq!(empty.count()?, 0);
    assert!(empty.get(id(FeatureType::Way, 10))?.is_none());
    assert!(all.intersection(&empty)?.first()?.is_none());
    assert!(empty.difference(&all)?.first()?.is_none());
    assert!(empty.union(&empty)?.first()?.is_none());
    assert!(matches!(
        empty.union(&all)?.first(),
        Err(Error::MissingTile(_))
    ));
    assert!(matches!(
        all.spatial(SpatialFilter::new(
            SpatialPredicate::Disjoint,
            &empty_geometry
        )?)
        .first(),
        Err(Error::MissingTile(_))
    ));
    Ok(())
}
