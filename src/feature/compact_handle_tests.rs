use super::*;

#[test]
fn tile_extents_round_trip_without_truncation() -> Result<()> {
    // Cover the representation's extremes as well as GOL's smaller
    // 30-bit page/payload limits, without allocating a multi-terabyte file.
    for start in [0, 4096, u64::from(u32::MAX) << 12] {
        for length in [0, 28, (1 << 30) + 3, u64::from(u32::MAX)] {
            let tile = TileExtent::new(start, start + length)?;
            assert_eq!(tile.start(), start);
            assert_eq!(tile.end(), start + length);
        }
    }
    for (start, end) in [
        (1, 4097),
        (4096, 4095),
        (0, 1 << 32),
        (1 << 44, (1 << 44) + 28),
        (u64::MAX, u64::MAX),
    ] {
        assert!(TileExtent::new(start, end).is_err());
    }
    assert!(std::mem::size_of::<Feature<'_, &[u8]>>() <= 32);
    Ok(())
}

#[test]
fn feature_extents_remain_checked_at_large_offsets() -> Result<()> {
    let library = Library::new(include_bytes!("../../tests/fixtures/basic.gol").as_slice())?;
    let start = ((1u64 << 30) - 1) << 12;
    let tile = TileExtent::new(start, start + (1 << 30) + 3)?;
    for (kind, before, after) in [(0, 8, 12), (1, 16, 16), (2, 16, 16)] {
        let header = !0xfffu64 | (kind << 3);
        let mut record = [0; 12];
        record[..8].copy_from_slice(&header.to_le_bytes());
        record[8..].copy_from_slice(&i32::MIN.to_le_bytes());
        for anchor in [start + before, tile.end() - after] {
            let feature = Feature::at_record(&library, &record, anchor, tile)?;
            assert_eq!(feature.anchor(), anchor);
            assert_eq!(feature.tile_start(), start);
            assert_eq!(feature.tile_end(), tile.end());
            assert_eq!(feature.id().id, (1u64 << 52) - 1);
            assert_eq!(feature.tag_pointer, i32::MIN);
            assert!(feature.check(start, tile.end() - start).is_ok());
            assert!(feature.check(start - 1, 1).is_err());
            assert!(feature.check(tile.end(), 1).is_err());
            assert!(feature.check(u64::MAX, 1).is_err());
        }
        for anchor in [
            start - 1,
            start + before - 1,
            tile.end() - after + 1,
            start + (1 << 32),
            u64::MAX,
        ] {
            assert!(Feature::at_record(&library, &record, anchor, tile).is_err());
        }
    }
    let mut invalid = [0; 12];
    invalid[0] = 3 << 3;
    assert!(Feature::at_record(&library, &invalid, start + 16, tile).is_err());
    Ok(())
}
