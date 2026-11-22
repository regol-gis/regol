use super::*;

#[test]
fn borrowed_global_records_match_checked_reads() -> Result<()> {
    let fixture = include_bytes!("../../tests/fixtures/global.gol");
    let start = fixture.len().next_multiple_of(4096);
    let table = start + 32;
    let text = table + 12;
    let global = (1u16 << 2) | 0x8000;
    for (flags, payload, string, end) in [
        (0, 250i32, &[][..], 4),
        (1, 1, &[][..], 4),
        (1, 65535, &[][..], 4),
        (2, 1291, &[][..], 6),
        (2, 1291, &[][..], 5),
        (3, 10, &[3, b'X', 0xc3, 0xa9], 16),
        (3, 10, &[1, 255], 14),
        (3, 10, &[0x81, 0x00, b'x'], 15),
        (3, 10, &[2, b'x', b'y'], 14),
        (3, -64, &[][..], 6),
    ] {
        let mut bytes = fixture.to_vec();
        bytes.resize(text + string.len(), 0);
        bytes[table..table + 2].copy_from_slice(&(global | flags).to_le_bytes());
        bytes[table + 2..table + 6].copy_from_slice(&payload.to_le_bytes());
        bytes[text..].copy_from_slice(string);
        let library = Library::new(bytes)?;
        let feature = Feature {
            library: &library,
            header: 0,
            tile: TileExtent::new(start as u64, (table + end) as u64)?,
            anchor_offset: 24,
            tag_pointer: 0,
        };
        let mut tags = feature.tags()?;
        let expected = feature
            .tags()?
            .read_fallback(table as u64)
            .map_err(|e| e.to_string());
        assert_eq!(tags.next().unwrap().map_err(|e| e.to_string()), expected);
        assert!(tags.next().is_none());
    }
    Ok(())
}

#[test]
fn fast_tag_lookup_matches_tag_iteration_for_global_local_and_missing_keys() -> Result<()> {
    for fixture in [
        include_bytes!("../../tests/fixtures/basic.gol").as_slice(),
        include_bytes!("../../tests/fixtures/global.gol").as_slice(),
        include_bytes!("../../tests/fixtures/query_semantics.gol").as_slice(),
    ] {
        let library = Library::new(fixture)?;
        for feature in library.features() {
            let feature = feature?;
            let all: Vec<_> = feature.tags()?.collect::<Result<Vec<_>>>()?;
            for tag in &all {
                let first = all
                    .iter()
                    .find(|t| t.key == tag.key)
                    .map(|t| t.value.clone());
                assert_eq!(feature.tag(&tag.key)?, first);
            }
            assert_eq!(feature.tag("definitely_absent_key")?, None);
        }
    }
    Ok(())
}
