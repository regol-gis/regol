use regol::{Error, FeatureType, Library, Limits};

const BASIC: &[u8] = include_bytes!("fixtures/basic.gol");

fn exercise(data: &[u8]) {
    let limits = Limits {
        max_traversal_steps: 1000,
        max_tree_depth: 16,
        max_tags: 100,
        max_coordinates: 100,
        max_members: 100,
    };
    if let Ok(lib) = Library::with_limits(data, limits) {
        for f in lib.features() {
            let Ok(f) = f else {
                break;
            };
            let _ = f.bounds();
            let _ = f.geometry();
            if let Ok(parents) = f.parent_relations() {
                for parent in parents {
                    if parent.is_err() {
                        break;
                    }
                }
            }
            if let Ok(tags) = f.tags() {
                for tag in tags {
                    if tag.is_err() {
                        break;
                    }
                }
            }
            if f.kind() == FeatureType::Way {
                if let Ok(coords) = f.coordinates() {
                    for point in coords {
                        if point.is_err() {
                            break;
                        }
                    }
                }
                if let Ok(nodes) = f.way_nodes() {
                    for node in nodes {
                        if node.is_err() {
                            break;
                        }
                    }
                }
            }
            if f.kind() == FeatureType::Relation {
                if let Ok(members) = f.members() {
                    for member in members {
                        if member.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn every_truncation_is_panic_free() {
    for end in 0..BASIC.len() {
        exercise(&BASIC[..end]);
    }
}

#[test]
fn deterministic_mutations_are_panic_free() {
    let mut state = 0x853c49e6748fea9bu64;
    let mut bytes = BASIC.to_vec();
    for _ in 0..5000 {
        bytes.copy_from_slice(BASIC);
        for _ in 0..4 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let p = (state as usize) % bytes.len();
            bytes[p] ^= (state >> 32) as u8;
        }
        exercise(&bytes);
    }
}

#[test]
fn unsupported_version_and_bad_header() {
    let mut data = BASIC.to_vec();
    data[4..8].copy_from_slice(&1u32.to_le_bytes());
    assert!(matches!(
        Library::new(data.as_slice()),
        Err(Error::UnsupportedVersion(1))
    ));
    data[4..8].copy_from_slice(&2u32.to_le_bytes());
    data[16] = 2;
    assert!(matches!(
        Library::new(data.as_slice()),
        Err(Error::Invalid { .. })
    ));
    data[16] = 0;
    data[0] = 0;
    assert!(matches!(
        Library::new(data.as_slice()),
        Err(Error::Invalid { .. })
    ));
}

#[test]
fn traversal_budget_fails_explicitly_and_iterator_is_fused() {
    let lib = Library::with_limits(
        BASIC,
        Limits {
            max_traversal_steps: 1,
            ..Limits::default()
        },
    )
    .unwrap();
    let mut features = lib.features();
    assert!(matches!(features.next(), Some(Err(Error::Limit(_)))));
    assert!(features.next().is_none());
    assert!(features.next().is_none());
}

#[test]
fn missing_tile_does_not_silently_drop_data() {
    let mut data = BASIC.to_vec();
    let page = u32::from_le_bytes(data[152..156].try_into().unwrap()) as usize * 4096;
    data[page + 4..page + 8].fill(0);
    let lib = Library::new(data.as_slice()).unwrap();
    assert!(matches!(
        lib.features().next(),
        Some(Err(Error::MissingTile(1)))
    ));
}

#[test]
fn cyclic_rtree_is_bounded() {
    let mut data = BASIC.to_vec();
    let index = u32::from_le_bytes(data[152..156].try_into().unwrap()) as usize * 4096;
    let tile =
        (u32::from_le_bytes(data[index + 4..index + 8].try_into().unwrap()) >> 2) as usize * 4096;
    let root =
        tile + 8 + i32::from_le_bytes(data[tile + 8..tile + 12].try_into().unwrap()) as usize;
    let branch =
        root + (i32::from_le_bytes(data[root..root + 4].try_into().unwrap()) & !3) as usize;
    data[branch..branch + 4].copy_from_slice(&1u32.to_le_bytes()); // final sibling; points back to itself
    let lib = Library::with_limits(
        data.as_slice(),
        Limits {
            max_tree_depth: 8,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(matches!(lib.features().next(), Some(Err(Error::Limit(_)))));
}

#[test]
fn cyclic_relation_expansion_is_bounded_but_direct_members_remain_readable() {
    let mut data = BASIC.to_vec();
    let anchor = (0..data.len() - 16)
        .step_by(4)
        .find(|&p| {
            let word = u64::from_le_bytes(data[p..p + 8].try_into().unwrap());
            word >> 12 == 22 && (word >> 3) & 3 == 2
        })
        .unwrap();
    let body = (anchor as i64
        + 12
        + i64::from(i32::from_le_bytes(
            data[anchor + 12..anchor + 16].try_into().unwrap(),
        ))) as usize;
    let old = i32::from_le_bytes(data[body..body + 4].try_into().unwrap());
    assert_eq!(old & 2, 0);
    let delta = (anchor as i64 - (body & !3) as i64) / 4;
    data[body..body + 4].copy_from_slice(&(((delta as i32) << 3) | (old & 7)).to_le_bytes());
    let lib = Library::new(data.as_slice()).unwrap();
    let relation = lib
        .features()
        .map(Result::unwrap)
        .find(|f| f.id().to_string() == "R22")
        .unwrap();
    assert_eq!(
        relation
            .members()
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .feature
            .id(),
        relation.id()
    );
    assert!(matches!(
        relation.geometry(),
        Err(Error::Geometry("cyclic relation"))
    ));
}

#[test]
fn geometry_budget_applies_across_members_and_nested_relations() {
    let lib = Library::with_limits(
        BASIC,
        Limits {
            max_coordinates: 4,
            ..Limits::default()
        },
    )
    .unwrap();
    let relation = lib
        .features()
        .map(Result::unwrap)
        .find(|f| f.id().to_string() == "R22")
        .unwrap();
    assert!(matches!(relation.geometry(), Err(Error::Limit(_))));
}

#[test]
fn field_limits_report_errors_and_stop_instead_of_truncating() {
    let lib = Library::with_limits(
        BASIC,
        Limits {
            max_tags: 1,
            max_members: 1,
            max_coordinates: 2,
            ..Limits::default()
        },
    )
    .unwrap();
    let features: Vec<_> = lib.features().map(Result::unwrap).collect();
    let node = features
        .iter()
        .copied()
        .find(|f| f.id().to_string() == "N1")
        .unwrap();
    let mut tags = node.tags().unwrap();
    assert!(tags.next().unwrap().is_ok());
    assert!(matches!(tags.next(), Some(Err(Error::Limit(_)))));
    assert!(tags.next().is_none());
    let relation = features
        .iter()
        .copied()
        .find(|f| f.id().to_string() == "R21")
        .unwrap();
    let mut members = relation.members().unwrap();
    assert!(members.next().unwrap().is_ok());
    assert!(matches!(members.next(), Some(Err(Error::Limit(_)))));
    assert!(members.next().is_none());
    let way = features
        .iter()
        .copied()
        .find(|f| f.id().to_string() == "W10")
        .unwrap();
    assert!(matches!(way.coordinates(), Err(Error::Limit(_))));
    let mut nodes = way.feature_nodes().unwrap();
    assert!(nodes.next().unwrap().is_ok());
    assert!(matches!(nodes.next(), Some(Err(Error::Limit(_)))));
    assert!(nodes.next().is_none());
}
