/// A positive exact-text test with a unique matching global value code.
#[derive(Clone, Copy)]
pub(crate) struct DictionaryEquality {
    pub(crate) key: u16,
    pub(crate) value: u16,
}

pub(crate) enum EqualityProbe {
    Reject,
    Match,
    Unknown,
}

pub(crate) trait DictionaryProbe: Copy {
    const TAG_PROBE_LIMIT: u64;

    // Prepared common-key lookups only visit the global table.
    fn probe(self, bytes: &[u8], strings: usize, max_tags: u64, offset: usize) -> EqualityProbe;
}

/// Two required global keys in increasing dictionary order.
#[derive(Clone, Copy)]
pub(crate) struct DictionaryPair {
    pub(crate) first: u16,
    pub(crate) second: u16,
}

impl DictionaryProbe for DictionaryPair {
    const TAG_PROBE_LIMIT: u64 = 16;

    #[inline(always)]
    fn probe(self, bytes: &[u8], strings: usize, max_tags: u64, offset: usize) -> EqualityProbe {
        let first = u32::from(self.first) << 2;
        let second = u32::from(self.second) << 2;
        let mut remaining = max_tags.min(Self::TAG_PROBE_LIMIT);
        let Some(mut tail) = bytes.get(offset..) else {
            return EqualityProbe::Unknown;
        };
        // The first checked lookup stops at an exact key, a larger key, or
        // the end of the global table. Its absence settles the conjunction.
        loop {
            if remaining == 0 {
                return EqualityProbe::Unknown;
            }
            remaining -= 1;
            let Some(word) = tail.first_chunk::<4>() else {
                return EqualityProbe::Unknown;
            };
            let entry = u32::from_le_bytes(*word);
            let key_and_last = entry & 0xfffc;
            if key_and_last & 0x7ffc == first {
                // The ordered evaluator still decodes this value even if the
                // second key is missing. Preserve errors from that lookup.
                match entry & 3 {
                    0 => {}
                    1 if ((entry >> 16) as usize) < strings => {}
                    2 if tail.len() >= 6 => {}
                    _ => return EqualityProbe::Unknown,
                }
                if entry & 0x8000 != 0 {
                    return EqualityProbe::Reject;
                }
                let step = 4 + (entry as usize & 2);
                let Some(next) = tail.get(step..) else {
                    return EqualityProbe::Unknown;
                };
                tail = next;
                break;
            }
            if key_and_last > first {
                return EqualityProbe::Reject;
            }
            let step = 4 + (entry as usize & 2);
            let Some(next) = tail.get(step..) else {
                return EqualityProbe::Unknown;
            };
            tail = next;
        }
        for _ in 0..remaining {
            let Some(word) = tail.first_chunk::<4>() else {
                return EqualityProbe::Unknown;
            };
            let entry = u32::from_le_bytes(*word);
            let key_and_last = entry & 0xfffc;
            if key_and_last & 0x7ffc == second {
                return EqualityProbe::Unknown;
            }
            if key_and_last > second {
                return EqualityProbe::Reject;
            }
            let step = 4 + (entry as usize & 2);
            let Some(next) = tail.get(step..) else {
                return EqualityProbe::Unknown;
            };
            tail = next;
        }
        EqualityProbe::Unknown
    }
}

pub(crate) trait DictionaryKey: Copy {
    fn key(self) -> u16;
    fn accepts(self, value: u16) -> bool;
    fn accepts_number(self) -> bool {
        false
    }
    // An inconclusive local test needs to read and decode the spelling.
    fn local_match(self) -> Option<bool> {
        None
    }
}

impl<K: DictionaryKey> DictionaryProbe for K {
    const TAG_PROBE_LIMIT: u64 = 16;

    #[inline(always)]
    fn probe(self, bytes: &[u8], strings: usize, max_tags: u64, offset: usize) -> EqualityProbe {
        probe_one_key(self, bytes, strings, max_tags, offset)
    }
}

/// A positive truthiness test with at most two falsy global value codes
/// (typically `""` and `"no"`), avoiding bitmap lookups on the hot count path.
#[derive(Clone, Copy)]
pub(crate) struct DictionaryTruthy {
    pub(crate) key: u16,
    pub(crate) falsy0: u32,
    pub(crate) falsy1: u32,
    pub(crate) local_match: Option<bool>,
}

impl DictionaryTruthy {
    pub(crate) fn from_global_matches(
        key: u16,
        values: &[u64],
        string_count: usize,
        local_match: Option<bool>,
    ) -> Option<Self> {
        let mut falsy = [u32::MAX; 2];
        let mut found = 0usize;
        for (group, &word) in values.iter().enumerate() {
            let start = group * 64;
            if start >= string_count {
                break;
            }
            let valid = (string_count - start).min(64);
            let mask = if valid == 64 {
                u64::MAX
            } else {
                (1u64 << valid) - 1
            };
            let mut zeros = !word & mask;
            while zeros != 0 {
                if found == 2 {
                    return None;
                }
                let bit = zeros.trailing_zeros() as usize;
                falsy[found] = (start + bit) as u32;
                found += 1;
                zeros &= zeros - 1;
            }
        }
        Some(Self {
            key,
            falsy0: falsy[0],
            falsy1: falsy[1],
            local_match,
        })
    }
}

impl DictionaryKey for DictionaryTruthy {
    #[inline(always)]
    fn key(self) -> u16 {
        self.key
    }

    #[inline(always)]
    fn accepts(self, value: u16) -> bool {
        let value = u32::from(value);
        value != self.falsy0 && value != self.falsy1
    }

    #[inline(always)]
    fn accepts_number(self) -> bool {
        true
    }

    #[inline(always)]
    fn local_match(self) -> Option<bool> {
        self.local_match
    }
}

impl DictionaryKey for DictionaryEquality {
    #[inline(always)]
    fn key(self) -> u16 {
        self.key
    }

    #[inline(always)]
    fn accepts(self, value: u16) -> bool {
        value == self.value
    }

    #[inline(always)]
    fn local_match(self) -> Option<bool> {
        Some(false)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct DictionarySet<'a> {
    pub(crate) key: u16,
    pub(crate) values: &'a [u64],
    pub(crate) numbers_match: bool,
    pub(crate) local_match: Option<bool>,
}

impl DictionaryKey for DictionarySet<'_> {
    #[inline(always)]
    fn key(self) -> u16 {
        self.key
    }

    #[inline(always)]
    fn accepts(self, value: u16) -> bool {
        let value = usize::from(value);
        self.values[value / 64] & (1 << (value % 64)) != 0
    }

    #[inline(always)]
    fn accepts_number(self) -> bool {
        self.numbers_match
    }

    #[inline(always)]
    fn local_match(self) -> Option<bool> {
        self.local_match
    }
}

pub(crate) struct DictionaryUnion {
    first: u16,
    last: u16,
    value: u16,
    key_mask: Box<[u64]>,
}

impl DictionaryUnion {
    pub(crate) fn from_tests(tests: &[DictionaryEquality]) -> Option<Self> {
        let value = tests.first()?.value;
        if tests.iter().any(|test| test.value != value) {
            return None;
        }
        let first = tests.iter().map(|test| test.key).min()?;
        let last = tests.iter().map(|test| test.key).max()?;
        if first == last {
            return None;
        }
        let mut key_mask = vec![0; usize::from(last - first) / 64 + 1];
        for test in tests {
            let bit = usize::from(test.key - first);
            key_mask[bit / 64] |= 1 << (bit % 64);
        }
        Some(Self {
            first,
            last,
            value,
            key_mask: key_mask.into_boxed_slice(),
        })
    }
}

impl DictionaryProbe for &DictionaryUnion {
    const TAG_PROBE_LIMIT: u64 = 12;

    #[inline(always)]
    fn probe(self, bytes: &[u8], strings: usize, max_tags: u64, offset: usize) -> EqualityProbe {
        use EqualityProbe::*;
        let Some(mut tail) = bytes.get(offset..) else {
            return Unknown;
        };
        let mut previous = 0;
        let mut matched = false;
        for _ in 0..max_tags.min(Self::TAG_PROBE_LIMIT) {
            let Some(word) = tail.first_chunk::<4>() else {
                return Unknown;
            };
            let entry = u32::from_le_bytes(*word);
            let key = ((entry >> 2) & 8191) as u16;
            let terminal = entry == 0x8001;
            if !terminal && key <= previous {
                return Unknown;
            }
            if !terminal && key <= self.last {
                let bit = usize::from(key.wrapping_sub(self.first));
                if key >= self.first && self.key_mask[bit / 64] & (1 << (bit % 64)) != 0 {
                    let value = (entry >> 16) as u16;
                    match entry & 3 {
                        0 => {}
                        1 if usize::from(value) < strings => matched |= value == self.value,
                        2 | 3 if tail.len() >= 6 => {}
                        _ => return Unknown,
                    }
                }
            }
            if terminal || key >= self.last || entry & 0x8000 != 0 {
                return if matched { Match } else { Reject };
            }
            previous = key;
            let step = 4 + (entry as usize & 2);
            let Some(next) = tail.get(step..) else {
                return Unknown;
            };
            tail = next;
        }
        Unknown
    }
}

/// Test checked global entries from a tile-bounded slice.
/// The full lookup handles short reads and invalid data.
#[inline(always)]
fn probe_one_key<K: DictionaryKey>(
    probe: K,
    bytes: &[u8],
    strings: usize,
    max_tags: u64,
    offset: usize,
) -> EqualityProbe {
    use EqualityProbe::*;
    let key = u32::from(probe.key()) << 2;
    // Only common keys (13 bits) can reach the prepared matcher.
    debug_assert!(key <= 0x7ffc);
    let Some(mut tail) = bytes.get(offset..) else {
        return Unknown;
    };
    // Limit speculative work. If the lookup budget runs out, the regular
    // decoder restarts at the table and reports its usual limit error.
    for _ in 0..max_tags.min(K::TAG_PROBE_LIMIT) {
        let Some(word) = tail.first_chunk::<4>() else {
            return Unknown;
        };
        let entry = u32::from_le_bytes(*word);
        let key_and_last = entry & 0xfffc;
        if key_and_last & 0x7ffc == key {
            if entry == 0x8001 {
                return Reject;
            }
            let value = (entry >> 16) as u16;
            let matched = match entry & 3 {
                0 => Some(probe.accepts_number()),
                1 if usize::from(value) < strings => Some(probe.accepts(value)),
                2 if tail.len() >= 6 => Some(probe.accepts_number()),
                // Check the entry's width without following its displacement.
                3 if tail.len() >= 6 => probe.local_match(),
                _ => None,
            };
            return match matched {
                Some(true) => Match,
                Some(false) => Reject,
                None => Unknown,
            };
        }
        // A larger encoded word implies either a larger sorted key or the
        // terminal entry (bit 15). The equal-key case was handled above.
        if key_and_last > key {
            return Reject;
        }
        let step = 4 + (entry as usize & 2);
        let Some(next) = tail.get(step..) else {
            return Unknown;
        };
        tail = next;
    }
    Unknown
}
