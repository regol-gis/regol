#![forbid(unsafe_code)]
use regol::{Bounds, Library, Query, Result};

#[test]
fn union_index_filters_match_decoded_selection_including_unindexed_arms() -> Result<()> {
    for name in [
        "basic",
        "global",
        "tiled_global",
        "stress",
        "query_semantics",
        "wide_exports",
    ] {
        let library = Library::new(std::fs::read(format!("tests/fixtures/{name}.gol"))?)?;
        for text in [
            "*[amenity=cafe],*[highway]",
            "*[amenity=cafe],*[!name]",
            "*[amenity=cafe],*[unknown_key]",
            "*[amenity=cafe],*",
            "*[amenity=cafe][name],*[highway][name]",
            "n[amenity=cafe],wa[highway]",
            "a[natural=water],w[waterway]",
            "*[landuse=forest],*[natural=wood]",
            "*[name~\"a.*\"],*[highway]",
            "*[value!=12],*[highway]",
        ] {
            let query = Query::parse(text)?;
            let prepared = query.prepare(&library)?;
            for bounds in [Bounds::WORLD, Bounds::new(-30000, -30000, 30000, 30000)?] {
                // The simple evaluator does not use prepared category masks.
                let mut expected = library
                    .select_in(&query, bounds)
                    .map(|f| f.map(|f| f.id()))
                    .collect::<Result<Vec<_>>>()?;
                let mut actual = prepared
                    .in_bounds(bounds)
                    .map(|f| f.map(|f| f.id()))
                    .collect::<Result<Vec<_>>>()?;
                expected.sort_unstable();
                actual.sort_unstable();
                assert_eq!(actual, expected, "{name}: {text}");
                assert_eq!(prepared.count_in_bounds(bounds)?, expected.len() as u64);
            }
        }
    }
    Ok(())
}
