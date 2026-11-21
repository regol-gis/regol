#![cfg(feature = "way-checkpoints")]

use regol::{Error, FeatureType, Library, Result, WayCheckpointIndex};
use sha2::{Digest, Sha256};

const WITH_IDS: &[u8] = include_bytes!("fixtures/coincident_ids.gol");
const WITHOUT_IDS: &[u8] = include_bytes!("fixtures/coincident.gol");

#[test]
fn round_trip_preserves_coordinates_ids_and_area_closure() -> Result<()> {
    let library = Library::new(WITH_IDS)?;
    for (stride, minimum) in [(1, 1), (2, 2), (3, 3), (32, 1), (8, 1024)] {
        let index = WayCheckpointIndex::build(&library, stride, minimum, 4096)?;
        let bytes = index.to_bytes();
        let reopened = WayCheckpointIndex::from_bytes(&library, &bytes)?;
        assert_eq!(reopened.to_bytes(), bytes);
        let mut area_ways = 0;
        for feature in library.features() {
            let feature = feature?;
            if feature.kind() != FeatureType::Way {
                assert!(matches!(
                    reopened.vertex(feature, 0),
                    Err(Error::WrongFeatureType)
                ));
                continue;
            }
            let sequence = feature.way_nodes()?.collect::<Result<Vec<_>>>()?;
            assert_eq!(
                reopened.way_nodes(feature)?.collect::<Result<Vec<_>>>()?,
                sequence
            );
            let ids = sequence
                .iter()
                .map(|node| node.id.unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                reopened
                    .original_node_ids(feature)?
                    .collect::<Result<Vec<_>>>()?,
                ids
            );
            assert_eq!(
                feature
                    .original_way_node_ids()?
                    .collect::<Result<Vec<_>>>()?,
                ids
            );
            for (position, vertex) in sequence.into_iter().enumerate() {
                assert_eq!(reopened.vertex(feature, position)?, Some(vertex));
            }
            assert_eq!(reopened.vertex(feature, usize::MAX)?, None);
            area_ways += usize::from(feature.is_area());
        }
        // Separate fixture exercises an area (including a repeated closing ID).
        let area_library = Library::new(&include_bytes!("fixtures/builder.gol")[..])?;
        let area_index = WayCheckpointIndex::build(&area_library, stride, minimum, 4096)?;
        for area in area_library.features() {
            let area = area?;
            if area.kind() == FeatureType::Way && area.is_area() {
                let vertices = area.way_nodes()?.collect::<Result<Vec<_>>>()?;
                assert_eq!(vertices.first(), vertices.last());
                assert_eq!(
                    area_index.vertex(area, vertices.len() - 1)?,
                    vertices.first().copied()
                );
                assert_eq!(area_index.vertex(area, vertices.len())?, None);
                area_ways += 1;
            }
        }
        assert!(area_ways > 0);
    }
    Ok(())
}

#[test]
fn rejects_corruption_options_and_mismatched_sources() -> Result<()> {
    let library = Library::new(WITH_IDS)?;
    assert!(WayCheckpointIndex::build(&library, 0, 1, 4096).is_err());
    assert!(WayCheckpointIndex::build(&library, 1, 0, 4096).is_err());
    assert!(matches!(
        WayCheckpointIndex::build(&library, 1, 1, 96),
        Err(Error::Limit("way checkpoints"))
    ));
    let other = Library::new(WITHOUT_IDS)?;
    assert!(WayCheckpointIndex::build(&other, 1, 1, 4096).is_err());

    let index = WayCheckpointIndex::build(&library, 1, 1, 4096)?;
    let bytes = index.to_bytes();
    assert!(WayCheckpointIndex::from_bytes(&library, &bytes[..bytes.len() - 1]).is_err());

    let mut invalid_version = bytes.clone();
    invalid_version[7] ^= 1;
    assert!(WayCheckpointIndex::from_bytes(&library, &invalid_version).is_err());

    let mut corrupt = bytes.clone();
    corrupt[64] ^= 1;
    assert!(WayCheckpointIndex::from_bytes(&library, &corrupt).is_err());

    // A valid checksum does not make an overflowing directory entry valid.
    let mut invalid_entry = bytes.clone();
    invalid_entry[72..80].fill(255);
    let digest_offset = invalid_entry.len() - 32;
    let digest = Sha256::digest(&invalid_entry[..digest_offset]);
    invalid_entry[digest_offset..].copy_from_slice(&digest);
    assert!(WayCheckpointIndex::from_bytes(&library, &invalid_entry).is_err());

    // Change the store ID without changing the file length or layout.
    let mut changed_bytes = WITH_IDS.to_vec();
    changed_bytes[8] ^= 1;
    let changed = Library::new(&changed_bytes[..])?;
    assert!(matches!(
        WayCheckpointIndex::from_bytes(&changed, &bytes),
        Err(Error::WayCheckpoints("sidecar belongs to a different GOL"))
    ));
    let other_instance = Library::new(WITH_IDS)?;
    let foreign_way = other_instance
        .features()
        .find_map(|item| item.ok().filter(|item| item.kind() == FeatureType::Way))
        .expect("fixture way");
    assert!(index.vertex(foreign_way, 0).is_err());
    Ok(())
}
