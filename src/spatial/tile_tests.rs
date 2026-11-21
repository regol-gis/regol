use super::*;
use std::{io, sync::Mutex};

struct Observed<'a> {
    bytes: &'a [u8],
    reads: Mutex<Vec<(u64, usize)>>,
}
impl Source for Observed<'_> {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        self.reads.lock().unwrap().push((offset, output.len()));
        self.bytes.read_exact_at(offset, output)
    }
}

#[cfg(feature = "parallel")]
#[test]
fn partitioning_can_leave_payload_reads_to_independent_traversals() -> Result<()> {
    let bytes = include_bytes!("../../tests/fixtures/tiled_global.gol").as_slice();
    let library = Library::new(Observed {
        bytes,
        reads: Mutex::new(Vec::new()),
    })?;
    let reference = Library::new(bytes)?;
    for bounds in [None, Some(Bounds::new(-30000, -30000, 30000, 30000)?)] {
        library.source().reads.lock().unwrap().clear();
        let (partitions, _) = Features::new(&library, bounds).partitions(1)?;
        let Partitions::Wide { template, tiles } = partitions else {
            panic!("expected a flat catalog");
        };
        assert!(tiles.len() > 1);
        assert!(!template.has_remaining_work());
        let reads = library.source().reads.lock().unwrap();
        assert!(!reads.is_empty());
        assert!(
            reads.iter().all(|&(p, len)| {
                p >= library.tile_index + 4 && p + len as u64 <= library.tile_index_end
            }),
            "partition discovery must not read any tile payload"
        );
        drop(reads);
        let mut actual = Vec::new();
        let mut cursor = template.worker_cursor();
        for tile in tiles {
            cursor.defer_tile(tile);
            for feature in &mut cursor {
                let feature = feature?;
                actual.push((feature.id(), feature.bounds()?));
            }
        }
        let mut expected = Features::new(&reference, bounds)
            .map(|f| f.and_then(|f| Ok((f.id(), f.bounds()?))))
            .collect::<Result<Vec<_>>>()?;
        actual.sort_unstable_by_key(|f| f.0);
        expected.sort_unstable_by_key(|f| f.0);
        assert_eq!(actual, expected);
    }
    Ok(())
}

#[cfg(feature = "parallel")]
#[test]
fn deferred_tile_errors_are_reported_when_consumed_and_fuse_the_partition() -> Result<()> {
    // Keep the valid index but zero every payload length. Index partitioning
    // should succeed; each affected traversal must report an error.
    let bytes = include_bytes!("../../tests/fixtures/tiled_global.gol").as_slice();
    let reference = Library::new(bytes)?;
    let mut damaged = bytes.to_vec();
    let (partitions, _) = Features::new(&reference, None).partitions(1)?;
    let Partitions::Wide { template, tiles } = partitions else {
        panic!("expected a flat catalog");
    };
    let headers = tiles.len();
    assert!(!template.has_remaining_work());
    for tile in tiles {
        let start = tile.start as usize;
        damaged[start..start + 4].copy_from_slice(&0u32.to_le_bytes());
    }
    assert!(headers > 1);
    let library = Library::new(damaged)?;
    let (partitions, _) = Features::new(&library, None).partitions(1)?;
    let Partitions::Wide { template, tiles } = partitions else {
        panic!("expected a flat catalog");
    };
    assert_eq!(tiles.len(), headers);
    let mut cursor = template.worker_cursor();
    for tile in tiles {
        cursor.defer_tile(tile);
        assert!(cursor.next().expect("deferred header error").is_err());
        assert!(cursor.next().is_none());
    }
    Ok(())
}
#[test]
fn tile_header_reads_preserve_type_masks_and_signed_root_offsets() -> Result<()> {
    let fixture = include_bytes!("../../tests/fixtures/basic.gol");
    for offsets in [[24i32, 28, 32, -4], [0, 0, 0, 0], [24, 0, 0, -4]] {
        let mut bytes = fixture.to_vec();
        bytes.resize(bytes.len().next_multiple_of(4096), 0);
        let start = bytes.len() as u64;
        bytes.extend_from_slice(&[0; 64]);
        // Both high flag bits are independent of the payload length.
        bytes[start as usize..start as usize + 4].copy_from_slice(&0xc000_003cu32.to_le_bytes());
        for (index, delta) in offsets.iter().enumerate() {
            let at = start as usize + 8 + index * 4;
            bytes[at..at + 4].copy_from_slice(&delta.to_le_bytes());
        }
        let library = Library::new(Observed {
            bytes: &bytes,
            reads: Mutex::new(Vec::new()),
        })?;
        for types in 0..16 {
            let mut features = Features::new(&library, None).restrict(types, 0);
            features.work.clear();
            library.source().reads.lock().unwrap().clear();
            features.load_tile(start, Bounds::WORLD)?;
            assert_eq!(features.tile.start(), start);
            assert_eq!(features.tile.end(), start + 64);
            let actual = features
                .work
                .iter()
                .map(|work| match work {
                    Work::Roots { p, node } => (*p, *node),
                    _ => panic!("unexpected header work"),
                })
                .collect::<Vec<_>>();
            let expected = (0..4)
                .rev()
                .filter_map(|index| {
                    let delta = offsets[index];
                    (types & (1 << index) != 0 && delta != 0).then(|| {
                        (
                            (start as i64 + 8 + index as i64 * 4 + i64::from(delta)) as u64,
                            index == 0,
                        )
                    })
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "type mask {types}");
            // A positioned-I/O source should not reread adjacent root fields.
            assert_eq!(*library.source().reads.lock().unwrap(), [(start, 24)]);
        }
    }
    Ok(())
}

#[test]
fn tile_header_coalescing_still_rejects_truncation_and_bad_extents() -> Result<()> {
    let fixture = include_bytes!("../../tests/fixtures/basic.gol");
    for available in (0..=28).chain([63, 64]) {
        for payload in [0u32, 23, 24, 60, 0x3fff_ffff] {
            let mut bytes = fixture.to_vec();
            bytes.resize(bytes.len().next_multiple_of(4096), 0);
            let start = bytes.len() as u64;
            bytes.extend_from_slice(&[0; 64]);
            bytes[start as usize..start as usize + 4].copy_from_slice(&payload.to_le_bytes());
            bytes.truncate(start as usize + available);
            let library = Library::new(Observed {
                bytes: &bytes,
                reads: Mutex::new(Vec::new()),
            })?;
            let mut features = Features::new(&library, None);
            features.work.clear();
            let result = features.load_tile(start, Bounds::WORLD);
            assert_eq!(
                result.is_ok(),
                payload >= 24 && u64::from(payload) + 4 <= available as u64,
                "available={available}, payload={payload}"
            );
        }
    }
    Ok(())
}

#[test]
fn tile_root_pointers_still_reject_underflow_and_outside_tile_reads() -> Result<()> {
    let fixture = include_bytes!("../../tests/fixtures/basic.gol");
    for delta in [i32::MIN, i32::MAX] {
        let mut bytes = fixture.to_vec();
        bytes.resize(bytes.len().next_multiple_of(4096), 0);
        let start = bytes.len() as u64;
        bytes.extend_from_slice(&[0; 64]);
        bytes[start as usize..start as usize + 4].copy_from_slice(&60u32.to_le_bytes());
        bytes[start as usize + 8..start as usize + 12].copy_from_slice(&delta.to_le_bytes());
        let library = Library::new(bytes)?;
        let mut features = Features::new(&library, None);
        features.work.clear();
        let loaded = features.load_tile(start, Bounds::WORLD);
        if delta < 0 {
            assert!(loaded.is_err());
        } else {
            loaded?;
            assert!(features.next().expect("root outside tile").is_err());
            assert!(features.next().is_none());
        }
    }
    Ok(())
}

#[test]
fn rejected_root_run_pauses_before_reading_the_next_root() -> Result<()> {
    let mut bytes = include_bytes!("../../tests/fixtures/basic.gol").to_vec();
    bytes.resize(bytes.len().next_multiple_of(4096), 0);
    let start = bytes.len();
    bytes.resize(start + 72, 0);
    for offset in [0, 8] {
        bytes[start + offset..start + offset + 4].copy_from_slice(&i32::MIN.to_le_bytes());
    }
    for (offset, word) in [(16, 17u32), (20, 4), (32, 23), (60, 1)] {
        bytes[start + offset..start + offset + 4].copy_from_slice(&word.to_le_bytes());
    }
    let library = Library::new(Observed {
        bytes: &bytes,
        reads: Mutex::new(Vec::new()),
    })?;
    let start = start as u64;
    let mut features = Features::new(&library, None).restrict(1, 4);
    features.tile = TileExtent::new(start, start + 72)?;
    features.work = vec![Work::Roots {
        p: start,
        node: true,
    }];
    library.source().reads.lock().unwrap().clear();
    let mut steps = 0;
    let paused = features.next_with_budget::<false>(
        |_, _, _| Ok(true),
        || {
            steps += 1;
            Ok(steps == 1)
        },
    );
    assert!(paused.is_none());
    assert_eq!(features.steps(), 1);
    assert_eq!(&*library.source().reads.lock().unwrap(), &[(start, 8)]);
    assert_eq!(features.next().expect("remaining feature")?.id().id, 0);
    assert_eq!(features.steps(), 5);
    assert!(features.next().is_none());
    Ok(())
}

#[cfg(all(feature = "mmap", target_os = "linux"))]
#[test]
fn mapped_advice_applies_to_unbounded_cursor_and_clears_on_error() -> Result<()> {
    use crate::source::mapped_scan_test_support::{
        force_sequential, mapped_fixture, sequential_at,
    };

    let bytes = include_bytes!("../../tests/fixtures/basic.gol").as_slice();
    let reference = Library::new(bytes)?;
    assert!(Features::new(&reference, None).read_ahead.is_none());
    let mapped = Library::new(mapped_fixture(bytes)?)?;

    let ids = Features::new(&reference, None)
        .map(|feature| feature.map(Feature::id))
        .collect::<Result<Vec<_>>>()?;

    let mut unbounded = Features::new(&mapped, None);
    force_sequential(unbounded.read_ahead.as_mut().expect("unbounded mmap"));
    assert_eq!(
        unbounded.next().transpose()?.map(Feature::id),
        ids.first().copied()
    );
    let start = unbounded.tile.start() as usize;
    if let Some(sequential) = sequential_at(mapped.source(), start) {
        assert!(sequential);
    }

    // A bounded cursor on the same map can run while advice is active.
    let bounded = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let bounded = Features::new(&mapped, Some(Bounds::WORLD));
                assert!(bounded.read_ahead.is_none());
                bounded
                    .map(|feature| feature.map(Feature::id))
                    .collect::<Result<Vec<_>>>()
            })
            .join()
            .unwrap()
    })?;
    assert_eq!(bounded, ids);

    // An invalid branch must surface the normal checked error and drop the hint
    // immediately, even if the caller keeps the fused iterator alive.
    unbounded.work = vec![Work::Branch {
        p: unbounded.tile.end(),
        node: true,
        depth: 0,
    }];
    unbounded.leaf = None;
    assert!(unbounded.next().expect("invalid branch").is_err());
    assert!(unbounded.next().is_none());
    assert!(unbounded.read_ahead.is_none());
    if let Some(sequential) = sequential_at(mapped.source(), start) {
        assert!(!sequential);
    }
    Ok(())
}

#[cfg(all(feature = "mmap", feature = "parallel", target_os = "linux"))]
#[test]
fn partition_handoff_releases_active_mmap_advice() -> Result<()> {
    use crate::source::mapped_scan_test_support::{
        force_sequential, mapped_fixture, sequential_at,
    };

    let bytes = include_bytes!("../../tests/fixtures/tiled_global.gol");
    let mapped = Library::new(mapped_fixture(bytes)?)?;
    let mut active = Features::new(&mapped, None);
    force_sequential(active.read_ahead.as_mut().expect("unbounded mmap"));
    active.next().expect("first feature")?;
    let start = active.tile.start() as usize;
    if let Some(sequential) = sequential_at(mapped.source(), start) {
        assert!(sequential);
    }
    let (partitions, _) = active.partitions(1)?;
    match partitions {
        Partitions::Wide { template, .. } => {
            assert!(template.read_ahead.is_none());
            assert!(template.worker_cursor().read_ahead.is_none());
        }
        Partitions::Split(workers) => {
            assert!(
                workers
                    .into_iter()
                    .all(|worker| worker.read_ahead.is_none())
            );
        }
    }
    if let Some(sequential) = sequential_at(mapped.source(), start) {
        assert!(!sequential);
    }
    Ok(())
}
