#![cfg(feature = "gob")]

use regol::{
    Bounds, Error, Feature, FeatureId, FeatureType, Geometry, Library, Query, Result,
    gob::{GobCatalog, GobStore},
    source::Source,
};

const TILED: &[u8] = include_bytes!("fixtures/tiled_global.gob");
const GEOMETRY: &[u8] = include_bytes!("fixtures/geometry.gob");
const STORE_BYTES: usize = 1024 * 1024;

fn open(bytes: &[u8]) -> Result<GobCatalog> {
    let prefix_len = GobCatalog::prefix_len(&bytes[..64])?;
    GobCatalog::open(&bytes[..prefix_len])
}

fn chunk<'a>(bytes: &'a [u8], store: &GobStore, tip: u32) -> &'a [u8] {
    let entry = store.catalog().tile(tip).expect("test includes the tile");
    let start = entry.offset as usize;
    &bytes[start..start + entry.len as usize]
}

fn import(bytes: &[u8], store: &mut GobStore, tip: u32) -> Result<()> {
    store.import_tile(tip, chunk(bytes, store, tip))
}

#[test]
fn catalogs_validate_and_keep_coverage_for_tiles_absent_from_an_archive() -> Result<()> {
    let catalog = open(TILED)?;
    assert_eq!(catalog.archive_len(), TILED.len() as u64);
    assert_eq!(catalog.tile_count(), 13);
    let all_tips = catalog.tiles_in_bounds(Bounds::WORLD);
    let viewport = Bounds::new(0, 0, 40_000, 40_000)?;
    let local_tips = catalog.tiles_in_bounds(viewport);
    assert_eq!(all_tips.len(), catalog.tile_count());
    assert!(local_tips.len() < all_tips.len());
    assert_eq!(local_tips[0], 1);
    let mut entries = all_tips
        .iter()
        .map(|&tip| catalog.tile(tip).unwrap())
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.offset);
    let mut offset = GobCatalog::prefix_len(TILED)? as u64;
    for entry in entries {
        assert_eq!(entry.offset, offset);
        offset += u64::from(entry.len);
    }
    assert_eq!(offset, catalog.archive_len());

    let metadata_start = 68 + 8 * catalog.tile_count();
    let metadata_end = GobCatalog::prefix_len(TILED)?;
    let mut empty = TILED[..64].to_vec();
    empty[28..32].fill(0);
    let crc = crc32c::crc32c(&empty);
    empty.extend_from_slice(&crc.to_le_bytes());
    empty.extend_from_slice(&TILED[metadata_start..metadata_end]);
    let partial = open(&empty)?;
    assert_eq!(partial.tile_count(), 0);
    assert_eq!(partial.archive_len(), empty.len() as u64);
    assert_eq!(partial.tiles_in_bounds(viewport), local_tips);
    assert_eq!(partial.tile(1), None);

    for damaged in [TILED[..63].to_vec(), TILED[..metadata_end - 1].to_vec()] {
        assert!(matches!(
            GobCatalog::open(&damaged),
            Err(Error::InvalidGob { .. })
        ));
    }
    for bad_byte in [0, 4, 64, metadata_start + 4] {
        let mut damaged = TILED[..metadata_end].to_vec();
        damaged[bad_byte] ^= 1;
        assert!(matches!(
            GobCatalog::open(&damaged),
            Err(Error::InvalidGob { .. })
        ));
    }
    Ok(())
}

#[test]
fn imports_publish_individually_and_missing_links_keep_their_tile_id() -> Result<()> {
    let catalog = open(TILED)?;
    let viewport = Bounds::new(0, 0, 40_000, 40_000)?;
    let local_tips = catalog.tiles_in_bounds(viewport);
    let mut store = GobStore::new(catalog, STORE_BYTES)?;
    assert!(matches!(
        store.library().in_bounds(viewport).next().unwrap(),
        Err(Error::MissingTile(1))
    ));
    assert_eq!(store.take_missing_tile(), Some(1));
    assert_eq!(store.take_missing_tile(), None);
    let initial = store.retained_bytes();
    let mut limited = GobStore::new(open(TILED)?, initial)?;
    assert!(matches!(
        import(TILED, &mut limited, 1),
        Err(Error::Limit(_))
    ));
    assert!(!limited.is_loaded(1));
    assert_eq!(limited.retained_bytes(), initial);

    for tip in local_tips {
        let previous = store.retained_bytes();
        let mut damaged = chunk(TILED, &store, tip).to_vec();
        damaged[4] ^= 1;
        assert!(matches!(
            store.import_tile(tip, &damaged),
            Err(Error::InvalidGob { .. })
        ));
        assert_eq!(store.retained_bytes(), previous);
        assert!(!store.is_loaded(tip));
        import(TILED, &mut store, tip)?;
        assert!(store.is_loaded(tip));
        let loaded = store.retained_bytes();
        import(TILED, &mut store, tip)?;
        assert_eq!(store.retained_bytes(), loaded);
        assert!(store.is_loaded(1));
    }

    let query = Query::parse("n[amenity=cafe]")?;
    assert_eq!(
        query.prepare(store.library())?.count_in_bounds(viewport)?,
        1
    );

    let query = Query::parse("r[route=bus]")?;
    let member_ids = |store: &GobStore| -> Result<Vec<_>> {
        let prepared = query.prepare(store.library())?;
        let relation = prepared.in_bounds(viewport).next().expect("bus route")?;
        relation
            .members()?
            .map(|m| m.map(|member| member.feature.id().id))
            .collect()
    };
    let Err(Error::MissingTile(tip)) = member_ids(&store) else {
        panic!("cross-viewport relation should require another tile");
    };
    assert_eq!(store.take_missing_tile(), Some(tip));
    assert!(!store.is_loaded(tip));
    import(TILED, &mut store, tip)?;
    assert_eq!(member_ids(&store)?, [5, 10, 1, 5]);

    let mut bytes = Vec::new();
    store.write_gol(&mut bytes)?;
    let persisted = Library::new(bytes)?;
    assert_eq!(persisted.tile_count(), store.library().tile_count());
    assert!(persisted.tile_count() < store.catalog().tile_count() as u32);
    let query = Query::parse("n[amenity=cafe]")?;
    assert_eq!(query.prepare(&persisted)?.count_in_bounds(viewport)?, 1);
    Ok(())
}

#[derive(Debug, PartialEq)]
struct Snapshot {
    id: FeatureId,
    bounds: Bounds,
    tags: Vec<(String, String)>,
    geometry: Geometry,
    members: Vec<(FeatureId, String)>,
}

fn snapshot<S: Source>(feature: Feature<'_, S>) -> Result<Snapshot> {
    let mut tags = feature
        .tags()?
        .map(|tag| tag.map(|tag| (tag.key.into_owned(), tag.value.to_string())))
        .collect::<Result<Vec<_>>>()?;
    tags.sort();
    let members = if feature.kind() == FeatureType::Relation {
        feature
            .members()?
            .map(|member| member.map(|m| (m.feature.id(), m.role.into_owned())))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Ok(Snapshot {
        id: feature.id(),
        bounds: feature.bounds()?,
        tags,
        geometry: feature.geometry()?,
        members,
    })
}

#[test]
fn complete_store_matches_gol_queries_and_passes_integrity_check() -> Result<()> {
    for (gob, gol) in [
        (
            TILED,
            include_bytes!("fixtures/tiled_global.gol").as_slice(),
        ),
        (GEOMETRY, include_bytes!("fixtures/geometry.gol").as_slice()),
    ] {
        let original = Library::new(gol)?;
        let mut store = GobStore::new(open(gob)?, STORE_BYTES)?;
        assert_eq!(original.revision()?, u64::from(store.catalog().revision()));
        assert_eq!(&store.catalog().tileset_id(), &gol[64..80]);
        assert!(store.library().has_way_node_ids());
        for tip in store
            .catalog()
            .tiles_in_bounds(Bounds::WORLD)
            .into_iter()
            .rev()
        {
            import(gob, &mut store, tip)?;
        }
        let library = store.library();
        assert_eq!(library.check_integrity()?, original.check_integrity()?);
        let mut serialized = Vec::new();
        store.write_gol(&mut serialized)?;
        assert_eq!(library.source().len(), serialized.len() as u64);
        let mut read = vec![0; serialized.len()];
        library.source().read_exact_at(0, &mut read)?;
        assert_eq!(read, serialized);
        let persisted = Library::new(serialized)?;
        assert_eq!(persisted.check_integrity()?, library.check_integrity()?);
        assert_eq!(
            library.properties()?.collect::<Result<Vec<_>>>()?,
            original.properties()?.collect::<Result<Vec<_>>>()?
        );
        for query in ["*", "a", "w[highway]", "n[amenity=cafe]", "*[name]", "r"] {
            let query = Query::parse(query)?;
            let mut expected = original
                .select(&query)
                .map(|f| snapshot(f?))
                .collect::<Result<Vec<_>>>()?;
            let mut actual = library
                .select(&query)
                .map(|f| snapshot(f?))
                .collect::<Result<Vec<_>>>()?;
            expected.sort_by_key(|f| f.id);
            actual.sort_by_key(|f| f.id);
            assert_eq!(actual, expected);
        }
    }
    Ok(())
}
