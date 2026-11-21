use super::*;
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    sync::atomic::{AtomicUsize, Ordering},
};

#[allow(unsafe_code)]
pub(crate) fn mapped_fixture(bytes: &[u8]) -> io::Result<MappedFile> {
    fixture_with_writer(bytes).map(|(mapped, _)| mapped)
}

#[allow(unsafe_code)]
fn fixture_with_writer(bytes: &[u8]) -> io::Result<(MappedFile, fs::File)> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "regol-mapped-scan-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)?;
    let mapping = (|| {
        file.write_all(bytes)?;
        // SAFETY: the fixture is complete and only this test knows its path.
        // The writer is used only after the owner drops. Unlinking below
        // leaves the mapped file contents intact.
        unsafe { MappedFile::open(&path) }
    })();
    let removed = fs::remove_file(path);
    mapping.and_then(|mapping| removed.map(|()| (mapping, file)))
}

pub(crate) fn force_helpers(scan: &mut MappedScan<'_>) {
    scan.helper_faults_observed = true;
}

pub(crate) fn force_sequential(scan: &mut MappedScan<'_>) {
    scan.sequential = true;
    scan.next_sample = u64::MAX;
}

pub(crate) fn sequential_at(map: &MappedFile, offset: usize) -> Option<bool> {
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
            return Some(flags.split_whitespace().any(|flag| flag == "sr"));
        }
    }
    None
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
