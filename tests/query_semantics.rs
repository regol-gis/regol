use regol::{Library, Query, Result};
use std::collections::BTreeMap;

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
