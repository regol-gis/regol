use super::*;
use crate::Query;

#[test]
fn absent_local_table_avoids_io_but_keeps_pointer_validation() -> Result<()> {
    use std::cell::Cell;
    struct Observed<'a> {
        bytes: &'a [u8],
        segments: Cell<usize>,
    }
    impl Source for Observed<'_> {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read_exact_at(&self, p: u64, out: &mut [u8]) -> std::io::Result<()> {
            self.bytes.read_exact_at(p, out)
        }
        fn segment(&self, p: u64) -> std::io::Result<Option<(u64, &[u8])>> {
            self.segments.set(self.segments.get() + 1);
            self.bytes.segment(p)
        }
    }
    let bytes = include_bytes!("../../tests/fixtures/global.gol").as_slice();
    let library = Library::new(Observed {
        bytes,
        segments: Cell::new(0),
    })?;
    let feature = library
        .features()
        .find_map(|f| match f {
            Ok(f) if f.tag_pointer & 1 == 0 => Some(Ok(f)),
            Err(e) => Some(Err(e)),
            _ => None,
        })
        .expect("fixture has a feature without local tags")?;
    assert!(
        library
            .dictionary()
            .unwrap()
            .key_code("no_such_local_key")
            .is_none()
    );
    library.source.segments.set(0);
    assert!(
        feature
            .encoded_local_tag(
                "no_such_local_key",
                &mut ReadWindow::new(library.source()),
                &mut ReadWindow::new(library.source()),
            )?
            .is_none()
    );
    assert_eq!(library.source.segments.get(), 0);
    let bad = Feature {
        tag_pointer: i32::MAX & !1,
        ..feature
    };
    assert!(
        bad.encoded_local_tag(
            "no_such_local_key",
            &mut ReadWindow::new(library.source()),
            &mut ReadWindow::new(library.source()),
        )
        .is_err()
    );
    assert_eq!(library.source.segments.get(), 0);
    Ok(())
}

fn require_query_error(bytes: &[u8], text: &str) -> Result<()> {
    let library = Library::new(bytes)?;
    let query = Query::parse(text)?;
    assert!(library.select(&query).collect::<Result<Vec<_>>>().is_err());
    let prepared = query.prepare(&library)?;
    let mut results = prepared.iter();
    assert!(results.by_ref().collect::<Result<Vec<_>>>().is_err());
    assert!(results.next().is_none(), "error must fuse the iterator");
    #[cfg(feature = "parallel")]
    {
        let executor = crate::QueryExecutor::new(2)?;
        let result = executor.with_query(&prepared, None, |results| {
            let result = results.by_ref().collect::<Result<Vec<_>>>();
            assert!(results.next().is_none());
            result
        });
        assert!(result.and_then(|result| result).is_err());
    }
    Ok(())
}

#[test]
fn local_plan_matches_raw_key_bytes_while_tag_reads_validate_utf8() -> Result<()> {
    let original = include_bytes!("../../tests/fixtures/stress.gol");
    let library = Library::new(original.as_slice())?;
    let feature = library
        .features()
        .find_map(|f| match f {
            Ok(f) if f.tag_pointer & 1 != 0 => Some(Ok(f)),
            Err(e) => Some(Err(e)),
            _ => None,
        })
        .expect("fixture has local tags")?;
    let table = add(feature.anchor() + 8, i64::from(feature.tag_pointer & !1))?;
    let word = original.as_slice().i32(table - 4)?;
    let mut key = add(table & !3, i64::from((word & !7) >> 1))?;
    assert!(original.as_slice().varint(&mut key)? > 0);
    let mut damaged = original.to_vec();
    damaged[key as usize] = 255;
    let damaged_lib = Library::new(damaged.as_slice())?;
    // Search-time key matching compares raw bytes without validating non-matching keys.
    for text in ["*[no_such_local_key]", "*[!no_such_local_key]"] {
        let query = Query::parse(text)?;
        let simple = damaged_lib
            .select(&query)
            .map(|f| f.map(|f| f.id()))
            .collect::<Result<Vec<_>>>()?;
        let prepared = query.prepare(&damaged_lib)?;
        let ids = prepared
            .iter()
            .map(|f| f.map(|f| f.id()))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(simple, ids);
    }
    // User-facing tag iteration and GOL integrity checks still reject invalid UTF-8.
    let mut saw_utf8_err = false;
    for f in damaged_lib.features() {
        let f = f?;
        for tag in f.tags()? {
            if tag.is_err() {
                saw_utf8_err = true;
                break;
            }
        }
    }
    assert!(saw_utf8_err);
    Ok(())
}

#[test]
fn local_string_window_preserves_general_decoder_errors_and_long_encodings() -> Result<()> {
    let library = Library::new(include_bytes!("../../tests/fixtures/basic.gol").as_slice())?;
    let mut cases = vec![
        vec![],
        vec![0],
        vec![1, b'a'],
        vec![2, b'n', b'o'],
        vec![2, b'N', b'O'],
        vec![2, 0xc3, 0xa9],
        vec![1, 255],
        vec![3, b'a'],
        vec![0x80, 0],
        vec![0xff; 10],
    ];
    let mut noncanonical = vec![0x80; 9];
    noncanonical.push(0);
    cases.push(noncanonical);
    for length in [127usize, 128, 16383, 16384, 32767, 32768] {
        let mut n = length;
        let mut bytes = Vec::new();
        while n >= 128 {
            bytes.push((n as u8 & 127) | 128);
            n >>= 7;
        }
        bytes.push(n as u8);
        bytes.resize(bytes.len() + length, b'x');
        cases.push(bytes);
    }
    for bytes in &cases {
        for end in [
            0,
            1.min(bytes.len()),
            bytes.len().saturating_sub(1),
            bytes.len(),
        ] {
            for source_end in [end, bytes.len()] {
                let source = &bytes[..source_end];
                let window = ReadWindow::at(&source, 0)?;
                // Exercise the string decoder independently of headers:
                // only the library type and enclosing tile range are used.
                let feature = Feature {
                    library: &library,
                    anchor_offset: 0,
                    tile: TileExtent::new(0, end as u64)?,
                    header: 0,
                    tag_pointer: 0,
                };
                let expected = feature
                    .local_string_source(&source, 0)
                    .map(|s| s.into_owned())
                    .map_err(|e| e.to_string());
                let actual = feature
                    .local_string_window(&window, 0)
                    .map(|s| s.into_owned())
                    .map_err(|e| e.to_string());
                assert_eq!(
                    actual,
                    expected,
                    "source={source_end}, tile={end}, prefix={:?}",
                    &bytes[..bytes.len().min(10)]
                );
                let expected_bytes = feature
                    .local_bytes_source(&source, 0)
                    .map(|b| b.into_owned())
                    .map_err(|e| e.to_string());
                assert_eq!(
                    feature
                        .local_string_equals(&window, 0, "no")
                        .map_err(|e| e.to_string()),
                    expected_bytes.map(|b| b.as_slice() == b"no"),
                );
            }
        }
    }
    Ok(())
}

#[test]
fn prepared_predicates_match_raw_local_bytes_while_tag_reads_validate_utf8() -> Result<()> {
    let original = include_bytes!("../../tests/fixtures/basic.gol");
    let library = Library::new(original.as_slice())?;
    let mut payload = None;
    for feature in library.features() {
        let feature = feature?;
        if let Some(EncodedValue::Local { field, delta }) = feature.encoded_tag(
            "name",
            library.dictionary().unwrap().key_code("name"),
            &mut ReadWindow::new(library.source()),
            &mut ReadWindow::new(library.source()),
        )? {
            let mut p = feature.local_value_target(field, delta)?;
            if original.as_slice().varint(&mut p)? > 0 {
                payload = Some(p as usize);
                break;
            }
        }
    }
    let mut bytes = original.to_vec();
    bytes[payload.expect("fixture has local names")] = 255;
    let library = Library::new(bytes.as_slice())?;
    // Search-time local-string matching skips UTF-8 validation on candidates;
    // unprepared and prepared execution agree on every selector.
    for text in [
        "*[name=R*]",
        "*[name=Royal],*[name=Other]",
        "*[name=R*][gol_reader_missing_test_key]",
        "*[name]",
        "*[name=yes]",
    ] {
        let query = Query::parse(text)?;
        let simple = library
            .select(&query)
            .map(|f| f.map(|f| f.id()))
            .collect::<Result<Vec<_>>>()?;
        let prepared = query.prepare(&library)?;
        let ids = prepared
            .iter()
            .map(|f| f.map(|f| f.id()))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(simple, ids);
    }
    // Reading the corrupted tag value directly still rejects invalid UTF-8.
    let mut saw_utf8_err = false;
    for f in library.features() {
        if f?.tag("name").is_err() {
            saw_utf8_err = true;
            break;
        }
    }
    assert!(saw_utf8_err);
    let query = Query::parse("*[gol_reader_missing_test_key][name]")?;
    assert!(
        query
            .prepare(&library)?
            .iter()
            .collect::<Result<Vec<_>>>()?
            .is_empty()
    );
    Ok(())
}

#[test]
fn prepared_bitsets_reject_undefined_global_values() -> Result<()> {
    let original = include_bytes!("../../tests/fixtures/global.gol");
    let library = Library::new(original.as_slice())?;
    let code = library
        .dictionary()
        .unwrap()
        .key_code("building")
        .expect("common building key");
    let mut payload = None;
    for feature in library.features() {
        let feature = feature?;
        let mut p = library.source.relative(feature.anchor() + 8, !1)?;
        loop {
            let word = library.source.u16(p)?;
            if (word >> 2) & 8191 == code && word & 3 == 1 {
                payload = Some(p as usize + 2);
                break;
            }
            if word & 0x8000 != 0 {
                break;
            }
            p += 4 + u64::from(word & 2);
        }
        if payload.is_some() {
            break;
        }
    }
    let p = payload.expect("fixture has a global building value");
    let mut bytes = original.to_vec();
    bytes[p..p + 2].copy_from_slice(&u16::MAX.to_le_bytes());
    require_query_error(&bytes, "*[building]")?;
    require_query_error(&bytes, "*[building=yes],*[building=outer]")
}
