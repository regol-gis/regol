#![forbid(unsafe_code)]
use regol::{
    FeatureType, Library, Query, Result,
    source::{PagedFile, Source},
};

#[test]
fn open_file_backends_read_absolute_offsets_from_an_existing_handle() -> std::io::Result<()> {
    use regol::source::FileSource;
    use std::io::{Seek, SeekFrom};

    let path = "tests/fixtures/basic.gol";
    let bytes = std::fs::read(path)?;
    let check = |source: &dyn Source| -> std::io::Result<()> {
        assert_eq!(source.len(), bytes.len() as u64);
        for offset in [0, 17, bytes.len() - 64] {
            let mut actual = [0; 64];
            source.read_exact_at(offset as u64, &mut actual)?;
            assert_eq!(actual, bytes[offset..offset + 64]);
        }
        assert!(source.read_exact_at(source.len() - 1, &mut [0; 2]).is_err());
        Ok(())
    };
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(123))?;
    check(&FileSource::from_file(file.try_clone()?)?)?;
    check(&PagedFile::from_file(file.try_clone()?)?)?;
    check(&regol::source::WindowedFile::from_file(file.try_clone()?)?)?;
    check(&regol::source::CachedFile::from_file(
        file.try_clone()?,
        16 * 1024,
    )?)?;
    // Unix positioned reads must also leave the shared cursor unchanged.
    #[cfg(unix)]
    assert_eq!(file.stream_position()?, 123);
    Ok(())
}

#[test]
fn cached_pages_cover_boundaries_and_remain_valid_after_file_change() -> std::io::Result<()> {
    use std::io::{Seek, Write};
    let mut file = tempfile::tempfile()?;
    let original: Vec<u8> = (0..600_123).map(|i| (i % 251) as u8).collect();
    file.write_all(&original)?;
    let source = PagedFile::from_file(file.try_clone()?)?;
    let page_bytes = source.page_bytes();
    assert_eq!(source.cached_bytes(), 0);
    assert!(source.retained_tail(0).is_none());
    assert_eq!(source.cached_bytes(), 0);
    let mut first = [0; 23];
    source.read_exact_at(0, &mut first)?;
    assert_eq!(first, original[..23]);
    assert_eq!(source.cached_bytes(), page_bytes);
    assert_eq!(
        source.retained_tail(7).and_then(|tail| tail.get(..16)),
        Some(&original[7..23])
    );
    let page_end = page_bytes as u64;
    assert_eq!(
        source.retained_tail(page_end - 2),
        Some(&original[page_bytes - 2..page_bytes]),
    );
    assert!(source.retained_tail(page_end).is_none());
    assert_eq!(source.cached_bytes(), page_bytes);
    for offset in [0, page_bytes - 4, page_bytes * 2 - 8, 600_100] {
        let mut bytes = [0; 23];
        source.read_exact_at(offset as u64, &mut bytes)?;
        assert_eq!(bytes, original[offset..offset + 23]);
    }
    let mut spanning = vec![0; page_bytes + 37];
    source.read_exact_at((page_bytes - 7) as u64, &mut spanning)?;
    assert_eq!(spanning, original[page_bytes - 7..page_bytes * 2 + 30]);
    assert!(source.view((page_bytes - 4) as u64, 23)?.is_none());
    let cached = source.cached_bytes();
    assert!(source.read_exact_at(source.len() - 1, &mut [0; 2]).is_err());
    assert!(source.view(u64::MAX, 1).is_err());
    assert!(source.view(source.len(), 0)?.unwrap().is_empty());
    assert_eq!(source.cached_bytes(), cached);
    let borrowed = source.view(0, 23)?.unwrap();
    file.set_len(0)?;
    file.rewind()?;
    file.write_all(b"changed")?;
    assert_eq!(borrowed, &original[..23]);
    Ok(())
}

#[test]
fn uncached_pages_fail_after_truncation_without_exposing_partial_reads() -> std::io::Result<()> {
    use std::io::Write;
    let mut file = tempfile::tempfile()?;
    file.write_all(&vec![91; 600_123])?;
    let source = PagedFile::from_file(file.try_clone()?)?;
    let borrowed = source.view(0, 23)?.unwrap();
    let cached = source.cached_bytes();
    file.set_len(1)?;
    assert_eq!(borrowed, &[91; 23]);
    for _ in 0..2 {
        let next_page = source.page_bytes() as u64;
        assert!(source.view(next_page, 23).is_err());
        assert!(source.retained_tail(next_page).is_none());
        assert_eq!(source.cached_bytes(), cached);
    }
    Ok(())
}

#[test]
fn paged_queries_and_fields_agree_with_immutable_bytes() -> Result<()> {
    for name in [
        "basic",
        "stress",
        "wide_exports",
        "query_semantics",
        "geometry",
    ] {
        let path = format!("tests/fixtures/{name}.gol");
        let owned = Library::new(std::fs::read(&path)?)?;
        let paged = Library::new(PagedFile::open(path)?)?;
        compare_queries_and_fields(&owned, &paged)?;
    }
    Ok(())
}

#[test]
fn windowed_queries_and_fields_agree_with_immutable_bytes() -> Result<()> {
    for name in [
        "basic",
        "stress",
        "wide_exports",
        "query_semantics",
        "geometry",
    ] {
        let path = format!("tests/fixtures/{name}.gol");
        let owned = Library::new(std::fs::read(&path)?)?;
        let windowed = Library::new(regol::source::WindowedFile::open(path)?)?;
        compare_queries_and_fields(&owned, &windowed)?;
    }
    Ok(())
}

#[test]
fn bounded_cache_queries_and_fields_agree_after_frequent_eviction() -> Result<()> {
    for name in [
        "basic",
        "stress",
        "wide_exports",
        "query_semantics",
        "geometry",
        "tiled_global",
    ] {
        let path = format!("tests/fixtures/{name}.gol");
        let owned = Library::new(std::fs::read(&path)?)?;
        let cached = Library::new(regol::source::CachedFile::open(path, 16 * 1024)?)?;
        compare_queries_and_fields(&owned, &cached)?;
        assert!(cached.source().allocated_bytes()? <= 16 * 1024);
    }
    Ok(())
}

fn compare_queries_and_fields<S: Source>(
    owned: &Library<Vec<u8>>,
    actual_source: &Library<S>,
) -> Result<()> {
    let mut actual = actual_source.features();
    for a in owned.features() {
        let a = a?;
        let b = actual
            .next()
            .expect("actual_source traversal ended early")?;
        assert_eq!(a.id(), b.id());
        assert_eq!(a.bounds()?, b.bounds()?);
        assert_eq!(
            a.tags()?.collect::<Result<Vec<_>>>()?,
            b.tags()?.collect::<Result<Vec<_>>>()?
        );
        if a.kind() == FeatureType::Way {
            assert_eq!(
                a.coordinates()?.collect::<Result<Vec<_>>>()?,
                b.coordinates()?.collect::<Result<Vec<_>>>()?
            );
            assert_eq!(
                a.feature_nodes()?
                    .map(|f| f.map(|f| f.id()))
                    .collect::<Result<Vec<_>>>()?,
                b.feature_nodes()?
                    .map(|f| f.map(|f| f.id()))
                    .collect::<Result<Vec<_>>>()?
            );
        } else if a.kind() == FeatureType::Relation {
            assert_eq!(
                a.members()?
                    .map(|m| m.map(|m| (m.feature.id(), m.role)))
                    .collect::<Result<Vec<_>>>()?,
                b.members()?
                    .map(|m| m.map(|m| (m.feature.id(), m.role)))
                    .collect::<Result<Vec<_>>>()?
            );
        }
    }
    assert!(
        actual.next().is_none(),
        "actual_source traversal returned extra features"
    );
    for text in [
        "*",
        "*[name]",
        "*[!name]",
        "*[value>=12]",
        "na[amenity=cafe]",
    ] {
        let query = Query::parse(text)?;
        let expected = query
            .prepare(owned)?
            .iter()
            .map(|f| f.map(|f| f.id()))
            .collect::<Result<Vec<_>>>()?;
        let actual = query
            .prepare(actual_source)?
            .iter()
            .map(|f| f.map(|f| f.id()))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(actual, expected);
        assert_eq!(
            query.prepare(actual_source)?.count()?,
            expected.len() as u64
        );
    }
    Ok(())
}

#[test]
fn sparse_file_reads_across_directory_groups_and_a_short_final_page() -> std::io::Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = tempfile::NamedTempFile::new()?;
    file.write_all(include_bytes!("fixtures/basic.gol"))?;
    let boundary = 128 * 32 * 1024;
    let end = 64 * boundary + 1;
    file.seek(SeekFrom::Start(boundary - 1))?;
    file.write_all(&[3, 4])?;
    file.seek(SeekFrom::Start(end - 1))?;
    file.write_all(&[5])?;
    let lib =
        Library::open_with_dictionary_loading(file.path(), regol::DictionaryLoading::OnDemand)
            .unwrap();
    let source = lib.source();
    let initial = source.cached_bytes();
    assert_eq!(initial, source.page_bytes());
    assert!(source.retained_tail(boundary).is_none());
    let mut pair = [0; 2];
    source.read_exact_at(boundary - 1, &mut pair)?;
    assert_eq!(pair, [3, 4]);
    assert_eq!(source.view(end - 1, 1)?, Some(&[5][..]));
    assert_eq!(source.cached_bytes(), initial + 2 * source.page_bytes() + 1);
    assert!(source.read_exact_at(end, &mut pair).is_err());
    Ok(())
}
