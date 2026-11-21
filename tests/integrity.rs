use regol::{Error, Library};

const BASIC: &[u8] = include_bytes!("fixtures/basic.gol");
const STRESS: &[u8] = include_bytes!("fixtures/stress.gol");

fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn put_word(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn refresh_metadata_checksums(bytes: &mut [u8]) {
    let end = 4096 + word(bytes, 20) as usize;
    let metadata_checksum = crc32c::crc32c(&bytes[4096..end]);
    put_word(bytes, 112, metadata_checksum);
    refresh_header_checksum(bytes);
}

fn refresh_header_checksum(bytes: &mut [u8]) {
    let header_checksum = crc32c::crc32c(&bytes[..504]);
    put_word(bytes, 504, header_checksum);
}

fn root_branch(bytes: &[u8], tile: usize, index: usize) -> usize {
    let root = pointer(bytes, tile + index, !0);
    pointer(bytes, root, !3)
}

fn pointer(bytes: &[u8], offset: usize, mask: i32) -> usize {
    offset
        .checked_add_signed((word(bytes, offset) as i32 & mask) as isize)
        .unwrap()
}

fn refresh_tile_checksum(bytes: &mut [u8], tile: usize) {
    let crc_offset = tile + (word(bytes, tile) & 0x3fff_ffff) as usize;
    let crc = crc32c::crc32c(&bytes[tile..crc_offset]);
    put_word(bytes, crc_offset, crc);
}

#[test]
fn verifies_both_single_and_multi_tile_libraries_and_checks_fields() -> regol::Result<()> {
    for data in [
        BASIC,
        include_bytes!("fixtures/tiled.gol").as_slice(),
        STRESS,
    ] {
        let library = Library::new(data)?;
        let result = library.check_integrity()?;
        assert_eq!(result.tiles, library.tile_count());
        assert_eq!(
            result.features,
            library.features().collect::<regol::Result<Vec<_>>>()?.len() as u64
        );
        assert!(
            library
                .indexed_keys()
                .collect::<regol::Result<Vec<_>>>()?
                .len()
                > 1
        );
    }
    Ok(())
}

#[test]
fn parent_must_contain_each_child_branch() -> regol::Result<()> {
    // Page 20 of the stress fixture has a multi-level node tree.
    let tile = 20 * 4096;
    let branch = root_branch(STRESS, tile, 8);
    assert_eq!(word(STRESS, branch) & 2, 0);
    let child = pointer(STRESS, branch, !3);
    let mut bytes = STRESS.to_vec();
    let child_min_x = word(&bytes, child + 4) as i32;
    put_word(&mut bytes, branch + 4, (child_min_x + 1) as u32);
    refresh_tile_checksum(&mut bytes, tile);
    let library = Library::new(bytes)?;
    assert!(
        library
            .features()
            .collect::<regol::Result<Vec<_>>>()
            .is_ok()
    );
    let result = library.check_integrity();
    assert!(
        matches!(result, Err(Error::Invalid { offset, message: "R-tree branch bounds exclude child branch" }) if offset == child as u64),
        "{result:?}"
    );
    Ok(())
}

#[test]
fn parent_must_contain_feature_bounds_including_duplicate_leaves() -> regol::Result<()> {
    let directory = word(BASIC, 152) as usize * 4096;
    let basic_tile = (word(BASIC, directory + 4) >> 2) as usize * 4096;
    // The stress fixture's east tile on page 13 has ways also stored west of it.
    for (data, tile, index, node) in [(BASIC, basic_tile, 8, true), (STRESS, 13 * 4096, 12, false)]
    {
        let branch = root_branch(data, tile, index);
        assert_ne!(word(data, branch) & 2, 0);
        let leaf = pointer(data, branch, !3);
        if !node {
            assert_ne!(word(data, leaf + 16) & 64, 0);
            assert!((word(data, leaf) as i32) < 0);
            assert_eq!(word(data, leaf + 8), 0);
        }
        let mut bytes = data.to_vec();
        let parent_max_y = word(&bytes, branch + 16) as i32;
        put_word(
            &mut bytes,
            leaf + if node { 4 } else { 12 },
            (parent_max_y + 1) as u32,
        );
        refresh_tile_checksum(&mut bytes, tile);
        let library = Library::new(bytes)?;
        assert_eq!(
            library.features().count(),
            Library::new(data)?.features().count()
        );
        let result = library.check_integrity();
        assert!(
            matches!(result, Err(Error::Invalid { offset, message: "R-tree branch bounds exclude feature" }) if offset == leaf as u64),
            "{result:?}"
        );
    }
    Ok(())
}

#[test]
fn detects_header_metadata_catalog_and_tile_checksum_corruption() {
    let directory = word(BASIC, 152) as usize * 4096;
    let tile = ((word(BASIC, directory + 4) >> 2) * 4096) as usize;
    let tile_crc = tile + word(BASIC, tile) as usize;
    for (where_to_flip, error_offset, refresh_header) in [
        (65, 504, false),
        (112, 112, true),
        (116, 116, true),
        (tile_crc, tile_crc, false),
    ] {
        let mut bytes = BASIC.to_vec();
        bytes[where_to_flip] ^= 1;
        if refresh_header {
            refresh_header_checksum(&mut bytes);
        }
        let result = Library::new(bytes).unwrap().check_integrity();
        assert!(
            matches!(result, Err(Error::Invalid { offset, .. }) if offset == error_offset as u64),
            "{result:?}"
        );
    }
}

#[test]
fn zeroed_checksums_are_errors() {
    let directory = word(BASIC, 152) as usize * 4096;
    let tile = (word(BASIC, directory + 4) >> 2) as usize * 4096;
    let tile_crc = tile + (word(BASIC, tile) & 0x3fff_ffff) as usize;
    for offset in [504, 112, 116, tile_crc] {
        let mut bytes = BASIC.to_vec();
        put_word(&mut bytes, offset, 0);
        if offset == 112 || offset == 116 {
            refresh_header_checksum(&mut bytes);
        }
        let result = Library::new(bytes).unwrap().check_integrity();
        assert!(
            matches!(result, Err(Error::Invalid { offset: actual, .. }) if actual == offset as u64),
            "{result:?}"
        );
    }
}

#[test]
fn missing_tiles_and_duplicate_page_addresses_are_errors() {
    let mut bytes = include_bytes!("fixtures/tiled.gol").to_vec();
    let directory = word(&bytes, 152) as usize * 4096;
    bytes[directory + 4..directory + 8].fill(0);
    assert!(matches!(
        Library::new(bytes).unwrap().check_integrity(),
        Err(Error::MissingTile(_))
    ));
    let mut bytes = include_bytes!("fixtures/tiled.gol").to_vec();
    let root = word(&bytes, directory + 4);
    let first_link = directory + 12;
    let first_page_entry = first_link + (word(&bytes, first_link) & !3) as usize;
    bytes[first_page_entry..first_page_entry + 4].copy_from_slice(&root.to_le_bytes());
    assert!(matches!(
        Library::new(bytes).unwrap().check_integrity(),
        Err(Error::Invalid {
            message: "invalid or overlapping tile extent",
            ..
        })
    ));
}

#[test]
fn rejects_metadata_tables_overlap_or_spill_past_the_checksum() {
    let string_table = word(BASIC, 84) as usize;
    let metadata_size = word(BASIC, 20);
    let metadata_end = 4096 + metadata_size as usize;
    let mut cases = [
        BASIC.to_vec(),
        BASIC.to_vec(),
        BASIC.to_vec(),
        BASIC.to_vec(),
    ];

    // The first nonempty global string is two bytes. Replace those characters
    // with two NULs so the same bytes also decode as an empty properties table.
    let overlapping_properties = string_table + 4;
    assert_eq!(cases[0][string_table + 3], 2);
    cases[0][overlapping_properties..overlapping_properties + 2].fill(0);
    put_word(&mut cases[0], 92, overlapping_properties as u32);

    // The final property starts within the metadata and ends in its padding.
    put_word(&mut cases[1], 20, metadata_size - 1);

    // An optional absent properties table leaves the dictionary last.
    put_word(&mut cases[2], 92, 0);
    put_word(&mut cases[2], 20, word(BASIC, 92) - 4096 - 1);

    // The first byte of a zero schema count sits inside the declared range.
    cases[3][metadata_end - 1] = 0;
    put_word(&mut cases[3], 88, (metadata_end - 1) as u32);
    put_word(&mut cases[3], 92, 0);

    for mut bytes in cases {
        refresh_metadata_checksums(&mut bytes);
        let result = Library::new(bytes).unwrap().check_integrity();
        assert!(
            matches!(
                result,
                Err(Error::Invalid {
                    message: "metadata table exceeds its allocated range",
                    ..
                })
            ),
            "{result:?}"
        );
    }
}

#[test]
fn alternate_active_snapshot_checks_only_its_tiles() -> regol::Result<()> {
    let directory = word(BASIC, 152) as usize * 4096;
    let directory_end = directory + word(BASIC, directory) as usize + 4;
    let tile = (word(BASIC, directory + 4) >> 2) as usize * 4096;
    let tile_crc = tile + (word(BASIC, tile) & 0x3fff_ffff) as usize;
    let new_tile = BASIC.len().next_multiple_of(4096);
    let new_directory = (new_tile + tile_crc + 4 - tile).next_multiple_of(4096);
    let mut bytes = BASIC.to_vec();
    bytes.resize(new_tile, 0);
    bytes.extend_from_slice(&BASIC[tile..tile_crc + 4]);
    bytes.resize(new_directory, 0);
    bytes.extend_from_slice(&BASIC[directory..directory_end]);
    put_word(
        &mut bytes,
        new_directory + 4,
        (new_tile / 4096) as u32 * 4 + 2,
    );

    let pages = bytes.len().div_ceil(4096);
    put_word(&mut bytes, 24, pages as u32);
    put_word(&mut bytes, 192, u32::MAX);
    put_word(&mut bytes, 196, 42); // modified-since revision
    put_word(&mut bytes, 216, (new_directory / 4096) as u32);
    put_word(&mut bytes, 220, 1);
    bytes[tile_crc] ^= 1;
    bytes[16] = 1;
    refresh_header_checksum(&mut bytes);
    let library = Library::new(bytes.as_slice())?;
    assert_eq!(library.revision()?, u64::from(u32::MAX));
    assert_eq!(library.check_integrity()?.features, 8);

    bytes[16] = 0;
    refresh_header_checksum(&mut bytes);
    let result = Library::new(bytes)?.check_integrity();
    assert!(
        matches!(result, Err(Error::Invalid { offset, message: "tile checksum mismatch" }) if offset == tile_crc as u64)
    );
    Ok(())
}
