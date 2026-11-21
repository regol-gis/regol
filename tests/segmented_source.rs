#![forbid(unsafe_code)]
use regol::{FeatureId, Geometry, Library, Result, source::Source};
use std::io;

struct Segments<'a> {
    bytes: &'a [u8],
    width: usize,
}
impl Source for Segments<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
        self.bytes.read_exact_at(offset, out)
    }
    fn segment(&self, offset: u64) -> io::Result<Option<(u64, &[u8])>> {
        let offset = usize::try_from(offset).map_err(|_| io::ErrorKind::UnexpectedEof)?;
        let start = offset / self.width * self.width;
        let end = start.saturating_add(self.width).min(self.bytes.len());
        let bytes = self
            .bytes
            .get(start..end)
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        Ok(Some((start as u64, bytes)))
    }
}

fn shapes<S: Source>(library: Library<S>) -> Result<Vec<(FeatureId, Geometry)>> {
    library
        .features()
        .map(|feature| {
            let feature = feature?;
            Ok((feature.id(), feature.geometry()?))
        })
        .collect()
}

#[test]
fn owned_read_windows_preserve_geometry_and_prepared_query_results() -> Result<()> {
    use regol::Query;
    struct Buffered<'a>(Segments<'a>);
    impl Source for Buffered<'_> {
        fn len(&self) -> u64 {
            self.0.len()
        }
        fn read_buffer_size(&self) -> usize {
            self.0.width
        }
        fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
            self.0.read_exact_at(offset, out)
        }
    }
    let geometry = include_bytes!("fixtures/geometry.gol").as_slice();
    let expected_shapes = shapes(Library::new(geometry)?)?;
    let bytes = include_bytes!("fixtures/basic.gol").as_slice();
    let simple = Library::new(bytes)?;
    for width in [1, 7, 31, 4096] {
        assert_eq!(
            shapes(Library::new(Buffered(Segments {
                bytes: geometry,
                width
            }))?)?,
            expected_shapes
        );
        let buffered = Library::new(Buffered(Segments { bytes, width }))?;
        for text in [
            "*",
            "*[name]",
            "*[!name]",
            "na[amenity=cafe]",
            "*[value>=12]",
            "*[name][name]",
        ] {
            let query = Query::parse(text)?;
            let expected = simple
                .select(&query)
                .map(|f| f.map(|f| f.id()))
                .collect::<Result<Vec<_>>>()?;
            let actual = query
                .prepare(&buffered)?
                .iter()
                .map(|f| f.map(|f| f.id()))
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(actual, expected, "buffer width={width}, query={text}");
            assert_eq!(query.prepare(&buffered)?.count()?, expected.len() as u64);
            let folded = query
                .prepare(&buffered)?
                .try_fold(Vec::new(), |mut ids, feature| {
                    ids.push(feature.id());
                    Ok(ids)
                })?;
            assert_eq!(folded, expected);
        }
    }
    Ok(())
}

#[test]
fn headers_and_geometry_agree_across_short_segments_and_fallbacks() -> Result<()> {
    let bytes = include_bytes!("fixtures/geometry.gol").as_slice();
    let expected = shapes(Library::new(bytes)?)?;
    assert!(!expected.is_empty());
    for width in [1, 7, 10, 11, 12, 13, 16, 20, 31, 32, 257] {
        let library = Library::new(Segments { bytes, width })?;
        assert_eq!(shapes(library)?, expected, "segment width={width}");
    }
    Ok(())
}

#[test]
fn spatial_windows_match_independent_feature_bounds_at_segment_edges() -> Result<()> {
    use regol::Bounds;
    let bytes = include_bytes!("fixtures/geometry.gol").as_slice();
    let library = Library::new(bytes)?;
    let records = library
        .features()
        .map(|feature| {
            let feature = feature?;
            Ok((feature.id(), feature.bounds()?))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut windows = vec![Bounds::WORLD, Bounds::new(-1, -1, 1, 1)?];
    for &(_, bounds) in &records {
        windows.push(bounds);
        windows.push(Bounds::new(
            bounds.min.x,
            bounds.min.y,
            bounds.min.x,
            bounds.min.y,
        )?);
        windows.push(Bounds::new(
            bounds.max.x,
            bounds.max.y,
            bounds.max.x,
            bounds.max.y,
        )?);
    }
    for width in [1, 7, 8, 10, 11, 12, 13, 15, 16, 17, 20, 31, 32, 33, 257] {
        let library = Library::new(Segments { bytes, width })?;
        for &bounds in &windows {
            let mut expected: Vec<_> = records
                .iter()
                .filter(|(_, b)| bounds.intersects(*b))
                .map(|(id, _)| *id)
                .collect();
            let mut actual = library
                .in_bounds(bounds)
                .map(|f| f.map(|f| f.id()))
                .collect::<Result<Vec<_>>>()?;
            expected.sort_unstable();
            actual.sort_unstable();
            assert_eq!(actual, expected, "segment width={width}, bounds={bounds:?}");
        }
    }
    Ok(())
}

#[test]
fn dictionary_equality_crosses_record_and_tag_window_edges() -> Result<()> {
    use regol::Query;
    let bytes = include_bytes!("fixtures/stress.gol").as_slice();
    let reference = Library::new(bytes)?;
    let generic = Query::parse("na[amenity=cafe,cafe]")?;
    let expected = generic
        .prepare(&reference)?
        .iter()
        .map(|f| f.map(|f| f.id()))
        .collect::<Result<Vec<_>>>()?;
    assert!(!expected.is_empty());
    let query = Query::parse("na[amenity=cafe]")?;
    for width in [1, 7, 19, 20, 31, 32, 64, 4096] {
        let library = Library::new(Segments { bytes, width })?;
        let prepared = query.prepare(&library)?;
        let ids = prepared
            .iter()
            .map(|f| f.map(|f| f.id()))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(ids, expected, "segment width={width}");
        assert_eq!(prepared.count()?, expected.len() as u64);
    }
    Ok(())
}

#[test]
fn prepared_tag_queries_match_general_decoding_across_short_segments() -> Result<()> {
    use regol::{Predicate, Query, Selector, TagValue, Types};
    let bytes = include_bytes!("fixtures/basic.gol").as_slice();
    let library = Library::new(bytes)?;
    let mut queries = Vec::new();
    for feature in library.features() {
        let mut keys = Vec::new();
        for tag in feature?.tags()? {
            let tag = tag?;
            keys.push(tag.key.to_string());
            queries.push(Query::new(
                Selector::new(Types::ALL).and(Predicate::truthy(tag.key.as_ref())),
            ));
            queries.push(Query::new(
                Selector::new(Types::ALL).and(Predicate::falsy(tag.key.as_ref())),
            ));
            let predicate = match tag.value {
                TagValue::Text(value) => Predicate::equal(tag.key.as_ref(), value.as_ref()),
                value => Predicate::equal_number(tag.key.as_ref(), value.as_number().unwrap())?,
            };
            queries.push(Query::new(Selector::new(Types::ALL).and(predicate)));
        }
        for pair in keys.windows(2) {
            for (first, second) in [(&pair[0], &pair[1]), (&pair[1], &pair[0])] {
                queries.push(Query::new(
                    Selector::new(Types::ALL)
                        .and(Predicate::truthy(first))
                        .and(Predicate::truthy(second)),
                ));
                queries.push(Query::new(
                    Selector::new(Types::ALL)
                        .and(Predicate::truthy(first))
                        .and(Predicate::falsy("gol_reader_missing_test_key"))
                        .and(Predicate::truthy(second)),
                ));
            }
        }
    }
    assert!(!queries.is_empty());
    let expected: Vec<Vec<_>> = queries
        .iter()
        .map(|query| {
            library
                .select(query)
                .map(|f| f.map(|f| f.id()))
                .collect::<Result<_>>()
        })
        .collect::<Result<_>>()?;
    for width in [1, 2, 3, 7, 8, 15, 16, 17, 31, 32, 33, 257] {
        let library = Library::new(Segments { bytes, width })?;
        for (query, expected) in queries.iter().zip(&expected) {
            let actual = query
                .prepare(&library)?
                .iter()
                .map(|f| f.map(|f| f.id()))
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(&actual, expected, "segment width={width}, query={query:?}");
        }
    }
    Ok(())
}
