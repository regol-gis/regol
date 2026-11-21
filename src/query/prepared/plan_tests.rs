use super::*;

#[test]
fn compiled_dictionary_bits_match_scalar_truth_and_equality() -> Result<()> {
    let library = Library::new(include_bytes!("../../../tests/fixtures/global.gol").as_slice())?;
    for predicate in [
        Predicate::truthy("key"),
        Predicate::falsy("key"),
        Predicate::equal("key", ""),
        Predicate::equal("key", "yes"),
        Predicate::not_equal("key", "no"),
        Predicate::not_equal("key", "absent é"),
        Predicate {
            key: "key".into(),
            test: Test::Equal {
                values: ["no", "no", "yes", "", "unknown"]
                    .into_iter()
                    .map(|text| Literal::Exact(text.into()))
                    .collect(),
                negate: true,
            },
        },
    ] {
        let bits = predicate.global_matches(&library)?;
        for code in 0..library.string_count {
            let value = TagValue::Text(library.global_string(code as u16)?);
            assert_eq!(
                bits[code / 64] & (1 << (code % 64)) != 0,
                predicate.matches_value(Some(value)),
                "{predicate:?}, global code {code}"
            );
        }
    }
    Ok(())
}

#[test]
fn local_key_plans_match_generic_lookup_under_tag_limits() -> Result<()> {
    let mut exercised = 0;
    for bytes in [
        include_bytes!("../../../tests/fixtures/basic.gol").as_slice(),
        include_bytes!("../../../tests/fixtures/stress.gol").as_slice(),
        include_bytes!("../../../tests/fixtures/query_semantics.gol").as_slice(),
    ] {
        for max_tags in [0, 1, 3, 100_000] {
            let library = Library::with_limits(
                bytes,
                crate::Limits {
                    max_tags,
                    ..Default::default()
                },
            )?;
            for key in ["test:group", "value", "name", "no_such_local_key"] {
                if library.dictionary().unwrap().key_code(key).is_some() {
                    continue;
                }
                for test in [
                    key.to_owned(),
                    format!("!{key}"),
                    format!("{key}!=12"),
                    format!("{key}>=2"),
                ] {
                    let query = Query::parse(&format!("*[{test}]"))?;
                    let optimized = query.prepare(&library)?;
                    let mut generic = query.prepare(&library)?;
                    let CompiledPlan::LocalOne(types, predicate) = generic.plan else {
                        panic!("dictionary-absent key must use the local plan");
                    };
                    generic.plan = CompiledPlan::One(types, predicate);
                    let collect = |query: &PreparedQuery<'_, '_, _>| {
                        query
                            .iter()
                            .map(|f| f.map(|f| f.id()))
                            .collect::<Result<Vec<_>>>()
                            .map_err(|e| e.to_string())
                    };
                    assert_eq!(
                        collect(&optimized),
                        collect(&generic),
                        "{test}: limit={max_tags}"
                    );
                    assert_eq!(
                        optimized.count().map_err(|e| e.to_string()),
                        generic.count().map_err(|e| e.to_string())
                    );
                    exercised += 1;
                }
            }
        }
    }
    assert!(exercised >= 64);
    Ok(())
}

#[test]
fn compiled_edges_preserve_empty_unicode_and_literal_text_semantics() {
    let texts = [
        "",
        "Royal",
        "Royal Oak",
        "The Royal",
        "The Royal Oak",
        "royal",
        "é",
        "été",
        "a*b",
        "no",
        "3",
    ];
    let cases: &[(&str, &[usize])] = &[
        ("", &[0]),
        ("*", &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        ("***", &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
        ("Royal", &[1]),
        ("Royal*", &[1, 2]),
        ("*Royal", &[1, 3]),
        ("*Royal*", &[1, 2, 3, 4]),
        ("**Royal**", &[1, 2, 3, 4]),
        ("é*", &[6, 7]),
        ("*é", &[6, 7]),
        ("*é*", &[6, 7]),
        // Programmatic predicates accept interior literal stars; the query
        // parser rejects them separately. Preserve that existing behavior.
        ("a*b", &[8]),
        ("3", &[10]),
    ];
    for &(pattern, expected) in cases {
        let literal = Literal::text(pattern.into());
        let actual: Vec<_> = texts
            .iter()
            .enumerate()
            .filter_map(|(index, text)| {
                literal
                    .matches(&TagValue::Text((*text).into()))
                    .then_some(index)
            })
            .collect();
        assert_eq!(actual, expected, "pattern {pattern:?}");
        assert!(!literal.matches(&TagValue::Number {
            mantissa: 3,
            scale: 0,
        }));
    }
}

fn unnormalized(text: &str) -> Result<Query> {
    let selectors = text
        .split(",*[")
        .enumerate()
        .map(|(index, selector)| {
            let text = if index == 0 {
                selector.to_owned()
            } else {
                format!("*[{selector}")
            };
            Query::parse(&text).map(|query| query.selectors.into_iter().next().unwrap())
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Query { selectors })
}
#[test]
fn adjacent_equalities_keep_the_generic_scan_order_errors_and_limits() -> Result<()> {
    for (bytes, text) in [
        (
            include_bytes!("../../../tests/fixtures/global.gol").as_slice(),
            "*[building=yes,outer],*[building=inner],*[building=yes]",
        ),
        (
            include_bytes!("../../../tests/fixtures/query_semantics.gol").as_slice(),
            r#"*[value=3],*[value="3"],*[value=pre*],*[value=*end],*[value=*mid*]"#,
        ),
        (
            include_bytes!("../../../tests/fixtures/query_semantics.gol").as_slice(),
            "*[name=\"été\"],*[name=\"\"],*[name=\"Royal Oak\"]",
        ),
        (
            include_bytes!("../../../tests/fixtures/query_semantics.gol").as_slice(),
            "*[test:group=data],*[test:group=missing]",
        ),
    ] {
        let query = Query::parse(text)?;
        let generic = unnormalized(text)?;
        assert_eq!(query.selectors.len(), 1);
        for max_tags in [0, 1, 2, 100_000] {
            for max_traversal_steps in [0, 3, 20, 10_000] {
                let library = Library::with_limits(
                    bytes,
                    crate::Limits {
                        max_tags,
                        max_traversal_steps,
                        ..Default::default()
                    },
                )?;
                let unprepared = |query| {
                    library
                        .select(query)
                        .map(|f| f.map(|f| f.id()).map_err(|e| e.to_string()))
                        .collect::<Vec<_>>()
                };
                assert_eq!(unprepared(&query), unprepared(&generic), "{text}");
                let optimized = query.prepare(&library)?;
                let reference = generic.prepare(&library)?;
                assert!(matches!(
                    optimized.plan,
                    CompiledPlan::One(..) | CompiledPlan::LocalOne(..) | CompiledPlan::ValueSet(..)
                ));
                assert!(matches!(reference.plan, CompiledPlan::Any(_)));
                assert_eq!(
                    (
                        optimized.types,
                        optimized.required_keys,
                        optimized.alternative_keys
                    ),
                    (
                        reference.types,
                        reference.required_keys,
                        reference.alternative_keys
                    ),
                );
                for bounds in [None, Some(Bounds::new(-10_000, -10_000, 10_000, 10_000)?)] {
                    let trace = |query: &PreparedQuery<'_, '_, _>| {
                        query
                            .execute(bounds)
                            .map(|f| f.map(|f| f.id()).map_err(|e| e.to_string()))
                            .collect::<Vec<_>>()
                    };
                    let count = |query: &PreparedQuery<'_, '_, _>| {
                        match bounds {
                            Some(bounds) => query.count_in_bounds(bounds),
                            None => query.count(),
                        }
                        .map_err(|e| e.to_string())
                    };
                    assert_eq!(trace(&optimized), trace(&reference), "{text} {bounds:?}");
                    assert_eq!(count(&optimized), count(&reference), "{text} {bounds:?}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn mixed_alternatives_keep_their_original_plan() -> Result<()> {
    let bytes = include_bytes!("../../../tests/fixtures/global.gol").as_slice();
    let library = Library::new(bytes)?;
    for text in [
        "n[building=yes],a[building=outer]",
        "*[building=yes],*[amenity=cafe]",
        "*[building=yes],*[building!=outer]",
        "*[building=yes],*[building=outer][name]",
    ] {
        assert!(
            matches!(
                Query::parse(text)?.prepare(&library)?.plan,
                CompiledPlan::Any(_)
            ),
            "{text}"
        );
    }
    Ok(())
}

#[test]
fn parser_enforces_selector_limit_before_folding_same_key_arms() -> Result<()> {
    let selectors = vec!["n[amenity=cafe]"; 256].join(",");
    let normalized = Query::parse(&selectors)?;
    assert_eq!(normalized.selectors.len(), 1);
    assert!(matches!(
        Query::parse(&(selectors + ",n[amenity=pub]")),
        Err(crate::Error::Query { .. })
    ));
    Ok(())
}

#[test]
fn builder_merges_adjacent_equalities_without_reordering_other_arms() -> Result<()> {
    use super::super::Selector;
    let first = Selector::new(Types::ALL).and(Predicate::equal("building", "yes"));
    let second = Selector::new(Types::ALL).and(Predicate::equal("building", "outer"));
    let third = Selector::new(Types::ALL).and(Predicate::truthy("name"));
    let optimized = Query::new(first.clone())
        .or(second.clone())
        .or(third.clone());
    let reference = Query {
        selectors: vec![first, second, third],
    };
    assert_eq!(optimized.selectors.len(), 2);
    let lib = Library::new(include_bytes!("../../../tests/fixtures/global.gol").as_slice())?;
    let ids = |query: &Query| {
        lib.select(query)
            .map(|feature| feature.map(|f| f.id()))
            .collect::<Result<Vec<_>>>()
    };
    assert_eq!(ids(&optimized)?, ids(&reference)?);
    let optimized = optimized.prepare(&lib)?;
    let reference = reference.prepare(&lib)?;
    assert_eq!(optimized.required_keys, reference.required_keys);
    assert_eq!(optimized.alternative_keys, reference.alternative_keys);
    assert_eq!(optimized.count()?, reference.count()?);
    Ok(())
}
