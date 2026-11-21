use regol::{Error, Library, Limits, Predicate, Query, Result, Selector, Types};

#[test]
fn postings_match_traversal_order() -> Result<()> {
    let bytes = include_bytes!("fixtures/basic.gol").as_slice();
    let ordinary = Library::new(bytes)?;
    let mut indexed = Library::new(bytes)?;
    for types in [Types::NODE, Types::AREA, Types::ALL] {
        for (key, values) in [
            ("amenity", ["cafe", "bench", "missing"]),
            ("name", ["Café 東京", "West", "missing"]),
            ("missing_key", ["yes", "no", "missing"]),
        ] {
            indexed.index_text_key(key, types, 1024)?;
            for text in values {
                let query = Query::new(Selector::new(types).and(Predicate::equal(key, text)));
                let expected = query
                    .prepare(&ordinary)?
                    .iter()
                    .map(|feature| Ok(feature?.id()))
                    .collect::<Result<Vec<_>>>()?;
                let actual = query
                    .prepare(&indexed)?
                    .iter()
                    .map(|feature| Ok(feature?.id()))
                    .collect::<Result<Vec<_>>>()?;
                assert_eq!(actual, expected, "{key}={text}");
            }
        }
    }
    Ok(())
}

#[test]
fn postings_respect_dictionary_key_and_value_encodings() -> Result<()> {
    let mut bytes = include_bytes!("fixtures/query_semantics.gol").to_vec();
    let dictionary = u32::from_le_bytes(bytes[84..88].try_into().unwrap()) as usize;
    let word = |bytes: &[u8], spelling: &[u8]| {
        dictionary
            + bytes[dictionary..]
                .windows(spelling.len())
                .position(|entry| entry == spelling)
                .unwrap()
            + 1
    };
    let yes = word(&bytes, b"\x03yes");
    bytes[yes..yes + 3].copy_from_slice(b"12m");

    // The key is local in every feature. Its value spells "12m" in both
    // encodings, but only the original dictionary-coded "yes" matches.
    let matches = |bytes: &[u8], value, expected: &[u64]| -> Result<()> {
        let query = Query::new(Selector::new(Types::ALL).and(Predicate::equal("value", value)));
        let ordinary = Library::new(bytes)?;
        let mut indexed = Library::new(bytes)?;
        indexed.index_text_key("value", Types::ALL, 8192)?;
        let simple = ordinary
            .select(&query)
            .map(|feature| Ok(feature?.id().id))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(simple, expected);
        for library in [&ordinary, &indexed] {
            let ids = query
                .prepare(library)?
                .iter()
                .map(|feature| Ok(feature?.id().id))
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(ids, expected, "value={value}");
        }
        Ok(())
    };
    matches(&bytes, "12m", &[15])?;
    matches(&bytes, "12 m", &[7])?;
    let ordinary = Library::new(bytes.as_slice())?;
    for (text, expected) in [
        (r#"*[value="12m","12 m"]"#, vec![7, 15]),
        (r#"*[value="12m","*2m"]"#, vec![6, 15]),
        (r#"*[value!="12m"][value="*2m"]"#, vec![6]),
    ] {
        let query = Query::parse(text)?;
        let ids = |items: Vec<regol::FeatureId>| {
            let mut ids: Vec<_> = items.into_iter().map(|id| id.id).collect();
            ids.sort_unstable();
            ids
        };
        let prepared = query.prepare(&ordinary)?;
        assert_eq!(
            ids(ordinary
                .select(&query)
                .map(|f| f.map(|f| f.id()))
                .collect::<Result<_>>()?),
            expected,
            "{text}"
        );
        assert_eq!(
            ids(prepared
                .iter()
                .map(|f| f.map(|f| f.id()))
                .collect::<Result<_>>()?),
            expected,
            "{text}"
        );
    }

    // Giving the key a common code makes both occurrences inaccessible
    // to the selector because their keys are still stored in local tables.
    let inner = word(&bytes, b"\x05inner");
    bytes[inner..inner + 5].copy_from_slice(b"value");
    matches(&bytes, "12m", &[])?;
    matches(&bytes, "12 m", &[])?;
    Ok(())
}

#[test]
fn build_has_resource_limits() -> Result<()> {
    let bytes = include_bytes!("fixtures/basic.gol").as_slice();
    let mut library = Library::new(bytes)?;
    let used = library.index_text_key("amenity", Types::ALL, 1024)?;
    assert!(matches!(
        library.index_text_key("amenity", Types::ALL, used - 1),
        Err(Error::Limit("text tag index bytes"))
    ));
    assert_eq!(library.index_text_key("amenity", Types::ALL, used)?, used);

    let mut library = Library::with_limits(
        bytes,
        Limits {
            max_traversal_steps: 1,
            ..Limits::default()
        },
    )?;
    assert!(matches!(
        library.index_text_key("amenity", Types::ALL, 1024),
        Err(Error::Limit("spatial traversal steps"))
    ));

    let mut library = Library::with_limits(
        bytes,
        Limits {
            max_tags: 0,
            ..Limits::default()
        },
    )?;
    assert!(matches!(
        library.index_text_key("amenity", Types::ALL, 1024),
        Err(Error::Limit("tag lookup"))
    ));
    Ok(())
}

#[test]
fn unsupported_queries_fall_back_and_numeric_values_keep_their_type() -> Result<()> {
    let bytes = include_bytes!("fixtures/query_semantics.gol").as_slice();
    let ordinary = Library::new(bytes)?;
    let mut indexed = Library::new(bytes)?;
    indexed.index_text_key("value", Types::ALL, 8192)?;
    for text in [
        "*[value=12m]",
        "*[value=12]",
        "n[value=12m]",
        "*[value!=12m]",
        "*[value=12m][name]",
        "*[value=12m,fake]",
        "*[name]",
    ] {
        let query = Query::parse(text)?;
        let ordinary = query.prepare(&ordinary)?;
        let indexed = query.prepare(&indexed)?;
        assert_eq!(
            indexed
                .iter()
                .map(|feature| Ok(feature?.id()))
                .collect::<Result<Vec<_>>>()?,
            ordinary
                .iter()
                .map(|feature| Ok(feature?.id()))
                .collect::<Result<Vec<_>>>()?,
            "{text}",
        );
        let bounds = regol::Bounds::new(-1, -1, 1, 1)?;
        assert_eq!(
            indexed.in_bounds(bounds).count(),
            ordinary.in_bounds(bounds).count()
        );
    }
    Ok(())
}
