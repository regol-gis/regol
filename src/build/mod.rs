//! Compile an OSM PBF snapshot into a new GOL 2.0 library.
mod dictionary;
mod directory;
mod encode;
mod pbf;
mod spill;
mod stage;
mod store;
mod strings;

use anyhow::{Context, Result, ensure};
use std::{fs::File, io::BufReader, path::Path};
use tempfile::NamedTempFile;

/// Objects written and geometry or references omitted during compilation.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildReport {
    /// Features written to the output.
    pub features: u64,
    /// Populated tiles written to the store.
    pub tiles: u32,
    /// Ways with missing vertices or too few vertices for their geometry.
    pub omitted_ways: u64,
    /// Relations omitted because their bounds could not be resolved.
    pub omitted_relations: u64,
    /// Member references omitted because their target was unavailable.
    pub omitted_members: u64,
}

/// Storage options for native GOL output.
#[derive(Debug, Clone, Copy)]
pub struct BuildOptions {
    /// Keep infrequent optional text in tiles to reduce global dictionary startup.
    /// Numeric representations and required index keys stay the same.
    pub compact_dictionary: bool,
    /// Approximate uncompressed feature-byte budget per tile before splitting.
    pub target_tile_bytes: u64,
    /// Maximum number of leaf tiles into which a boundary-crossing feature may
    /// be duplicated using GOL north/west suppression flags. `1` stores every
    /// feature once in its deepest enclosing tile.
    pub max_duplicate_tiles: usize,
    /// Maximum children per internal R-tree branch node.
    pub rtree_branch_fanout: usize,
    /// Maximum feature records per R-tree leaf array.
    pub rtree_leaf_size: usize,
    /// Optional memory budget in bytes before transparently spilling staging
    /// and resolved buffers to temporary files beside the output GOL.
    /// `None` detects available system/cgroup memory automatically on Linux.
    pub memory_limit_bytes: Option<u64>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            compact_dictionary: false,
            target_tile_bytes: directory::TARGET_TILE_BYTES,
            max_duplicate_tiles: 1,
            rtree_branch_fanout: encode::RTREE_BRANCH_FANOUT,
            rtree_leaf_size: encode::RTREE_LEAF_SIZE,
            memory_limit_bytes: None,
        }
    }
}

/// Which zoom levels the writer can populate in the GOL tile directory.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TileZooms {
    /// Split into four children at the next zoom level.
    #[default]
    Consecutive,
    /// Split into up to sixteen children two zoom levels down.
    Even,
}

/// Build a new file. Scratch files are created beside the destination and removed
/// on return. An existing destination is never replaced.
pub fn build_pbf(input: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<BuildReport> {
    build_pbf_with_options(input, output, BuildOptions::default())
}

/// Build a new GOL with an explicit dictionary policy.
pub fn build_pbf_with_options(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: BuildOptions,
) -> Result<BuildReport> {
    build_pbf_with_layout(input, output, options, TileZooms::default())
}

/// Build a GOL 2.0 snapshot with the given dictionary policy and tile hierarchy.
pub fn build_pbf_with_layout(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    options: BuildOptions,
    zooms: TileZooms,
) -> Result<BuildReport> {
    ensure!(
        options.target_tile_bytes > 0,
        "target_tile_bytes must be positive"
    );
    ensure!(
        options.max_duplicate_tiles >= 1,
        "max_duplicate_tiles must be at least 1"
    );
    ensure!(
        (2..=255).contains(&options.rtree_branch_fanout),
        "rtree_branch_fanout must be in 2..=255"
    );
    ensure!(
        (1..=1024).contains(&options.rtree_leaf_size),
        "rtree_leaf_size must be in 1..=1024"
    );
    let output = output.as_ref();
    ensure!(
        !output.try_exists()?,
        "output already exists: {}",
        output.display()
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let input =
        File::open(input.as_ref()).with_context(|| format!("open {}", input.as_ref().display()))?;
    let pbf_bytes = input.metadata().map(|m| m.len()).unwrap_or(0);
    let budget = spill::MemoryBudget::new(parent, options.memory_limit_bytes, pbf_bytes);
    let (header, raw) =
        pbf::read(BufReader::with_capacity(256 * 1024, input), &budget).context("read OSM PBF")?;
    let dictionary = raw.build_dictionary(options);
    let (dataset, mut report) =
        stage::resolve(raw, &dictionary, &budget).context("resolve OSM references")?;
    let (directory, placement) = directory::Directory::build(
        &dataset,
        options.target_tile_bytes,
        options.max_duplicate_tiles,
        zooms,
        &budget,
    )?;
    let mut file = NamedTempFile::new_in(parent)?;
    report.tiles = store::write_parallel(
        file.as_file_mut(),
        &dataset,
        &placement,
        directory,
        &header,
        &dictionary,
        options,
        &budget,
    )?;
    drop(placement);
    drop(dataset);
    drop(budget);
    file.as_file().sync_all()?;
    file.persist_noclobber(output)
        .with_context(|| format!("publish {}", output.display()))?;
    Ok(report)
}

#[cfg(test)]
#[derive(Default, Clone)]
pub(super) struct Tags {
    pub keys: Vec<String>,
    pub values: Vec<String>,
}

#[inline]
pub(super) fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
}

#[inline]
fn varint(buf: &mut Vec<u8>, mut value: u64) {
    while value >= 128 {
        buf.push(value as u8 | 128);
        value >>= 7;
    }
    buf.push(value as u8);
}

#[inline]
fn signed(buf: &mut Vec<u8>, value: i64) {
    varint(buf, ((value << 1) ^ (value >> 63)) as u64);
}

// Low two bits distinguish the independent OSM ID namespaces.
#[inline]
fn object_id(kind: i64, id: i64) -> Result<i64> {
    ensure!((1..1_i64 << 52).contains(&id), "invalid OSM object ID {id}");
    Ok(id * 4 + kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bounds, FeatureType, Library};
    use std::collections::BTreeSet;

    #[test]
    fn only_complete_ways_mark_nodes_as_used() -> Result<()> {
        let mut raw = stage::RawStage::new();
        for id in 1..=4 {
            raw.add_node(id, id as f64, 0.0, &Tags::default())?;
        }
        raw.add_way(10, &Tags::default(), vec![1, 2])?;
        raw.add_way(11, &Tags::default(), vec![2, 1])?;
        raw.add_way(12, &Tags::default(), vec![3, 4, 99])?;
        let dictionary = raw.build_dictionary(BuildOptions::default());
        let (dataset, report) =
            stage::resolve(raw, &dictionary, &spill::MemoryBudget::unlimited())?;
        assert_eq!(report.omitted_ways, 1);
        let mut orphans = Vec::new();
        for fi in 0..dataset.way_base {
            let f = dataset.feature(fi)?;
            let tags: Vec<(String, String)> = dataset
                .feature_tags_vec(fi)?
                .into_iter()
                .map(|(k, v)| {
                    (
                        dataset.strings.get(k).to_owned(),
                        dataset.strings.get(v).to_owned(),
                    )
                })
                .collect();
            assert_eq!(tags, [("geodesk:orphan".to_owned(), "yes".to_owned())]);
            orphans.push(f.obj_id / 4);
        }
        assert_eq!(orphans, [3, 4]);
        Ok(())
    }

    #[test]
    fn grouped_indexes_preserve_mixed_key_and_value_queries() -> Result<()> {
        use crate::{Query, decode::Decode};
        let temp = tempfile::tempdir()?;
        let mut raw = stage::RawStage::new();
        let keys = ["amenity", "building", "highway", "shop", "route", "leisure"];
        for id in 1..=1030 {
            let mut tags = Tags {
                keys: vec!["marker".into()],
                values: vec!["bench".into()],
            };
            for (bit, key) in keys.iter().enumerate() {
                if id & (1 << bit) != 0 {
                    tags.keys.push((*key).to_owned());
                    tags.values.push("yes".into());
                }
            }
            if id <= 3 {
                tags.keys.push("bridge:structure".into());
                tags.values.push("arch".into());
            } else if id <= 6 {
                tags.keys.push("tunnel".into());
                tags.values.push("yes".into());
            }
            if id == 1 {
                tags.keys.push("bench".into());
                tags.values.push("wood".into());
            } else if id == 2 {
                tags.keys.push("sample".into());
                tags.values.push("marker".into());
            }
            raw.add_node(id, id as f64 / 10000.0, 0.0, &tags)?;
        }
        let dictionary = raw.build_dictionary(BuildOptions::default());
        let (dataset, _) = stage::resolve(raw, &dictionary, &spill::MemoryBudget::unlimited())?;
        let (directory, placement) =
            directory::Directory::with_target(&dataset, u64::MAX, TileZooms::Consecutive)?;
        let path = temp.path().join("indexed.gol");
        store::write(
            &mut File::create(&path)?,
            &dataset,
            &placement,
            directory,
            &Default::default(),
            &dictionary,
        )?;
        let lib = Library::open(path)?;
        let tile = lib.tile_page(1)?;
        assert_eq!(
            lib.source().u32(tile + 24)?,
            0,
            "local features need no exports"
        );
        let roots = lib.source().relative(tile + 8, !0)?;
        let mut root = roots;
        while lib.source().u32(root)? & 1 == 0 {
            root += 8;
        }
        let root_count = (root - roots) / 8 + 1;
        assert!(root_count <= u64::from(encode::MAX_KEY_INDEXES));
        for (selector, count) in [
            ("n[bench]", 1),
            ("n[bench=wood]", 1),
            ("n[!bench]", 1029),
            ("n[marker]", 1030),
            ("n[marker=bench]", 1030),
            ("n[sample]", 1),
            ("n[sample=marker]", 1),
            ("n[bridge:structure=arch]", 3),
            ("n[tunnel]", 3),
            ("n[bridge]", 0),
            ("n[amenity][building]", 257),
        ] {
            let query = Query::parse(selector)?;
            let found = query
                .prepare(&lib)?
                .iter()
                .collect::<crate::Result<Vec<_>>>()?;
            assert_eq!(found.len(), count, "{selector}");
        }
        for f in lib.features() {
            let f = f?;
            let (key, expected) = match f.id().id {
                1 => ("bench", "wood"),
                2 => ("sample", "marker"),
                _ => continue,
            };
            assert_eq!(f.tag(key)?.unwrap().to_string(), expected);
            let tags = f.tags()?.collect::<crate::Result<Vec<_>>>()?;
            assert!(
                tags.iter()
                    .any(|t| t.key == key && t.value.to_string() == expected)
            );
        }
        Ok(())
    }

    #[test]
    fn leaves_share_tables_and_preserve_local_key_value_spellings() -> Result<()> {
        use crate::decode::Decode;
        let dir = tempfile::tempdir()?;
        let mut raw = stage::RawStage::new();
        // Far apart in spatial order, these two nodes share their entire table.
        // A second pair uses the same local spelling as a key and a value.
        for (id, key, value) in [
            (1, "first", "shared value"),
            (2, "local spelling", "odd"),
            (3, "first", "shared value"),
            (4, "other", "local spelling"),
        ] {
            raw.add_node(
                id,
                id as f64,
                0.0,
                &Tags {
                    keys: vec![key.into()],
                    values: vec![value.into()],
                },
            )?;
        }
        for id in 5..200 {
            raw.add_node(
                id,
                2.0,
                0.0,
                &Tags {
                    keys: vec!["note".into()],
                    values: vec![format!("value {id}")],
                },
            )?;
        }
        let dictionary = raw.build_dictionary(BuildOptions::default());
        let (dataset, _) = stage::resolve(raw, &dictionary, &spill::MemoryBudget::unlimited())?;
        let (directory, placement) =
            directory::Directory::with_target(&dataset, u64::MAX, TileZooms::Consecutive)?;
        let path = dir.path().join("leaves.gol");
        store::write(
            &mut File::create(&path)?,
            &dataset,
            &placement,
            directory,
            &Default::default(),
            &dictionary,
        )?;
        let lib = Library::open(path)?;
        let mut shared = Vec::new();
        for feature in lib.features() {
            let f = feature?;
            match f.id().id {
                1 | 3 => {
                    assert_eq!(f.tag("first")?.unwrap().to_string(), "shared value");
                    shared.push(lib.source().relative(f.anchor() + 8, !1)?);
                }
                2 => assert_eq!(f.tag("local spelling")?.unwrap().to_string(), "odd"),
                4 => assert_eq!(f.tag("other")?.unwrap().to_string(), "local spelling"),
                id => assert_eq!(f.tag("note")?.unwrap().to_string(), format!("value {id}")),
            }
        }
        assert_eq!(shared.len(), 2);
        assert_eq!(shared[0], shared[1]);
        Ok(())
    }

    #[test]
    fn even_zooms_address_all_sixteen_children_in_row_order() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let mut raw = stage::RawStage::new();
        let mut id = 0;
        for lat in [80.0, 40.0, -40.0, -80.0] {
            for lon in [-140.0, -50.0, 50.0, 140.0] {
                id += 1;
                raw.add_node(id, lon, lat, &Tags::default())?;
            }
        }
        let dictionary = raw.build_dictionary(BuildOptions::default());
        let (dataset, _) = stage::resolve(raw, &dictionary, &spill::MemoryBudget::unlimited())?;
        let (directory, placement) =
            directory::Directory::with_target(&dataset, 1, TileZooms::Even)?;
        assert_eq!(directory.words[2], 0xffff);
        let path = temp.path().join("grid.gol");
        store::write(
            &mut File::create(&path)?,
            &dataset,
            &placement,
            directory,
            &Default::default(),
            &dictionary,
        )?;
        let lib = Library::open(path)?;
        let mut found = 0;
        for f in lib.features() {
            let f = f?;
            let ids = lib
                .in_bounds(f.bounds()?)
                .map(|item| item.map(|f| f.id()))
                .collect::<crate::Result<Vec<_>>>()?;
            assert_eq!(ids, [f.id()]);
            found += 1;
        }
        assert_eq!(found, 16);
        Ok(())
    }

    #[test]
    fn border_duplication_preserves_integrity_and_exact_query_identities() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("stress.osm.pbf");
        std::fs::write(&src, include_bytes!("../../tests/fixtures/stress.osm.pbf"))?;
        let single_path = dir.path().join("single.gol");
        let dup_path = dir.path().join("dup.gol");
        build_pbf_with_layout(
            &src,
            &single_path,
            BuildOptions {
                target_tile_bytes: 16 * 1024,
                max_duplicate_tiles: 1,
                ..Default::default()
            },
            TileZooms::Consecutive,
        )?;
        build_pbf_with_layout(
            &src,
            &dup_path,
            BuildOptions {
                target_tile_bytes: 16 * 1024,
                max_duplicate_tiles: 16,
                ..Default::default()
            },
            TileZooms::Consecutive,
        )?;
        let single = Library::open(&single_path)?;
        let dup = Library::open(&dup_path)?;
        dup.check_integrity()?;
        let ids = |lib: &Library<_>, bounds: Option<Bounds>| -> crate::Result<Vec<_>> {
            let mut v = match bounds {
                None => lib
                    .features()
                    .map(|f| f.map(|f| f.id()))
                    .collect::<crate::Result<Vec<_>>>()?,
                Some(b) => lib
                    .in_bounds(b)
                    .map(|f| f.map(|f| f.id()))
                    .collect::<crate::Result<Vec<_>>>()?,
            };
            let len = v.len();
            v.sort();
            v.dedup();
            assert_eq!(
                v.len(),
                len,
                "duplicate feature returned for bounds={bounds:?}"
            );
            Ok(v)
        };
        assert_eq!(ids(&single, None)?, ids(&dup, None)?);
        let world_bounds = single
            .features()
            .map(|f| f.and_then(|f| f.bounds()))
            .collect::<crate::Result<Vec<_>>>()?;
        for sample in world_bounds.into_iter().step_by(37) {
            assert_eq!(ids(&single, Some(sample))?, ids(&dup, Some(sample))?);
        }
        Ok(())
    }

    #[test]
    fn crossing_tiles_preserves_member_order_ids_and_parent_links() -> Result<()> {
        for zooms in [TileZooms::Consecutive, TileZooms::Even] {
            check_cross_tile_links(zooms)?;
        }
        Ok(())
    }

    fn check_cross_tile_links(zooms: TileZooms) -> Result<()> {
        let dir = tempfile::tempdir()?;
        let (header, raw) = pbf::read(
            include_bytes!("../../tests/fixtures/basic.osm.pbf").as_slice(),
            &spill::MemoryBudget::unlimited(),
        )?;
        let dictionary = raw.build_dictionary(BuildOptions::default());
        let (dataset, _) = stage::resolve(raw, &dictionary, &spill::MemoryBudget::unlimited())?;
        let (directory, placement) = directory::Directory::with_target(&dataset, 1, zooms)?;
        let path = dir.path().join("tiled.gol");
        let count = store::write(
            &mut File::create(&path)?,
            &dataset,
            &placement,
            directory,
            &header,
            &dictionary,
        )?;
        assert!(count > 2);
        let lib = Library::open(path)?;
        let mut seen = BTreeSet::new();
        for feature in lib.features() {
            let f = feature?;
            assert!(seen.insert(f.id()));
            if f.kind() == FeatureType::Way && f.id().id == 10 {
                let nodes = f
                    .feature_nodes()?
                    .map(|n| n.map(|f| f.id().id))
                    .collect::<crate::Result<Vec<_>>>()?;
                assert_eq!(nodes, [5, 1]);
            }
            if f.kind() == FeatureType::Relation && f.id().id == 21 {
                let members = f
                    .members()?
                    .map(|m| m.map(|m| (m.feature.id().id, m.role.into_owned())))
                    .collect::<crate::Result<Vec<_>>>()?;
                assert_eq!(
                    members,
                    [
                        (5, "stop".into()),
                        (10, "".into()),
                        (1, "custom rôle".into()),
                        (5, "stop".into())
                    ]
                );
            }
            if f.kind() == FeatureType::Node && f.id().id == 1 {
                let parents = f
                    .parent_relations()?
                    .map(|p| p.map(|p| p.id().id))
                    .collect::<crate::Result<Vec<_>>>()?;
                assert_eq!(parents, [21, 22]);
            }
        }
        assert_eq!(seen.len(), 8);
        let crossing = lib
            .in_bounds(Bounds::new(-1, -1, 1, 1)?)
            .map(|f| f.map(|f| f.id().to_string()))
            .collect::<crate::Result<BTreeSet<_>>>()?;
        assert_eq!(crossing, ["R21", "R22", "W10"].map(str::to_owned).into());
        Ok(())
    }

    #[test]
    fn spilled_build_matches_in_memory_across_fixtures_and_duplication() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let fixtures: &[(&str, &[u8], u64, usize)] = &[
            (
                "basic",
                include_bytes!("../../tests/fixtures/basic.osm.pbf"),
                1,
                1,
            ),
            (
                "wide",
                include_bytes!("../../tests/fixtures/wide_exports.osm.pbf"),
                16 * 1024,
                16,
            ),
        ];

        for &(name, pbf_bytes, target_tile_bytes, max_duplicate_tiles) in fixtures {
            let src = dir.path().join(format!("{name}.osm.pbf"));
            std::fs::write(&src, pbf_bytes)?;

            let mem_path = dir.path().join(format!("{name}-mem.gol"));
            let spill_path = dir.path().join(format!("{name}-spill.gol"));

            build_pbf_with_layout(
                &src,
                &mem_path,
                BuildOptions {
                    target_tile_bytes,
                    max_duplicate_tiles,
                    memory_limit_bytes: None,
                    ..Default::default()
                },
                TileZooms::Consecutive,
            )?;
            build_pbf_with_layout(
                &src,
                &spill_path,
                BuildOptions {
                    target_tile_bytes,
                    max_duplicate_tiles,
                    memory_limit_bytes: Some(0),
                    ..Default::default()
                },
                TileZooms::Consecutive,
            )?;

            // Ensure no leftover .regol-spill-* temporary directories remain.
            let leftover: Vec<_> = std::fs::read_dir(dir.path())?
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with(".regol-spill-"))
                .collect();
            assert!(
                leftover.is_empty(),
                "leftover spill directories after {name} build: {leftover:?}"
            );

            let mem_lib = Library::open(&mem_path)?;
            let spill_lib = Library::open(&spill_path)?;
            spill_lib.check_integrity()?;

            let snapshot = |lib: &Library<_>| -> crate::Result<Vec<_>> {
                let mut rows = Vec::new();
                for feat in lib.features() {
                    let f = feat?;
                    let mut tags: Vec<(String, String)> = f
                        .tags()?
                        .map(|t| {
                            let t = t?;
                            Ok((t.key.into_owned(), t.value.to_string()))
                        })
                        .collect::<crate::Result<_>>()?;
                    tags.sort();
                    let fnodes = if f.kind() == FeatureType::Way {
                        f.feature_nodes()?
                            .map(|n| n.map(|n| n.id().id))
                            .collect::<crate::Result<Vec<_>>>()?
                    } else {
                        Vec::new()
                    };
                    let members = if f.kind() == FeatureType::Relation {
                        f.members()?
                            .map(|m| m.map(|m| (m.feature.id().to_string(), m.role.into_owned())))
                            .collect::<crate::Result<Vec<_>>>()?
                    } else {
                        Vec::new()
                    };
                    let parents = f
                        .parent_relations()?
                        .map(|p| p.map(|p| p.id().id))
                        .collect::<crate::Result<Vec<_>>>()?;
                    rows.push((
                        f.id().to_string(),
                        f.bounds()?,
                        tags,
                        fnodes,
                        members,
                        parents,
                    ));
                }
                rows.sort_by(|a, b| a.0.cmp(&b.0));
                Ok(rows)
            };

            assert_eq!(
                snapshot(&mem_lib)?,
                snapshot(&spill_lib)?,
                "spilled build output diverged from in-memory build for {name}"
            );
        }
        Ok(())
    }

    #[test]
    fn spilled_build_external_merges_unsorted_nodes_and_rejects_duplicates() -> Result<()> {
        let dir = tempfile::tempdir()?;

        // Build a synthetic RawStage with 100,000 reversed-ID nodes (1.6 MiB > 1 MiB
        // minimum sort buffer) across two workers so DiskNodeStore must execute
        // multi-run external k-way merge sort.
        let make_unsorted = |include_dup: bool| -> Result<(spill::MemoryBudget, stage::RawStage)> {
            let budget = spill::MemoryBudget::force_spill_in(dir.path(), 0);
            let sink = spill::RawSpillSink::new();

            let mut w0 = stage::WorkerStage::new();
            let k_name = w0.intern("name");
            let v_pin = w0.intern("pin");
            w0.count(k_name, true);
            w0.count(v_pin, false);
            let mut nodes0 = Vec::with_capacity(50_000);
            for i in (50_001..=100_000i64).rev() {
                nodes0.push(stage::RawNode {
                    id: i,
                    x: (i as i32) * 10,
                    y: (i as i32) * 5,
                });
            }
            w0.node_chunks.push((0, false, nodes0));
            let tag_start = w0.next_tag_offset();
            w0.tags.push((k_name, v_pin));
            w0.tagged_nodes.push(stage::RawTaggedNode {
                id: 75_000,
                tag_start,
                tag_len: 1,
            });
            w0.maybe_spill(&budget, &sink, true)?;

            let mut w1 = stage::WorkerStage::new();
            let k_hw1 = w1.intern("highway");
            let v_path1 = w1.intern("path");
            w1.count(k_hw1, true);
            w1.count(v_path1, false);
            let mut nodes1 = Vec::with_capacity(50_001);
            for i in (1..=50_000i64).rev() {
                nodes1.push(stage::RawNode {
                    id: i,
                    x: (i as i32) * 10,
                    y: (i as i32) * 5,
                });
            }
            if include_dup {
                nodes1.push(stage::RawNode {
                    id: 75_000,
                    x: 1,
                    y: 2,
                });
            }
            w1.node_chunks.push((1, false, nodes1));
            let way_tag_start = w1.next_tag_offset();
            w1.tags.push((k_hw1, v_path1));
            w1.way_refs.extend_from_slice(&[1, 75_000, 100_000]);
            w1.ways.push(stage::RawWay {
                id: 500,
                tag_start: way_tag_start,
                ref_start: 0,
                tag_len: 1,
                ref_len: 3,
            });
            w1.maybe_spill(&budget, &sink, true)?;

            Ok((budget, stage::RawStage::from_workers(vec![w0, w1], sink)))
        };

        let opts = BuildOptions::default();
        let (budget, raw) = make_unsorted(false)?;
        let dictionary = raw.build_dictionary(opts);
        let (dataset, report) = stage::resolve(raw, &dictionary, &budget)?;
        // 100,000 nodes minus 2 untagged way-only nodes (1, 100_000) + 1 way = 99,999.
        assert_eq!(report.features, 99_999);
        let (directory, placement) = directory::Directory::build(
            &dataset,
            opts.target_tile_bytes,
            opts.max_duplicate_tiles,
            TileZooms::Consecutive,
            &budget,
        )?;
        let path = dir.path().join("external-sorted.gol");
        store::write_parallel(
            &mut File::create(&path)?,
            &dataset,
            &placement,
            directory,
            &pbf::Header::default(),
            &dictionary,
            opts,
            &budget,
        )?;
        drop(dataset);
        drop(budget);

        let lib = Library::open(&path)?;
        lib.check_integrity()?;
        let mut saw_way = false;
        let mut saw_tagged = false;
        let mut saw_way_only_untagged = false;
        let mut count = 0usize;
        for feat in lib.features() {
            let f = feat?;
            count += 1;
            if f.kind() == FeatureType::Way && f.id().id == 500 {
                saw_way = true;
                assert_eq!(f.bounds()?, Bounds::new(10, 5, 1_000_000, 500_000)?);
                let fnodes = f
                    .feature_nodes()?
                    .map(|n| n.map(|n| n.id().id))
                    .collect::<crate::Result<Vec<_>>>()?;
                assert_eq!(fnodes, vec![75_000]);
            } else if f.kind() == FeatureType::Node && f.id().id == 75_000 {
                saw_tagged = true;
                assert_eq!(
                    f.bounds()?,
                    Bounds::new(750_000, 375_000, 750_000, 375_000)?
                );
            } else if f.kind() == FeatureType::Node && (f.id().id == 1 || f.id().id == 100_000) {
                saw_way_only_untagged = true;
            }
        }
        assert_eq!(count, 99_999);
        assert!(saw_way && saw_tagged && !saw_way_only_untagged);

        let (dup_budget, dup_raw) = make_unsorted(true)?;
        let dup_dict = dup_raw.build_dictionary(BuildOptions::default());
        let err = match stage::resolve(dup_raw, &dup_dict, &dup_budget) {
            Ok(_) => panic!("expected duplicate node ID to fail in external merge"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("duplicate OSM node ID 75000"),
            "unexpected error: {err:#}"
        );
        Ok(())
    }
}
