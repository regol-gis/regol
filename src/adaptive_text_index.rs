use crate::text_index::TextTagIndex;
use crate::{Library, Types, source::Source};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

pub(crate) const DEFAULT_TEXT_INDEX_BYTES: usize = 16 * 1024 * 1024;
const MIN_STEPS: u64 = 100_000;
const REPEATS: u8 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScanKey {
    pub(crate) code: u16,
    pub(crate) types: Types,
}

#[derive(Default)]
struct RecentScans {
    candidates: [Option<(ScanKey, u8)>; 4],
    next: usize,
}

pub(crate) struct AdaptiveTextIndex {
    budget: usize,
    attempted: AtomicBool,
    recent: Mutex<RecentScans>,
    pub(crate) index: OnceLock<TextTagIndex>,
}

impl AdaptiveTextIndex {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            budget,
            attempted: AtomicBool::new(false),
            recent: Mutex::new(RecentScans::default()),
            index: OnceLock::new(),
        }
    }

    pub(crate) fn learning(&self) -> bool {
        self.budget != 0 && !self.attempted.load(Ordering::Acquire)
    }

    fn claim(&self, key: ScanKey, steps: u64) -> bool {
        if steps < MIN_STEPS || !self.learning() {
            return false;
        }
        let Ok(mut recent) = self.recent.lock() else {
            return false;
        };
        if !self.learning() {
            return false;
        }
        for slot in recent.candidates.iter_mut().flatten() {
            if slot.0 == key {
                slot.1 += 1;
                let claimed = slot.1 == REPEATS;
                if claimed {
                    // Claim before building. No other query waits for the build
                    // or retries if it fails or unwinds.
                    self.attempted.store(true, Ordering::Release);
                }
                return claimed;
            }
        }
        let next = recent.next;
        recent.candidates[next] = Some((key, 1));
        recent.next = (next + 1) % recent.candidates.len();
        false
    }
}

// The observer must see a successful end of traversal. Two yielded matches
// distinguish those completions from an empty stream requested with take(1).
pub(crate) struct ScanObservation<'a, 'q, S> {
    library: &'a Library<S>,
    key: ScanKey,
    name: &'q str,
}

impl<'a, 'q, S: Source> ScanObservation<'a, 'q, S> {
    pub(crate) fn new(library: &'a Library<S>, key: ScanKey, name: &'q str) -> Self {
        Self { library, key, name }
    }

    #[cold]
    pub(crate) fn finish(self, steps: u64, seen: u8) {
        let adaptive = &self.library.adaptive_text_index;
        if seen < 2 || !adaptive.claim(self.key, steps) {
            return;
        }
        if let Ok(index) =
            TextTagIndex::build(self.library, self.name, self.key.types, adaptive.budget)
        {
            let _ = adaptive.index.set(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Query, Result, decode::Decode};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn concurrent_scans_claim_only_one_attempt() {
        let index = AdaptiveTextIndex::new(DEFAULT_TEXT_INDEX_BYTES);
        let key = ScanKey {
            code: 1,
            types: Types::NODE,
        };
        assert!(!index.claim(key, MIN_STEPS - 1));
        assert!(!index.claim(key, MIN_STEPS));
        let attempts = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    if index.claim(key, MIN_STEPS) {
                        attempts.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
        assert!(!index.learning());
        assert!(index.index.get().is_none());
    }

    #[test]
    fn budget_failure_is_permanent_and_success_publishes_at_eof() -> Result<()> {
        let bytes = include_bytes!("../tests/fixtures/global.gol").as_slice();
        for cap in [1, DEFAULT_TEXT_INDEX_BYTES] {
            let mut library = Library::new(bytes)?;
            library.set_adaptive_text_index_bytes(cap);
            let key = ScanKey {
                code: library.dictionary().unwrap().key_code("building").unwrap(),
                types: Types::ALL,
            };
            let finish = |steps, seen| {
                ScanObservation::new(&library, key, "building").finish(steps, seen);
            };
            for _ in 0..4 {
                finish(MIN_STEPS, 1);
                finish(MIN_STEPS - 1, 2);
            }
            assert!(library.adaptive_text_index.learning());
            for _ in 0..REPEATS - 1 {
                finish(MIN_STEPS, 2);
                assert!(library.adaptive_text_index.learning());
                assert!(library.adaptive_text_index.index.get().is_none());
            }
            finish(MIN_STEPS, 2);
            assert!(!library.adaptive_text_index.learning());
            let published = library.adaptive_text_index.index.get();
            if cap == 1 {
                assert!(published.is_none());
                finish(MIN_STEPS, 2);
                assert!(library.adaptive_text_index.index.get().is_none());
            } else {
                let matches = published.unwrap().matching(&library, "yes");
                assert_eq!(matches.collect::<Result<Vec<_>>>()?.len(), 1);
            }
            library.set_adaptive_text_index_bytes(0);
            assert!(!library.adaptive_text_index.learning());
            assert!(library.adaptive_text_index.index.get().is_none());
        }
        Ok(())
    }

    #[test]
    fn learned_postings_respect_dictionary_value_encoding() -> Result<()> {
        let mut bytes = include_bytes!("../tests/fixtures/basic.gol").to_vec();
        let dictionary = bytes.u32(84)? as usize;
        let shop = dictionary
            + bytes[dictionary..]
                .windows(5)
                .position(|entry| entry == b"\x04shop")
                .unwrap()
            + 1;
        bytes[shop..shop + 4].copy_from_slice(b"cafe");

        let library = Library::new(bytes.as_slice())?;
        let cafe = Query::parse("n[amenity=cafe]")?;
        let bench = Query::parse("n[amenity=bench]")?;
        let cafe = cafe.prepare(&library)?;
        let bench = bench.prepare(&library)?;
        assert_eq!(cafe.count()?, 0);
        assert_eq!(bench.count()?, 1);
        assert!(bench.indexed_matches(None).is_none());
        for _ in 0..REPEATS {
            bench
                .scan_observation(None)
                .expect("fixture has an indexed amenity key")
                .finish(MIN_STEPS, 2);
        }
        assert!(cafe.indexed_matches(None).is_some());
        assert!(cafe.iter().next().is_none());
        assert_eq!(bench.iter().collect::<Result<Vec<_>>>()?.len(), 1);
        Ok(())
    }

    #[test]
    fn panic_does_not_hold_claim_lock_or_allow_retries() {
        let index = AdaptiveTextIndex::new(DEFAULT_TEXT_INDEX_BYTES);
        let key = ScanKey {
            code: 1,
            types: Types::NODE,
        };
        for _ in 0..REPEATS - 1 {
            assert!(!index.claim(key, MIN_STEPS));
        }
        let panic = std::panic::catch_unwind(|| {
            assert!(index.claim(key, MIN_STEPS));
            panic!("test unwind while holding the build claim");
        });
        assert!(panic.is_err());
        assert!(index.recent.try_lock().is_ok());
        assert!(!index.claim(key, MIN_STEPS));
    }
}
