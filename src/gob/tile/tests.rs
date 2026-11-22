use super::*;
use crate::{FeatureType, Library, Query, decode::Decode};

const FULL_TILE: &[u8] = &[
    // Nodes 10 and 11, way 20, relation 30.
    4, 21, 3, 0, 41, 0, 61, // Local key, value, and member role.
    3, 8, b'z', b'o', b'n', b'e', b'-', b'k', b'e', b'y', 6, b's', b'a', b'm', b'p', b'l', b'e', 9,
    b'e', b'd', b'g', b'e', b'-', b'r', b'o', b'l', b'e',
    // Shared empty tags and a shared backlink to relation 30.
    1, 4, 1, 0, 1, 4, 0, // Node 10 at (1, 1), then node 11 at (2, 1).
    0x1f, 0, 1, 2, 2, 0x5f, 0, 1, 2, 0,
    // Way tags are zone-key=sample and highway=yes. Coordinates start at
    // (1, 1). Both original node IDs and feature-node links are retained.
    0x3d, 13, 4, 3, 1, 25, 2, 1, 2, 1, 0, 2, 0, 20, 2, 8, 0, 2,
    // Relation bbox, then nodes with retained local role and way with outer.
    0x3b, 0, 0, 0, 1, 0, 18, 2, 4, 4, 10, 7,
    // No removals. Three exports: node 10, a hole, and relation 30.
    0, 15, 1, 0, 4,
];

fn convert_test(input: &[u8], max: usize) -> Result<Vec<u8>> {
    convert(input, Bounds::new(0, 0, 10, 10)?, true, &[(6, 2)], 33, max)
}

fn library(tile: &[u8]) -> Result<Library<Vec<u8>>> {
    // Use an existing dictionary with highway=6, yes=2 and outer=3.
    let mut gol = include_bytes!("../../../tests/fixtures/basic.gol").to_vec();
    let start = gol.len().next_multiple_of(4096);
    let entry = 4096 * usize::try_from(gol.u32(152)?).unwrap() + 4;
    gol[entry..entry + 4].copy_from_slice(&((start as u32 >> 12 << 2) | 2).to_le_bytes());
    gol[80..84].copy_from_slice(&1u32.to_le_bytes());
    gol.resize(start, 0);
    gol.extend_from_slice(tile);
    Library::new(gol)
}

#[test]
fn empty_tile_has_length_and_checksum() -> Result<()> {
    let tile = convert(&[0; 6], Bounds::WORLD, false, &[], 1, 32)?;
    assert_eq!(tile.len(), 32);
    assert_eq!(tile[..4], 28u32.to_le_bytes());
    assert_eq!(tile[28..], crc32c::crc32c(&tile[..28]).to_le_bytes());
    Ok(())
}

#[test]
fn materializes_tags_geometry_shared_links_and_export_holes() -> Result<()> {
    let tile = convert_test(FULL_TILE, usize::MAX)?;
    let exports = tile.relative(24, !0)?;
    assert_eq!(tile.u32(exports - 4)?, 3);
    assert_eq!(tile.u32(exports + 4)?, 0);
    assert_eq!(tile.u32(tile.relative(exports, !0)?)? >> 12, 10);
    assert_eq!(tile.u32(tile.relative(exports + 8, !0)?)? >> 12, 30);

    let lib = library(&tile)?;
    let query: Query = "w[highway=yes][zone-key=sample]".parse()?;
    let items = lib.select(&query).collect::<Result<Vec<_>>>()?;
    assert_eq!(items.len(), 1);
    let way = items[0];
    assert_eq!(way.id().id, 20);
    assert_eq!(way.bounds()?, Bounds::new(1, 1, 2, 1)?);
    assert_eq!(
        way.way_nodes()?
            .map(|n| n.map(|n| (n.point, n.id)))
            .collect::<Result<Vec<_>>>()?,
        [
            (Point { x: 1, y: 1 }, Some(10)),
            (Point { x: 2, y: 1 }, Some(11))
        ],
    );
    assert_eq!(
        way.feature_nodes()?
            .map(|f| f.map(|f| f.id().id))
            .collect::<Result<Vec<_>>>()?,
        [10, 11],
    );
    let relations = way.parent_relations()?.collect::<Result<Vec<_>>>()?;
    assert_eq!(relations.len(), 1);
    assert_eq!(relations[0].id().id, 30);
    let members = relations[0]
        .members()?
        .map(|m| m.map(|m| (m.feature.kind(), m.feature.id().id, m.role.into_owned())))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        members,
        [
            (FeatureType::Node, 10, "edge-role".to_owned()),
            (FeatureType::Node, 11, "edge-role".to_owned()),
            (FeatureType::Way, 20, "outer".to_owned()),
        ],
    );
    Ok(())
}

#[test]
fn rejects_partial_streams_invalid_exports_and_output_budget_overflow() -> Result<()> {
    for end in 0..FULL_TILE.len() {
        assert!(
            matches!(
                convert_test(&FULL_TILE[..end], usize::MAX),
                Err(Error::InvalidGob { .. })
            ),
            "prefix {end}"
        );
    }
    let mut bad_export = FULL_TILE.to_vec();
    *bad_export.last_mut().unwrap() = 5;
    assert!(matches!(
        convert_test(&bad_export, usize::MAX),
        Err(Error::InvalidGob {
            message: "export references unknown feature",
            ..
        })
    ));

    let size = convert_test(FULL_TILE, usize::MAX)?.len();
    let exact = convert_test(FULL_TILE, size)?;
    assert_eq!(exact.len(), size);
    assert!(exact.capacity() <= size);
    assert!(matches!(
        convert_test(FULL_TILE, size - 1),
        Err(Error::Limit("converted GOB tile bytes"))
    ));
    Ok(())
}

#[test]
fn reconstructs_narrow_and_wide_backward_links() -> Result<()> {
    let mut payload = vec![
        2, 21, 0, 41, // Feature index, one node and one way.
        0, 1, 4, 1, 0, 0, // Empty strings, one empty tag table, no shared parents.
        0x1b, 0, 0, 0, // Node 10 at (0, 0).
        0x1b, 0, 4, 0, 0, 2, 0, 2, 0, 2, 0,  // Way 20 has four coordinates.
        18, // GOL node table: 4 + 4 + 8 + 2 bytes.
    ];
    // Foreign TEX 0 and TIP 1, then local node 10, then wide deltas for
    // TEX +5000 and TIP +20480, then TEX -1 in the same foreign tile.
    payload.extend([
        0xff, 0x7f, 0xfd, 0xff, 0x01, 0, 0xc3, 0xb8, 0x02, 0x80, 0xc0, 0x02, 5,
    ]);
    payload.extend([0, 2, 0]); // No removals, legacy export slot 0 references node 10.
    let tile = convert(&payload, Bounds::new(0, 0, 10, 10)?, false, &[], 1, 4096)?;
    let root = tile.relative(12, !0)?;
    let branch = tile.relative(root, !1)?;
    let way = tile.relative(branch, !3)? + 16;
    let body = tile.relative(way + 12, !0)?;
    assert_eq!(tile.u16(body - 2)?, 0x8006); // Narrow export -2048.
    assert_eq!(tile.u16(body - 4)?, 0x8002); // Narrow tile -16383.
    assert_eq!(tile.u16(body - 10)?, 0x388e); // Low word of wide export +5000.
    assert_eq!(tile.u16(body - 12)?, 1);
    assert_eq!(tile.u16(body - 14)?, 0xa001); // Low word of wide tile +20480.
    assert_eq!(tile.u16(body - 16)?, 0);
    assert_eq!(tile.u16(body - 18)?, 0xfff3); // Narrow export -1 with last flag.
    let lib = library(&tile)?;
    let ways: Query = "w".parse()?;
    let way = lib.select(&ways).next().unwrap()?;
    let linked = way
        .feature_nodes()?
        .take(2)
        .map(|r| r.map(|f| f.id().id))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(linked, [10, 10]);
    Ok(())
}
