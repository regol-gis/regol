//! A replaceable byte cache. No borrowed cache slices escape a read call.
#![forbid(unsafe_code)]

use super::{FileSource, Source};
use std::{fs::File, io, path::Path, sync::Mutex};

const PAGE_BYTES: usize = 4096;
const WAYS: usize = 4;
const EMPTY: u64 = u64::MAX;

#[derive(Default)]
struct Set {
    entries: [Entry; WAYS],
    recent: u8,
    hand: usize,
}
struct Entry {
    page: u64,
    bytes: Vec<u8>,
}
impl Default for Entry {
    fn default() -> Self {
        Self {
            page: EMPTY,
            bytes: Vec::new(),
        }
    }
}

struct Cache<S> {
    source: S,
    sets: Vec<Mutex<Set>>,
    shift: u32,
}
impl<S: Source> Cache<S> {
    fn new(source: S, capacity_bytes: usize) -> io::Result<Self> {
        let requested = capacity_bytes / (PAGE_BYTES * WAYS);
        if requested == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cache requires at least four pages",
            ));
        }
        let count = 1usize << requested.ilog2();
        let mut sets = Vec::new();
        sets.try_reserve_exact(count).map_err(io::Error::other)?;
        sets.resize_with(count, || Mutex::new(Set::default()));
        Ok(Self {
            source,
            sets,
            shift: 64 - count.ilog2(),
        })
    }
    #[inline]
    fn set_index(&self, page: u64) -> usize {
        // Multiplicative indexing avoids common power-of-two file strides
        // concentrating every read on the same set. A one-set cache needs no shift.
        if self.shift == 64 {
            0
        } else {
            (page.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> self.shift) as usize
        }
    }
    fn capacity_bytes(&self) -> usize {
        self.sets.len() * WAYS * PAGE_BYTES
    }
    fn allocated_bytes(&self) -> io::Result<usize> {
        self.sets.iter().try_fold(0usize, |total, set| {
            let set = set
                .lock()
                .map_err(|_| io::Error::other("cache lock poisoned"))?;
            Ok(total
                + set
                    .entries
                    .iter()
                    .map(|entry| entry.bytes.capacity())
                    .sum::<usize>())
        })
    }
}
impl<S: Source> Source for Cache<S> {
    #[inline]
    fn len(&self) -> u64 {
        self.source.len()
    }
    fn read_buffer_size(&self) -> usize {
        PAGE_BYTES
    }
    fn read_exact_at(&self, mut offset: u64, mut output: &mut [u8]) -> io::Result<()> {
        if offset
            .checked_add(output.len() as u64)
            .is_none_or(|end| end > self.len())
        {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        while !output.is_empty() {
            let page = offset / PAGE_BYTES as u64;
            let within = (offset % PAGE_BYTES as u64) as usize;
            let mut set = self.sets[self.set_index(page)]
                .lock()
                .map_err(|_| io::Error::other("cache lock poisoned"))?;
            let slot = if let Some(index) = set.entries.iter().position(|entry| entry.page == page)
            {
                index
            } else {
                while set.recent & (1 << set.hand) != 0 {
                    set.recent &= !(1 << set.hand);
                    set.hand = (set.hand + 1) % WAYS;
                }
                let index = set.hand;
                set.hand = (set.hand + 1) % WAYS;
                let entry = &mut set.entries[index];
                // Invalidate before a possibly partial/failed read. Reuse the
                // allocation so continued replacement does not allocate pages.
                entry.page = EMPTY;
                let base = page * PAGE_BYTES as u64;
                let len = (self.len() - base).min(PAGE_BYTES as u64) as usize;
                if len > entry.bytes.len() {
                    entry
                        .bytes
                        .try_reserve_exact(len - entry.bytes.len())
                        .map_err(io::Error::other)?;
                }
                entry.bytes.resize(len, 0);
                self.source.read_exact_at(base, &mut entry.bytes)?;
                entry.page = page;
                index
            };
            set.recent |= 1 << slot;
            let bytes = &set.entries[slot].bytes;
            let count = output.len().min(bytes.len() - within);
            output[..count].copy_from_slice(&bytes[within..within + count]);
            output = &mut output[count..];
            offset += count as u64;
        }
        Ok(())
    }
}

/// Positioned I/O with a bounded, replaceable cache of copied 4 KiB pages.
///
/// Cache slices never escape a read. Decoders use their usual iterator-owned
/// windows and returned local strings remain owned. Stable file contents are
/// required for consistent results. The requested capacity bounds page payload;
/// cache metadata, iterator buffers and library/query data are additional.
pub struct CachedFile(Cache<FileSource>);
impl CachedFile {
    /// Open a file with a bounded page cache. See [`Self::from_file`] for capacity rounding.
    pub fn open(path: impl AsRef<Path>, capacity_bytes: usize) -> io::Result<Self> {
        Self::from_file(File::open(path)?, capacity_bytes)
    }
    /// Transfer an existing file handle and round capacity down to a power-of-two
    /// number of four-page sets. At least 16 KiB must be requested.
    pub fn from_file(file: File, capacity_bytes: usize) -> io::Result<Self> {
        Ok(Self(Cache::new(
            FileSource::from_file(file)?,
            capacity_bytes,
        )?))
    }
    /// Return the effective capacity in bytes of cached page payload.
    pub fn capacity_bytes(&self) -> usize {
        self.0.capacity_bytes()
    }
    /// Allocated page payload, including buffers retained after a failed fill.
    /// This locks each set and is intended for diagnostics outside query timing.
    pub fn allocated_bytes(&self) -> io::Result<usize> {
        self.0.allocated_bytes()
    }
}
impl Source for CachedFile {
    #[inline]
    fn len(&self) -> u64 {
        self.0.len()
    }
    #[inline]
    fn read_buffer_size(&self) -> usize {
        PAGE_BYTES
    }
    #[inline]
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        self.0.read_exact_at(offset, output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Observed {
        bytes: Vec<u8>,
        calls: AtomicUsize,
        fail: AtomicBool,
    }
    impl Source for Observed {
        fn len(&self) -> u64 {
            self.bytes.len() as u64
        }
        fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail.load(Ordering::Relaxed) {
                output.fill(0xee);
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.bytes.read_exact_at(offset, output)
        }
    }
    fn source() -> Observed {
        Observed {
            bytes: (0..PAGE_BYTES * 41 + 13).map(|i| (i % 251) as u8).collect(),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
        }
    }
    #[test]
    fn boundaries_churn_and_payload_bound() -> io::Result<()> {
        for requested in [
            PAGE_BYTES * WAYS,
            PAGE_BYTES * WAYS * 3,
            PAGE_BYTES * WAYS * 8 + 1,
        ] {
            let cache = Cache::new(source(), requested)?;
            assert!(cache.capacity_bytes() <= requested);
            assert_eq!(cache.allocated_bytes()?, 0);
            let mut first = [0; 16];
            cache.read_exact_at(0, &mut first)?;
            let calls = cache.source.calls.load(Ordering::Relaxed);
            first.fill(0xff);
            cache.read_exact_at(0, &mut first)?;
            assert_eq!(first, cache.source.bytes[..16]);
            assert_eq!(cache.source.calls.load(Ordering::Relaxed), calls);
            for turn in 0..4 {
                for page in (0..41).rev() {
                    let offset = (page * PAGE_BYTES + turn * 3) as u64;
                    let mut actual = [0; 137];
                    cache.read_exact_at(offset, &mut actual)?;
                    assert_eq!(
                        actual,
                        cache.source.bytes[offset as usize..offset as usize + actual.len()]
                    );
                }
            }
            for offset in [PAGE_BYTES - 3, PAGE_BYTES * 2 - 1, PAGE_BYTES * 40 + 12] {
                let mut actual = [0; 4097];
                cache.read_exact_at(offset as u64, &mut actual)?;
                assert_eq!(actual, cache.source.bytes[offset..offset + actual.len()]);
            }
            assert!(cache.allocated_bytes()? <= cache.capacity_bytes());
            let calls = cache.source.calls.load(Ordering::Relaxed);
            assert!(cache.read_exact_at(cache.len() - 1, &mut [0; 2]).is_err());
            assert!(cache.read_exact_at(u64::MAX, &mut [0; 2]).is_err());
            assert!(cache.read_exact_at(cache.len() + 1, &mut []).is_err());
            cache.read_exact_at(cache.len(), &mut [])?;
            assert_eq!(cache.source.calls.load(Ordering::Relaxed), calls);
        }
        Ok(())
    }
    #[test]
    fn failed_fill_cannot_become_a_cache_hit() -> io::Result<()> {
        let cache = Cache::new(source(), PAGE_BYTES * WAYS)?;
        let mut actual = [0; 24];
        for page in 0..WAYS {
            cache.read_exact_at((page * PAGE_BYTES) as u64, &mut actual)?;
        }
        let allocated = cache.allocated_bytes()?;
        cache.source.fail.store(true, Ordering::Relaxed);
        for _ in 0..2 {
            assert!(
                cache
                    .read_exact_at((PAGE_BYTES * 9) as u64, &mut actual)
                    .is_err()
            );
        }
        cache.source.fail.store(false, Ordering::Relaxed);
        cache.read_exact_at((PAGE_BYTES * 9) as u64, &mut actual)?;
        assert_eq!(
            actual,
            cache.source.bytes[PAGE_BYTES * 9..PAGE_BYTES * 9 + 24]
        );
        // Old page contents must also reload correctly after a failed replacement.
        cache.read_exact_at(0, &mut actual)?;
        assert_eq!(actual, cache.source.bytes[..24]);
        assert_eq!(cache.allocated_bytes()?, allocated);
        Ok(())
    }
    #[test]
    fn parallel_readers_replace_pages_without_borrowed_cache_views() -> io::Result<()> {
        let cache = Cache::new(source(), PAGE_BYTES * WAYS)?;
        assert!(cache.view(0, 1)?.is_none());
        assert!(cache.segment(0)?.is_none());
        std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|thread| {
                    scope.spawn({
                        let cache = &cache;
                        move || -> io::Result<()> {
                            for turn in 0..1000 {
                                let offset = ((turn * 7919 + thread * 31)
                                    % (cache.source.bytes.len() - 73))
                                    as u64;
                                let mut actual = [0; 73];
                                cache.read_exact_at(offset, &mut actual)?;
                                assert_eq!(
                                    actual,
                                    cache.source.bytes[offset as usize..offset as usize + 73]
                                );
                            }
                            Ok(())
                        }
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                handle.join().unwrap()?;
            }
            Ok::<_, io::Error>(())
        })?;
        assert_eq!(cache.allocated_bytes()?, cache.capacity_bytes());
        Ok(())
    }
    #[test]
    fn invalid_capacity_and_poisoned_locks_fail() -> io::Result<()> {
        for capacity in [0, 1, PAGE_BYTES * WAYS - 1] {
            assert!(Cache::new(source(), capacity).is_err());
        }
        let cache = Cache::new(source(), PAGE_BYTES * WAYS)?;
        let _ = std::panic::catch_unwind(|| {
            let _guard = cache.sets[0].lock().unwrap();
            panic!("poison");
        });
        assert!(cache.read_exact_at(0, &mut [0; 1]).is_err());
        assert!(cache.allocated_bytes().is_err());
        Ok(())
    }

    #[test]
    fn cached_mutations_preserve_successes_and_errors() -> crate::Result<()> {
        use crate::{Library, Limits, Query};
        let original = include_bytes!("../../tests/fixtures/tiled_global.gol");
        let mut state = 0xe17b_a923_5084_6c31u64;
        let queries = [
            Query::all(),
            Query::parse("*[name]")?,
            Query::parse("*[!amenity]")?,
        ];
        for iteration in 0..128 {
            let mut bytes = original.to_vec();
            if iteration != 0 {
                for _ in 0..4 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    let at = state as usize % bytes.len();
                    bytes[at] ^= (state >> 32) as u8;
                }
            }
            let limits = Limits {
                max_traversal_steps: 5000,
                max_tree_depth: 16,
                max_tags: 100,
                ..Limits::default()
            };
            let reference = Library::with_limits(bytes.as_slice(), limits);
            let cached =
                Library::with_limits(Cache::new(bytes.as_slice(), PAGE_BYTES * WAYS)?, limits);
            assert_eq!(reference.is_err(), cached.is_err());
            let (Ok(reference), Ok(cached)) = (reference, cached) else {
                continue;
            };
            for query in &queries {
                let expected = query.prepare(&reference).and_then(|prepared| {
                    prepared
                        .iter()
                        .map(|f| f.map(|f| f.id()))
                        .collect::<crate::Result<Vec<_>>>()
                });
                let actual = query.prepare(&cached).and_then(|prepared| {
                    prepared
                        .iter()
                        .map(|f| f.map(|f| f.id()))
                        .collect::<crate::Result<Vec<_>>>()
                });
                assert_eq!(expected.is_err(), actual.is_err());
                if let (Ok(expected), Ok(actual)) = (expected, actual) {
                    assert_eq!(expected, actual);
                }
            }
            assert!(cached.source().allocated_bytes()? <= PAGE_BYTES * WAYS);
        }
        Ok(())
    }
}
