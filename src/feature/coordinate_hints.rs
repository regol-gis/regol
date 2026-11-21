use super::*;

#[test]
fn hints_bound_success_error_closure_and_overflow() -> Result<()> {
    let library = Library::new(include_bytes!("../../tests/fixtures/geometry.gol").as_slice())?;
    let mut ways = 0;
    for feature in library.features() {
        let feature = feature?;
        if feature.kind() != FeatureType::Way {
            continue;
        }
        ways += 1;
        let expected = feature.coordinates()?.collect::<Result<Vec<_>>>()?;
        let mut iter = feature.coordinates()?;
        for (index, point) in expected.iter().enumerate() {
            let (lower, upper) = iter.size_hint();
            assert!(lower <= expected.len() - index);
            assert!(upper.is_none_or(|n| n >= expected.len() - index));
            assert_eq!(iter.next().transpose()?, Some(*point));
        }
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert!(iter.next().is_none());
        assert_eq!(iter.size_hint(), (0, Some(0)));

        let mut iter = feature.coordinates()?;
        iter.remaining = 1;
        iter.p = library.source.len() as u64;
        assert_eq!(iter.size_hint().0, 1);
        assert!(iter.next().unwrap().is_err());
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert!(iter.next().is_none());

        let mut iter = feature.coordinates()?;
        iter.remaining = 0;
        iter.close = true;
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert!(iter.next().is_none());

        let mut iter = feature.coordinates()?;
        iter.remaining = u64::MAX;
        iter.close = true;
        assert_eq!(iter.size_hint(), (1, None));
    }
    assert!(ways > 0);
    Ok(())
}

#[test]
fn short_coordinate_pairs_match_the_general_decoder() -> Result<()> {
    let pairs = [
        [1, 2, 0, 0, 0, 0],
        [0x80, 1, 0xff, 0xff, 0x7f, 0],
        [0x81, 0x80, 0, 0x82, 0x80, 0],
        [0xff, 0xff, 0x7f, 0xfe, 0xff, 0x7f],
    ];
    for bytes in pairs {
        let mut p = 0;
        let source = bytes.as_slice();
        let x = source.signed_varint(&mut p)? as i32;
        let y = source.signed_varint(&mut p)? as i32;
        assert_eq!(short_coordinate_pair(&bytes), Some((x, y, p)));
    }
    for bytes in [
        [0x80, 0x80, 0x80, 0x00, 0x01, 0x00],
        [0x01, 0x80, 0x80, 0x80, 0x00, 0x00],
        [0x80; 6],
    ] {
        assert_eq!(short_coordinate_pair(&bytes), None);
    }
    Ok(())
}

#[test]
fn owned_collection_matches_the_iterator_across_windows() -> Result<()> {
    struct Segmented<'a> {
        bytes: &'a [u8],
        width: usize,
    }

    impl Source for Segmented<'_> {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<()> {
            self.bytes.read_exact_at(offset, output)
        }

        fn segment(&self, offset: u64) -> std::io::Result<Option<(u64, &[u8])>> {
            if offset >= self.len() {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            if self.width == 0 {
                return Ok(None);
            }
            let start = offset as usize / self.width * self.width;
            let end = (start + self.width).min(self.bytes.len());
            Ok(Some((start as u64, &self.bytes[start..end])))
        }
    }

    fn check<S: Source>(source: S) -> Result<()> {
        let library = Library::new(source)?;
        let mut ways = 0;
        for feature in library.features() {
            let feature = feature?;
            if feature.kind() == FeatureType::Way {
                let expected = feature.coordinates()?.collect::<Result<Vec<_>>>()?;
                assert_eq!(feature.coordinates()?.collect_owned(0)?, expected);
                ways += 1;
            }
        }
        assert!(ways > 0);
        Ok(())
    }

    let bytes = include_bytes!("../../tests/fixtures/geometry.gol").as_slice();
    check(bytes)?;
    for width in [0, 3, 11] {
        check(Segmented { bytes, width })?;
    }
    Ok(())
}
