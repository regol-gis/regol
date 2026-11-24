use super::*;
use std::{
    fs,
    io::{self, Write},
};

#[allow(unsafe_code)]
pub(crate) fn mapped_fixture(bytes: &[u8]) -> io::Result<MappedFile> {
    fixture_with_writer(bytes).map(|(mapped, _)| mapped)
}

#[allow(unsafe_code)]
fn fixture_with_writer(bytes: &[u8]) -> io::Result<(MappedFile, fs::File)> {
    let mut file = tempfile::tempfile()?;
    file.write_all(bytes)?;
    // SAFETY: the anonymous file is owned by this test. The writer is used
    // again only after the mapping and its helpers have been dropped.
    let mapping = unsafe { MappedFile::from_immutable_file(&file)? };
    Ok((mapping, file))
}

pub(crate) fn force_helpers(scan: &mut MappedScan<'_>) {
    scan.helper_faults_observed = true;
}

pub(crate) fn force_sequential(scan: &mut MappedScan<'_>) {
    scan.sequential = true;
    scan.next_sample = u64::MAX;
}

#[test]
fn only_slow_large_demand_reads_decline_speculation() {
    let start = Activity {
        major_faults: 10,
        input_blocks: 100,
        wall: Instant::now(),
        cpu: Duration::ZERO,
    };
    let decision = |faults, blocks, wall_ms, cpu_ms| {
        Activity {
            major_faults: start.major_faults + faults,
            input_blocks: start.input_blocks + blocks,
            wall: start.wall + Duration::from_millis(wall_ms),
            cpu: Duration::from_millis(cpu_ms),
        }
        .speculate_since(start)
    };
    let bulk = 2 * BULK_BLOCKS_PER_FAULT;
    assert_eq!(decision(1, bulk, 40, 0), None);
    assert_eq!(decision(2, bulk, 40, 0), Some(false));
    assert_eq!(decision(2, bulk, 8, 0), Some(false));
    assert_eq!(decision(2, bulk - 1, 8, 0), Some(true));
    assert_eq!(decision(2, bulk, 7, 0), Some(true));
    assert_eq!(decision(2, bulk, 40, 39), Some(true));
    assert_eq!(decision(2, 16, 40, 0), Some(true));
    assert_eq!(decision(2, 0, 40, 0), Some(true));
}

pub(crate) fn sequential_at(map: &MappedFile, offset: usize) -> Option<bool> {
    flag_at(map, offset, "sr")
}

fn flag_at(map: &MappedFile, offset: usize, wanted: &str) -> Option<bool> {
    let address = map.map.as_ptr() as usize + offset;
    let contents = fs::read_to_string("/proc/self/smaps").ok()?;
    let mut in_mapping = false;
    for line in contents.lines() {
        let Some(first) = line.split_whitespace().next() else {
            continue;
        };
        let range = first.split_once('-').and_then(|(start, end)| {
            Some(usize::from_str_radix(start, 16).ok()?..usize::from_str_radix(end, 16).ok()?)
        });
        if let Some(range) = range {
            in_mapping = range.contains(&address);
        }
        if let Some(flags) = line.strip_prefix("VmFlags:").filter(|_| in_mapping) {
            return Some(flags.split_whitespace().any(|flag| flag == wanted));
        }
    }
    None
}

#[cfg(feature = "parallel")]
#[test]
fn overlapping_demand_scans_restore_mapping_advice_after_the_last_exit() -> io::Result<()> {
    let map = mapped_fixture(&[0; 8192])?;
    let first = map.demand_advice().expect("random advice");
    let second = map.demand_advice().expect("overlapping scan");
    drop(first);
    let mut scan = MappedScan::new(&map);
    force_sequential(&mut scan);
    scan.enter_tile(0, 4096);
    assert!(scan.advised.is_none());
    if let Some(random) = flag_at(&map, 0, "rr") {
        assert!(random);
    }
    drop(second);
    if let Some(random) = flag_at(&map, 4096, "rr") {
        assert!(!random);
    }
    scan.enter_tile(0, 4096);
    assert!(scan.advised.is_some());
    assert!(map.demand_advice().is_none());
    drop(scan);
    assert!(map.demand_advice().is_some());
    Ok(())
}

#[test]
fn advising_a_mapped_range_validates_extent_and_clears_on_drop() -> io::Result<()> {
    let map = mapped_fixture(&[0; 8192])?;
    let mut scan = MappedScan::new(&map);
    force_sequential(&mut scan);
    for (start, end) in [(1, 0), (4096, 8193), (0, u64::MAX)] {
        scan.enter_tile(start, end);
        assert!(scan.advised.is_none());
    }
    scan.enter_tile(0, 4096);
    assert_eq!(scan.advised, Some((0, 4096)));
    if let Some(sequential) = sequential_at(&map, 0) {
        assert!(sequential);
    }
    drop(scan);
    if let Some(sequential) = sequential_at(&map, 0) {
        assert!(!sequential);
    }
    Ok(())
}

#[test]
fn scan_sampling_restarts_when_an_unadmitted_cursor_moves_threads() -> io::Result<()> {
    let map = mapped_fixture(&[0; 4096])?;
    let mut scan = MappedScan::new(&map);
    scan.previous = Some((
        thread::current().id(),
        Activity {
            major_faults: u64::MAX,
            input_blocks: u64::MAX,
            wall: Instant::now(),
            cpu: Duration::ZERO,
        },
    ));
    thread::scope(|scope| {
        scope
            .spawn(move || {
                scan.sample(1_000);
                assert!(!scan.sequential);
                if scan.next_sample != u64::MAX {
                    let (current, _) = scan.previous.expect("new thread sample");
                    assert_eq!(current, thread::current().id());
                    assert_eq!(scan.next_sample, 1_000 + INITIAL_SAMPLE_STEPS);
                }
            })
            .join()
            .unwrap();
    });
    Ok(())
}

#[test]
fn quiet_activity_leaves_mapping_normal_and_backs_off_sampling() -> io::Result<()> {
    let map = mapped_fixture(&[0; 4096])?;
    // The counters cannot increase beyond these stored values. This keeps
    // the test independent of any incidental I/O by the test process.
    let mut scan = MappedScan::new(&map);
    scan.previous = Some((
        thread::current().id(),
        Activity {
            major_faults: u64::MAX,
            input_blocks: u64::MAX,
            wall: Instant::now(),
            cpu: Duration::ZERO,
        },
    ));
    scan.sample(INITIAL_SAMPLE_STEPS);
    assert!(!scan.sequential);
    assert!(!scan.admit_helpers());
    if scan.next_sample != u64::MAX {
        assert_eq!(scan.next_sample, INITIAL_SAMPLE_STEPS + SAMPLE_INTERVAL);
    }
    scan.enter_tile(0, 4096);
    assert!(scan.advised.is_none());
    Ok(())
}

#[test]
fn mapping_drop_fences_active_helpers_and_disarms_queued_helpers() -> io::Result<()> {
    use crate::source::Source;
    use std::sync::mpsc;

    let bytes = include_bytes!("../../../tests/fixtures/tiled.gol");
    let (source, writer) = fixture_with_writer(bytes)?;
    let weak_mmap = Arc::downgrade(&source.map);
    let address = source.as_bytes().unwrap().as_ptr() as usize;
    let weak_worker = source.worker_library().expect("valid immutable fixture");
    let worker = weak_worker.upgrade().unwrap();
    let (started, active) = mpsc::channel();
    let (resume, wait) = mpsc::channel();
    let (dropped, owner_finished) = mpsc::channel();
    thread::scope(|scope| {
        let view = worker.clone();
        scope.spawn(move || {
            view.with_library(|library| {
                assert_eq!(
                    library.source().as_bytes().unwrap().as_ptr() as usize,
                    address
                );
                started.send(()).unwrap();
                wait.recv().unwrap();
                assert_eq!(library.source().as_bytes().unwrap(), bytes);
            });
        });
        active.recv().unwrap();
        scope.spawn(move || {
            drop(source);
            dropped.send(()).unwrap();
        });
        while worker.active() {
            thread::yield_now();
        }
        assert!(owner_finished.try_recv().is_err());
        worker.with_library(|_| panic!("queued helper admitted during close"));
        resume.send(()).unwrap();
        owner_finished.recv().unwrap();
        worker.with_library(|_| panic!("queued helper admitted after the original map dropped"));
    });
    assert!(weak_mmap.upgrade().is_none());
    writer.set_len(0)?;
    assert_eq!(writer.metadata()?.len(), 0);
    Ok(())
}

#[test]
fn cheap_take_one_does_not_initialize_helpers() -> crate::Result<()> {
    let bytes = include_bytes!("../../../tests/fixtures/tiled.gol");
    let mapped = crate::Library::new(mapped_fixture(bytes)?)?;
    let query = crate::Query::all();
    let prepared = query.prepare(&mapped)?;
    let hits = prepared.iter().take(1).collect::<crate::Result<Vec<_>>>()?;
    assert_eq!(hits.len(), 1);
    assert!(mapped.source().helpers.as_ref().unwrap().0.get().is_none());
    Ok(())
}
