use regol::{Bounds, Library, Predicate, Query, Result, Selector, Types};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn key_presence_accepts_all_value_encodings() -> Result<()> {
    let lib = Library::new(include_bytes!("fixtures/query_semantics.gol").as_slice())?;
    let original: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/query_semantics.osm.json")).unwrap();
    // The source includes numeric zero, "no", empty text and local strings.
    for key in ["value", "name", "absent"] {
        let expected = original["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|node| node["tags"].get(key).is_some())
            .map(|node| format!("N{}", node["id"]))
            .collect::<BTreeSet<_>>();
        let query = Query::new(Selector::new(Types::NODE).and(Predicate::present(key)));
        let prepared = query.prepare(&lib)?;
        for features in [
            Box::new(lib.select(&query)) as Box<dyn Iterator<Item = _>>,
            Box::new(prepared.in_bounds(Bounds::WORLD)),
        ] {
            let ids = features
                .map(|item| item.map(|f| f.id().to_string()))
                .collect::<Result<BTreeSet<_>>>()?;
            assert_eq!(ids, expected, "{key}");
        }
        assert_eq!(prepared.count()?, expected.len() as u64);
    }
    Ok(())
}

#[test]
fn branch_width_pruning_keeps_every_eligible_feature_at_the_threshold() -> Result<()> {
    let lib = Library::new(include_bytes!("fixtures/stress.gol").as_slice())?;
    let query = Query::all();
    let prepared = query.prepare(&lib)?;
    let width_and_id = |f: regol::Feature<'_, &[u8]>| -> Result<_> {
        let bounds = f.bounds()?;
        Ok((bounds.min.x.abs_diff(bounds.max.x), f.id()))
    };
    let all = prepared
        .in_bounds(Bounds::WORLD)
        .map(|f| width_and_id(f?))
        .collect::<Result<Vec<_>>>()?;
    let edge = all.iter().find(|(width, _)| *width > 0).unwrap().0;
    for minimum in [0, edge, edge.saturating_add(1), u32::MAX] {
        let candidates = prepared
            .candidates_in_bounds_with_min_width(Bounds::WORLD, minimum)
            .map(|f| width_and_id(f?))
            .collect::<Result<Vec<_>>>()?;
        if minimum == u32::MAX {
            assert!(candidates.len() < all.len());
        }
        assert_eq!(
            candidates
                .iter()
                .filter(|(w, _)| *w >= minimum)
                .collect::<Vec<_>>(),
            all.iter()
                .filter(|(w, _)| *w >= minimum)
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

#[test]
fn numeric_text_quoting_regex_and_negation_matrix() -> Result<()> {
    let library = Library::new(include_bytes!("fixtures/query_semantics.gol").as_slice())?;
    let expected: BTreeMap<String, Vec<String>> =
        serde_json::from_str(include_str!("fixtures/query_semantics.queries.json")).unwrap();
    let mut failures = Vec::new();
    for (text, mut want) in expected {
        // SDK 2.0 treats empty strings and escaped exact strings inconsistently.
        // These two expectations come directly from the original input tags.
        if text == r#"n[value=""]"# {
            want = vec!["N16".into()];
        }
        if text == r#"n[value="a\"b\\c"]"# {
            want = vec!["N30".into()];
        }
        if text == "n[value=\"a*b\"]" {
            assert!(Query::parse(&text).is_err());
            continue;
        }
        let query = Query::parse(&text)?;
        let mut actual = library
            .select(&query)
            .map(|f| f.map(|f| f.id().to_string()))
            .collect::<Result<Vec<_>>>()?;
        actual.sort();
        if actual != want {
            failures.push(format!("{text}: {actual:?} != {want:?}"));
        }
        let prepared = query.prepare(&library)?;
        let mut optimized = prepared
            .iter()
            .map(|f| f.map(|f| f.id().to_string()))
            .collect::<Result<Vec<_>>>()?;
        optimized.sort();
        assert_eq!(optimized, actual, "prepared vs simple: {text}");
        assert_eq!(
            prepared.count()?,
            actual.len() as u64,
            "fused count: {text}"
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    Ok(())
}

#[test]
fn query_resource_limits_are_explicit() {
    assert!(Query::parse(&std::iter::repeat_n("n", 257).collect::<Vec<_>>().join(",")).is_err());
    assert!(Query::parse(&format!("n{}", "[name]".repeat(1025))).is_err());
    assert!(Query::parse(&format!("n{}", "[name~x]".repeat(33))).is_err());
    assert!(Query::parse("n[name~\"a{1000000}\"]").is_err());
}
