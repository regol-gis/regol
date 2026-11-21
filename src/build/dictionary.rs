//! Choose frequently reused strings across the entire extract. Exact key counts
//! also tell us which spellings must fit in the smaller tag-key code space.
use super::{
    BuildOptions,
    encode::{Value, number},
    strings::{FxHashMap, StrMeta, StringPool},
};
use std::cmp::Reverse;

const KEY_LIMIT: usize = 8191; // key codes 1 through 8190
const STRING_LIMIT: usize = 32768; // also usable in a 15-bit relation role
// A GOL has 30 category bits. Related keys can safely share a bit: a
// candidate still has to satisfy the actual tag predicate.
const INDEX_GROUPS: &[&[&str]] = &[
    &["aerialway"],
    &["aeroway"],
    &["amenity"],
    &["boundary"],
    &["building"],
    &["building:part"],
    &["communication", "telecom"],
    &["craft"],
    &["emergency", "healthcare"],
    &["geological"],
    &["maxspeed"],
    &["highway"],
    &["historic"],
    &["industrial"],
    &["landuse"],
    &["leisure"],
    &["man_made"],
    &["military"],
    &["natural"],
    &["office"],
    &["place"],
    &["power"],
    &["railway"],
    &["route"],
    &["shop"],
    &["bridge", "tunnel"],
    &["tourism"],
    &["waterway"],
    &["name"],
    &["access"],
];

pub(super) struct Dictionary {
    pub words: Vec<String>,
    pub schema: Vec<(u16, u16)>,
    codes: FxHashMap<String, u16>,
    masks: FxHashMap<u16, u32>,
}

impl Dictionary {
    /// Build the global string dictionary and category schema from merged
    /// `(key_uses, total_uses)` frequency counts indexed by `StringPool` ID.
    pub(super) fn build(pool: &StringPool, freqs: &[(u64, u64)], options: BuildOptions) -> Self {
        let mut dictionary = Self {
            words: Vec::new(),
            schema: Vec::new(),
            codes: FxHashMap::default(),
            masks: FxHashMap::default(),
        };
        for word in [
            "",
            "no",
            "yes",
            "outer",
            "inner",
            "geodesk:duplicate",
            "geodesk:orphan",
            "geodesk:missing_members",
        ] {
            dictionary.add(word.to_owned());
        }
        for (index, keys) in INDEX_GROUPS.iter().enumerate() {
            for &key in *keys {
                let code = dictionary.add(key.to_owned());
                dictionary.schema.push((code, index as u16 + 1));
                dictionary.masks.insert(code, 1 << index);
            }
        }
        let required_words = dictionary.words.len();
        let mut optional_uses: Vec<u64> = Vec::new();

        // A spelling used even once as a key needs a key-sized code. Rank it
        // by all uses so a rare key such as "trunk" can still encode the
        // many highway=trunk values in the same file.
        let mut key_candidates: Vec<(u32, u64, u64)> = freqs
            .iter()
            .enumerate()
            .filter_map(|(id, &(keys, uses))| {
                (keys > 0 && uses >= 3).then_some((id as u32, keys, uses))
            })
            .collect();
        key_candidates.sort_unstable_by(|a, b| {
            b.2.cmp(&a.2)
                .then_with(|| b.1.cmp(&a.1))
                .then_with(|| pool.get(a.0).cmp(pool.get(b.0)))
        });
        key_candidates.truncate(8190);
        for (id, _, uses) in key_candidates {
            if dictionary.words.len() == KEY_LIMIT {
                break;
            }
            let before = dictionary.words.len();
            dictionary.add(pool.get(id).to_owned());
            if dictionary.words.len() > before {
                optional_uses.push(uses);
            }
        }

        let mut value_candidates: Vec<(u32, u64, u64)> = freqs
            .iter()
            .enumerate()
            .filter_map(|(id, &(keys, uses))| (uses >= 4).then_some((id as u32, keys, uses)))
            .collect();
        value_candidates.sort_unstable_by(|a, b| {
            Reverse(a.2)
                .cmp(&Reverse(b.2))
                .then_with(|| pool.get(a.0).cmp(pool.get(b.0)))
        });
        for (id, keys, uses) in value_candidates {
            if dictionary.words.len() >= STRING_LIMIT {
                break;
            }
            let word = pool.get(id);
            // Never promote a key spelling to a value-only code (issue #2).
            if (dictionary.words.len() < KEY_LIMIT || keys == 0)
                && match number(word) {
                    Some(Value::Wide(_)) => uses >= 300,
                    Some(_) => false,
                    None => true,
                }
            {
                let before = dictionary.words.len();
                dictionary.add(word.to_owned());
                if dictionary.words.len() > before {
                    optional_uses.push(uses);
                }
            }
        }

        if options.compact_dictionary {
            dictionary.compact(required_words, &optional_uses);
        }

        // A search for bridge:structure, for example, can prune roots via
        // the same bit as bridge. A subkey can also occur without its base.
        for (id, &(keys, _)) in freqs.iter().enumerate() {
            if keys == 0 {
                continue;
            }
            let word = pool.get(id as u32);
            let Some((base @ ("bridge" | "tunnel" | "maxspeed" | "access"), _)) =
                word.split_once(':')
            else {
                continue;
            };
            let Some(code) = dictionary
                .code(word)
                .filter(|&code| usize::from(code) < KEY_LIMIT)
            else {
                continue;
            };
            let bit = dictionary.masks[&dictionary.codes[base]];
            dictionary
                .schema
                .push((code, bit.trailing_zeros() as u16 + 1));
            dictionary.masks.insert(code, bit);
        }
        dictionary.schema.sort_unstable();
        dictionary
    }

    // Filter the default catalog after enforcing both code limits. This keeps
    // numeric tag types and their area rules identical even at dictionary
    // capacity, including decimal spellings also used as rare keys.
    fn compact(&mut self, required: usize, optional_uses: &[u64]) {
        let optional = self.words.split_off(required);
        self.codes.retain(|_, code| usize::from(*code) < required);
        for (word, &uses) in optional.into_iter().zip(optional_uses) {
            if number(&word).is_some() || uses >= 300 {
                self.add(word);
            }
        }
    }

    fn add(&mut self, word: String) -> u16 {
        if let Some(code) = self.codes.get(&word) {
            return *code;
        }
        let code = self.words.len() as u16;
        self.codes.insert(word.clone(), code);
        self.words.push(word);
        code
    }

    pub(super) fn code(&self, word: &str) -> Option<u16> {
        self.codes.get(word).copied()
    }

    #[cfg(test)]
    pub(super) fn key_code(&self, word: &str) -> Option<u16> {
        self.code(word)
            .filter(|&c| c != 0 && usize::from(c) < KEY_LIMIT)
    }

    pub(super) fn str_meta(&self, word: &str) -> StrMeta {
        let dict_code = self.code(word).unwrap_or(u16::MAX);
        let key_code = if dict_code != 0 && usize::from(dict_code) < KEY_LIMIT {
            dict_code
        } else {
            u16::MAX
        };
        let category_bit = if dict_code != u16::MAX {
            self.masks.get(&dict_code).copied().unwrap_or(0)
        } else {
            0
        };
        let (number_kind, number_val) = match number(word) {
            Some(Value::Narrow(n)) => (1, u32::from(n)),
            Some(Value::Wide(w)) => (2, w),
            _ => (0, 0),
        };
        StrMeta {
            dict_code,
            key_code,
            category_bit,
            number_kind,
            number_val,
        }
    }

    pub(super) fn build_meta(&self, pool: &StringPool) -> Vec<StrMeta> {
        (0..pool.len())
            .map(|id| self.str_meta(pool.get(id as u32)))
            .collect()
    }
}

#[inline]
pub(super) fn is_countable_word(word: &str) -> bool {
    // A narrow integer is already as compact as a dictionary code.
    !word.is_empty() && !matches!(number(word), Some(Value::Narrow(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::{Tags, stage::RawStage};
    use anyhow::Result;

    #[test]
    fn value_only_codes_never_shadow_a_local_key() -> Result<()> {
        let mut stage = RawStage::new();
        let mut tags = Tags {
            keys: (0..KEY_LIMIT).map(|i| format!("key{i}")).collect(),
            values: vec!["common value".into(); KEY_LIMIT],
        };
        // This spelling is eligible as a key, with far fewer than 300 uses.
        // Keeping it preserves the text type for values with the same spelling.
        tags.keys[0] = "4.7".into();
        tags.values[3] = "4.7".into();
        tags.values[0] = "trunk".into();
        tags.values[1] = "123".into();
        for id in 1..=4 {
            if id <= 3 {
                tags.values[2] = "rare key".into();
            } else {
                tags.values[2] = "common value".into();
            }
            stage.add_node(id, 0.0, 0.0, &tags)?;
        }
        stage.add_node(
            5,
            0.0,
            0.0,
            &Tags {
                keys: vec!["trunk".into(), "rare key".into()],
                values: vec!["123".into(), "123".into()],
            },
        )?;
        let dictionary = stage.build_dictionary(BuildOptions::default());
        assert!(usize::from(dictionary.code("common value").unwrap()) >= KEY_LIMIT);
        assert!(usize::from(dictionary.code("trunk").unwrap()) < KEY_LIMIT);
        assert_eq!(dictionary.code("rare key"), None);
        assert_eq!(dictionary.code("123"), None);
        assert!(dictionary.code("4.7").is_some());
        let compact = stage.build_dictionary(BuildOptions {
            compact_dictionary: true,
            ..Default::default()
        });
        assert!(compact.words.len() < dictionary.words.len());
        assert!(compact.key_code("4.7").is_some());
        assert!(compact.code("common value").is_some());
        for word in ["trunk", "rare key", "123"] {
            assert_eq!(compact.code(word), None);
        }
        Ok(())
    }
}
