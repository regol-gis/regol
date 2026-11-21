#![no_main]
use regol::{FeatureType, Library, Limits, Query, Types, source::Source};
use libfuzzer_sys::fuzz_target;
mod buffered;

fn read_source<S: Source>(source: S) {
    let limits = Limits {
        max_traversal_steps: 2000,
        max_tree_depth: 16,
        max_tags: 100,
        max_coordinates: 1000,
        max_members: 100,
    };
    let Ok(mut lib) = Library::with_limits(source, limits) else {
        return;
    };
    // Exercise encoded tag lookup as well as the generic tag iterator. Query
    // preparation can reject a damaged dictionary before iteration begins.
    for text in [
        "*",
        "*[no_such_local_key]",
        "*[!no_such_local_key]",
        "*[name]",
        "na[amenity=cafe]",
        "na[amenity=cafe][name][!disused]",
        "*[!name][capacity>=12]",
        "*[amenity],*[highway]",
        "*[amenity],*[!name]",
    ] {
        let query = Query::parse(text).unwrap();
        if let Ok(prepared) = query.prepare(&lib) {
            // Compare specialized paths with the original traversal/lookup.
            let reference = match text {
                "*" => Some(
                    lib.features()
                        .map(|f| f.map(|f| f.id()))
                        .collect::<regol::Result<Vec<_>>>(),
                ),
                "*[no_such_local_key]"
                | "*[!no_such_local_key]"
                | "na[amenity=cafe]"
                | "na[amenity=cafe][name][!disused]" => {
                    let repeated = match text {
                        "*[no_such_local_key]" => "*[no_such_local_key][no_such_local_key]",
                        "*[!no_such_local_key]" => "*[!no_such_local_key][!no_such_local_key]",
                        "na[amenity=cafe]" => "na[amenity=cafe,cafe]",
                        _ => "na[amenity=cafe,cafe][name][!disused]",
                    };
                    let query = Query::parse(repeated).unwrap();
                    Some(
                        query
                            .prepare(&lib)
                            .and_then(|q| q.iter().map(|f| f.map(|f| f.id())).collect()),
                    )
                }
                _ => None,
            };
            if let Some(reference) = reference {
                let actual = prepared
                    .iter()
                    .map(|f| f.map(|f| f.id()))
                    .collect::<regol::Result<Vec<_>>>();
                assert_eq!(actual.is_ok(), reference.is_ok());
                if let (Ok(actual), Ok(reference)) = (actual, reference) {
                    assert_eq!(actual, reference);
                }
            }
            let expected = prepared.iter().try_fold(0u64, |n, f| f.map(|_| n + 1));
            let counted = prepared.count();
            let folded = prepared.try_fold(0u64, |n, _| Ok(n + 1));
            assert_eq!(expected.is_ok(), folded.is_ok());
            if let (Ok(expected), Ok(folded)) = (&expected, &folded) {
                assert_eq!(expected, folded);
            }
            assert_eq!(expected.is_ok(), counted.is_ok());
            if let (Ok(expected), Ok(counted)) = (expected, counted) {
                assert_eq!(expected, counted);
            }
        }
    }
    let query = Query::parse("na[amenity=cafe]").unwrap();
    let ids = |library: &Library<S>| {
        query
            .prepare(library)
            .and_then(|prepared| {
                prepared
                    .iter()
                    .map(|item| item.map(|f| f.id()))
                    .collect::<regol::Result<Vec<_>>>()
            })
            .map_err(|err| err.to_string())
    };
    let ordinary = ids(&lib);
    if let Ok(bytes) = lib.index_text_key("amenity", Types::NODE.union(Types::AREA), 64 * 1024) {
        assert!(bytes <= 64 * 1024);
        assert_eq!(ordinary, ids(&lib));
    }
    if let Ok(properties) = lib.properties() {
        for p in properties {
            if p.is_err() {
                break;
            }
        }
    }
    for f in lib.features() {
        let Ok(f) = f else {
            break;
        };
        let _ = f.bounds();
        let _ = f.geometry();
        if let Ok(tags) = f.tags() {
            for t in tags {
                if t.is_err() {
                    break;
                }
            }
        }
        if let Ok(parents) = f.parent_relations() {
            for p in parents {
                if p.is_err() {
                    break;
                }
            }
        }
        if f.kind() == FeatureType::Way {
            if let Ok(coords) = f.coordinates() {
                for p in coords {
                    if p.is_err() {
                        break;
                    }
                }
            }
            if let Ok(nodes) = f.way_nodes() {
                for n in nodes {
                    if n.is_err() {
                        break;
                    }
                }
            }
        }
        if f.kind() == FeatureType::Relation {
            if let Ok(members) = f.members() {
                for m in members {
                    if m.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    read_source(data);
    for width in [1, 7, 4096] {
        read_source(buffered::Buffered { bytes: data, width });
    }
});
