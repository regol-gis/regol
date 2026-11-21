use regol::{FeatureType, Library, Result};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct CountingAllocator;
static ENABLED: AtomicBool = AtomicBool::new(false);
static COUNT: AtomicUsize = AtomicUsize::new(0);
// SAFETY: All allocation/deallocation operations are forwarded unchanged to
// System. The counter records allocations without accessing their memory.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: The caller provides the layout required by GlobalAlloc.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: The caller provides a live allocation and its original layout.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if ENABLED.load(Ordering::Relaxed) {
            COUNT.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: The caller guarantees the pointer/layout/reallocation contract.
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn borrowed_feature_access_allocates_nothing() -> Result<()> {
    let lib = Library::new(include_bytes!("fixtures/stress.gol").as_slice())?;
    let features: Vec<_> = lib.features().collect::<Result<_>>()?;
    COUNT.store(0, Ordering::SeqCst);
    ENABLED.store(true, Ordering::SeqCst);
    let result = (|| -> Result<()> {
        for f in features {
            std::hint::black_box(f.bounds()?);
            for tag in f.tags()? {
                std::hint::black_box(tag?);
            }
            for parent in f.parent_relations()? {
                std::hint::black_box(parent?);
            }
            if f.kind() == FeatureType::Way {
                for p in f.coordinates()? {
                    std::hint::black_box(p?);
                }
                for n in f.way_nodes()? {
                    std::hint::black_box(n?);
                }
            }
            if f.kind() == FeatureType::Relation {
                for m in f.members()? {
                    std::hint::black_box(m?);
                }
            }
        }
        Ok(())
    })();
    ENABLED.store(false, Ordering::SeqCst);
    result?;
    assert_eq!(
        COUNT.load(Ordering::SeqCst),
        0,
        "borrowed field traversal allocated"
    );

    // Reuse the existing test-only counter for owned way geometry. Keep this
    // in the same test so concurrent tests cannot overlap counting windows.
    let lib = Library::new(include_bytes!("fixtures/geometry.gol").as_slice())?;
    let ways: Vec<_> = lib
        .features()
        .filter_map(|feature| match feature {
            Ok(feature) if feature.kind() == FeatureType::Way => Some(Ok(feature)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<_>>()?;
    assert!(!ways.is_empty());
    for feature in &ways {
        let count = feature.coordinates()?.collect::<Result<Vec<_>>>()?.len();
        assert!((1..=1024).contains(&count));
    }
    let expected = ways.len();
    COUNT.store(0, Ordering::SeqCst);
    ENABLED.store(true, Ordering::SeqCst);
    let result = (|| -> Result<()> {
        for feature in ways {
            std::hint::black_box(feature.geometry()?);
        }
        Ok(())
    })();
    ENABLED.store(false, Ordering::SeqCst);
    result?;
    assert_eq!(
        COUNT.load(Ordering::SeqCst),
        expected,
        "small way geometry should allocate its coordinate buffer once"
    );
    compare_windowed_query_allocations()?;
    bounded_cache_replacement_reuses_allocations()?;
    Ok(())
}

fn bounded_cache_replacement_reuses_allocations() -> Result<()> {
    use regol::source::{CachedFile, Source};
    let path = std::env::temp_dir().join(format!("gol-cache-alloc-{}", std::process::id()));
    std::fs::write(&path, vec![73; 4096 * 32])?;
    let source = CachedFile::open(&path, 16 * 1024)?;
    let mut output = [0; 17];
    for page in 0..4 {
        source.read_exact_at(page * 4096, &mut output)?;
    }
    COUNT.store(0, Ordering::SeqCst);
    ENABLED.store(true, Ordering::SeqCst);
    let result = (|| -> Result<()> {
        for turn in 0..1024 {
            source.read_exact_at((turn % 32) * 4096, &mut output)?;
            std::hint::black_box(output);
        }
        Ok(())
    })();
    ENABLED.store(false, Ordering::SeqCst);
    result?;
    assert_eq!(
        COUNT.load(Ordering::SeqCst),
        0,
        "filled cache replacement allocated"
    );
    assert_eq!(source.allocated_bytes()?, 16 * 1024);
    assert_eq!(output, [73; 17]);
    drop(source);
    std::fs::remove_file(path)?;
    Ok(())
}

fn compare_windowed_query_allocations() -> Result<()> {
    use regol::{
        Query,
        source::{FileSource, Source, WindowedFile},
    };
    fn count<S: Source>(source: S) -> Result<(usize, u64, usize)> {
        let library = Library::new(source)?;
        // A wildcard consumes local text, so the window can serve the name bytes.
        let query = Query::parse("*[name=*]")?;
        let prepared = query.prepare(&library)?;
        COUNT.store(0, Ordering::SeqCst);
        ENABLED.store(true, Ordering::SeqCst);
        let result = (|| -> Result<(usize, u64)> {
            let mut count = 0;
            let mut ids = 0u64;
            for feature in prepared.iter() {
                let feature = std::hint::black_box(feature?);
                count += 1;
                ids = ids.wrapping_add(feature.id().id);
            }
            Ok((count, ids))
        })();
        ENABLED.store(false, Ordering::SeqCst);
        let (count, ids) = result?;
        Ok((count, ids, COUNT.load(Ordering::SeqCst)))
    }
    let path = "tests/fixtures/wide_exports.gol";
    let plain = count(FileSource::open(path)?)?;
    let windowed = count(WindowedFile::open(path)?)?;
    assert!(plain.0 > 0);
    assert_eq!((plain.0, plain.1), (windowed.0, windowed.1));
    assert!(
        windowed.2 < plain.2,
        "buffering should avoid per-name temporary allocations"
    );
    println!(
        "wide_exports name query: {} features; {} plain-file allocations, {} windowed allocations",
        plain.0, plain.2, windowed.2
    );
    Ok(())
}
