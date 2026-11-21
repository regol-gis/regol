use super::*;
use crate::FeatureType;

// Compare full prefixes against the ordinary decoder, including malformed
// non-matching candidates and exact limits.
#[test]
fn equality_scan_preserves_stream_errors_and_budgets() -> Result<()> {
    let source = include_bytes!("../../../tests/fixtures/global.gol").as_slice();
    let original = Library::new(source)?;
    let query = Query::parse("*[building=yes]")?;
    let value_set = Query::parse("*[building=yes,outer]")?;
    let compound = Query::parse("*[building=yes,outer][name]")?;
    let pair = Query::parse("*[building][boundary]")?;
    assert!(matches!(
        pair.prepare(&original)?.plan,
        CompiledPlan::Two(_, _, Some(_))
    ));
    assert!(matches!(
        value_set.prepare(&original)?.plan,
        CompiledPlan::ValueSet(..)
    ));
    assert!(matches!(
        compound.prepare(&original)?.plan,
        CompiledPlan::ValueSet(..)
    ));
    assert!(matches!(
        query.prepare(&original)?.plan,
        CompiledPlan::Equal(..)
    ));
    assert_eq!(query.prepare(&original)?.count()?, 1);
    let compare = |bytes: &[u8], limits: crate::Limits| -> Result<()> {
        let lib = Library::with_limits(bytes, limits)?;
        for bounds in [None, Some(Bounds::new(-10000, -10000, 10000, 10000)?)] {
            let run = |q: &Query, ordinary: bool| -> Result<_> {
                let mut prepared = q.prepare(&lib)?;
                if ordinary {
                    let plan = std::mem::replace(&mut prepared.plan, CompiledPlan::All(Types::ALL));
                    prepared.plan = match plan {
                        CompiledPlan::Equal(types, first, _, rest)
                        | CompiledPlan::ValueSet(types, first, _, rest) => {
                            let mut rest = rest.into_vec();
                            match rest.pop() {
                                Some(second) => CompiledPlan::Two(types, [first, second], None),
                                None => CompiledPlan::One(types, first),
                            }
                        }
                        CompiledPlan::Two(types, predicates, _) => {
                            CompiledPlan::Two(types, predicates, None)
                        }
                        plan => plan,
                    };
                }
                let mut iter = prepared.execute(bounds);
                let trace = iter
                    .by_ref()
                    .map(|f| f.map(|f| f.id()).map_err(|e| e.to_string()))
                    .collect::<Vec<_>>();
                assert!(iter.next().is_none());
                let count = match bounds {
                    Some(b) => prepared.count_in_bounds(b),
                    None => prepared.count(),
                }
                .map_err(|e| e.to_string());
                Ok((trace, count))
            };
            for q in [&query, &value_set, &compound, &pair] {
                assert_eq!(run(q, false)?, run(q, true)?);
            }
        }
        Ok(())
    };
    for max_tags in [0, 1, 2, 3, 100_000] {
        for max_traversal_steps in 0..64 {
            compare(
                source,
                crate::Limits {
                    max_tags,
                    max_traversal_steps,
                    ..Default::default()
                },
            )?;
        }
    }
    for f in original.features() {
        let f = f?;
        let anchor = f.anchor() as usize;
        let ptr = i32::from_le_bytes(source[anchor + 8..anchor + 12].try_into().unwrap());
        let table = (anchor + 8)
            .checked_add_signed((ptr & !1) as isize)
            .unwrap();
        // Invalid kind, inverted geometry bounds, invalid tag pointer,
        // invalid global value code, and a dangling wide/local value.
        let key = original.dictionary().unwrap().key_code("building").unwrap();
        let cases = [
            (anchor, vec![(source[anchor] & !24) | 24]),
            (anchor + 8, i32::MAX.to_le_bytes().to_vec()),
            (
                table,
                ((0xffff_0001u32) | (u32::from(key) << 2))
                    .to_le_bytes()
                    .to_vec(),
            ),
            (
                table,
                ((0x7fff_8003u32) | (u32::from(key) << 2))
                    .to_le_bytes()
                    .to_vec(),
            ),
        ];
        for (start, patch) in cases {
            let mut damaged = source.to_vec();
            damaged[start..start + patch.len()].copy_from_slice(&patch);
            compare(&damaged, Default::default())?;
        }
        if f.kind() != FeatureType::Node {
            let mut damaged = source.to_vec();
            damaged[anchor - 16..anchor - 12].copy_from_slice(&i32::MAX.to_le_bytes());
            compare(&damaged, Default::default())?;
        }
    }
    Ok(())
}

#[test]
fn planner_requires_a_leading_exact_literal_with_one_matching_code() -> Result<()> {
    let original = include_bytes!("../../../tests/fixtures/global.gol").as_slice();
    let lib = Library::new(original)?;
    for (query, uses_probe) in [
        ("*[building=outer]", true),
        ("*[building=unknown_value]", false),
        ("*[building!=outer]", false),
        ("*[building=outer,inner]", false),
        ("*[building=out*]", false),
        ("*[building=1]", false),
        ("*[building=outer][amenity]", true),
        ("*[building=outer][!amenity][!name]", true),
        ("*[amenity][building=outer]", false),
        ("*[building=outer],*[building=inner]", false),
    ] {
        assert_eq!(
            matches!(
                Query::parse(query)?.prepare(&lib)?.plan,
                CompiledPlan::Equal(..)
            ),
            uses_probe,
            "{query}"
        );
    }
    let mut duplicate = original.to_vec();
    let dictionary = u32::from_le_bytes(duplicate[84..88].try_into().unwrap()) as usize;
    let inner = duplicate[dictionary..]
        .windows(6)
        .position(|word| word == b"\x05inner")
        .unwrap();
    let start = dictionary + inner + 1;
    duplicate[start..start + 5].copy_from_slice(b"outer");
    let lib = Library::new(duplicate)?;
    assert!(matches!(
        Query::parse("*[building=outer]")?.prepare(&lib)?.plan,
        CompiledPlan::ProbedOne(..)
    ));
    let query = Query::parse("*[building=outer,unknown_value]")?;
    let prepared = query.prepare(&lib)?;
    let CompiledPlan::ValueSet(_, predicate, _, _) = prepared.plan else {
        panic!("exact value list should use a dictionary bitset");
    };
    assert_eq!(
        predicate
            .global_matches
            .iter()
            .map(|w| w.count_ones())
            .sum::<u32>(),
        2
    );
    Ok(())
}
