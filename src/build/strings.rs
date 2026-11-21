//! Compact string interning arena, precomputed per-string GOL metadata, and
//! a fast deterministic non-cryptographic hasher for internal builder maps.
use super::encode::Value;
use std::{
    collections::{HashMap, HashSet},
    hash::{BuildHasherDefault, Hasher},
};

/// Fast deterministic Fx-style hasher for internal integer and slice maps.
#[derive(Default)]
pub(super) struct FxHasher {
    hash: u64,
}

const FX_SEED: u64 = 0x517c_c1b7_2722_0a95;

impl FxHasher {
    #[inline]
    fn mix(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let word = u64::from_ne_bytes(bytes[..8].try_into().unwrap());
            self.mix(word);
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            let word = u32::from_ne_bytes(bytes[..4].try_into().unwrap());
            self.mix(u64::from(word));
            bytes = &bytes[4..];
        }
        if bytes.len() >= 2 {
            let word = u16::from_ne_bytes(bytes[..2].try_into().unwrap());
            self.mix(u64::from(word));
            bytes = &bytes[2..];
        }
        if let Some(&byte) = bytes.first() {
            self.mix(u64::from(byte));
        }
    }

    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.mix(u64::from(i));
    }

    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.mix(i);
    }

    #[inline]
    fn write_i64(&mut self, i: i64) {
        self.mix(i as u64);
    }

    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.mix(i as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub(super) type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub(super) type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;
pub(super) type FxHashSet<K> = HashSet<K, FxBuildHasher>;

#[inline]
pub(super) fn hash_str(bytes: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write(bytes);
    h.finish()
}

/// Dense string interner backed by a single contiguous UTF-8 buffer.
///
/// Each distinct string receives a dense `u32` ID (`0..len()`).
pub(super) struct StringPool {
    bytes: String,
    offsets: Vec<usize>,
    hashes: Vec<u64>,
    table: Vec<u32>, // open-addressing slots storing (id + 1), 0 = empty
}

impl StringPool {
    pub(super) fn new() -> Self {
        Self {
            bytes: String::new(),
            offsets: vec![0],
            hashes: Vec::new(),
            table: vec![0; 64],
        }
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        self.hashes.len()
    }

    #[inline]
    pub(super) fn get(&self, id: u32) -> &str {
        let i = id as usize;
        &self.bytes[self.offsets[i]..self.offsets[i + 1]]
    }

    pub(super) fn find(&self, s: &str) -> Option<u32> {
        let hash = hash_str(s.as_bytes());
        let mask = self.table.len() - 1;
        let mut slot = (hash as usize) & mask;
        loop {
            let entry = self.table[slot];
            if entry == 0 {
                return None;
            }
            let id = entry - 1;
            if self.hashes[id as usize] == hash && self.get(id) == s {
                return Some(id);
            }
            slot = (slot + 1) & mask;
        }
    }

    /// Intern `s`, returning `(id, is_new)`.
    pub(super) fn intern_ex(&mut self, s: &str) -> (u32, bool) {
        let hash = hash_str(s.as_bytes());
        let mask = self.table.len() - 1;
        let mut slot = (hash as usize) & mask;
        loop {
            let entry = self.table[slot];
            if entry == 0 {
                break;
            }
            let id = entry - 1;
            if self.hashes[id as usize] == hash && self.get(id) == s {
                return (id, false);
            }
            slot = (slot + 1) & mask;
        }
        let id = self.hashes.len() as u32;
        self.bytes.push_str(s);
        self.offsets.push(self.bytes.len());
        self.hashes.push(hash);
        if (self.hashes.len() * 4) >= (self.table.len() * 3) {
            self.grow();
        } else {
            self.table[slot] = id + 1;
        }
        (id, true)
    }

    #[inline]
    pub(super) fn intern(&mut self, s: &str) -> u32 {
        self.intern_ex(s).0
    }

    fn grow(&mut self) {
        let new_cap = (self.table.len() * 2).max(64);
        let mask = new_cap - 1;
        let mut table = vec![0u32; new_cap];
        for (id, &hash) in self.hashes.iter().enumerate() {
            let mut slot = (hash as usize) & mask;
            while table[slot] != 0 {
                slot = (slot + 1) & mask;
            }
            table[slot] = id as u32 + 1;
        }
        self.table = table;
    }

    /// Free the hash table once no further interning is needed.
    pub(super) fn shrink_lookup(&mut self) {
        self.hashes = Vec::new();
        self.table = Vec::new();
    }
}

/// Precomputed dictionary and numeric classification for a single interned string.
#[derive(Clone, Copy)]
pub(super) struct StrMeta {
    /// Global dictionary code, or `u16::MAX` if not in the global dictionary.
    pub dict_code: u16,
    /// Global dictionary key code (`1..8191`), or `u16::MAX` if local key.
    pub key_code: u16,
    /// Category index bitmask (`0` if not an indexed key).
    pub category_bit: u32,
    /// Pre-parsed numeric tag value kind: `0` = text, `1` = Narrow, `2` = Wide.
    pub number_kind: u8,
    /// Numeric payload (`u16` for Narrow, `u32` for Wide).
    pub number_val: u32,
}

impl StrMeta {
    #[inline]
    pub(super) fn number_value(self) -> Option<Value> {
        match self.number_kind {
            1 => Some(Value::Narrow(self.number_val as u16)),
            2 => Some(Value::Wide(self.number_val)),
            _ => None,
        }
    }

    #[inline]
    pub(super) fn has_dict_code(self) -> bool {
        self.dict_code != u16::MAX
    }

    #[inline]
    pub(super) fn has_key_code(self) -> bool {
        self.key_code != u16::MAX
    }
}
