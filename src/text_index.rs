use crate::{
    Error, Feature, Library, Result, Types,
    feature::{EncodedValue, TileExtent},
    source::Source,
    spatial::{DictionaryKey, Features},
};
use std::{cell::Cell, collections::BTreeMap};

#[derive(Clone, Copy)]
struct TextKey<'a> {
    code: u16,
    matched_value: &'a Cell<u16>,
}
impl DictionaryKey for TextKey<'_> {
    #[inline(always)]
    fn key(self) -> u16 {
        self.code
    }

    #[inline(always)]
    fn accepts(self, value: u16) -> bool {
        self.matched_value.set(value);
        true
    }
}

#[derive(Clone, Copy)]
struct TileRun {
    tile: u32,
    // First feature in this tile, relative to the posting's offset array.
    start: u32,
}

#[derive(Default)]
struct PostingBuilder {
    offsets: Vec<u32>,
    runs: Vec<TileRun>,
}

struct Posting {
    value: Box<str>,
    offsets: Box<[u32]>,
    runs: Box<[TileRun]>,
}

/// A reusable in-memory index of the text tags for one key and set of types.
///
/// Construction completes a checked national traversal before publishing
/// any postings. Keep the underlying library contents stable for the life
/// of the index. Matches retain traversal order and use exact text equality.
pub(crate) struct TextTagIndex {
    retained: usize,
    key: Box<str>,
    types: Types,
    postings: Box<[Posting]>,
    tiles: Box<[TileExtent]>,
}

impl TextTagIndex {
    /// Build postings for the text values an exact GOQL operand can match for `key`.
    /// The limit applies to
    /// [retained bytes](Self::retained_bytes). Build-time allocations may use
    /// more space. Traversal and tag limits on the library also apply.
    pub(crate) fn build<S: Source>(
        library: &Library<S>,
        key: &str,
        types: Types,
        max_bytes: usize,
    ) -> Result<Self> {
        let dictionary = library.dictionary()?;
        let code = dictionary.key_code(key);
        let category = code
            .and_then(|code| library.categories.iter().find(|(key, _)| *key == code))
            .map_or(0, |(_, category)| *category);
        let mut features = Features::new(library, None).restrict(types.0, category);
        let mut by_value: BTreeMap<String, usize> = BTreeMap::new();
        // Several dictionary codes can spell the same text. Resolve each
        // code to the shared posting slot on its first occurrence.
        const NO_SLOT: usize = usize::MAX;
        let mut dictionary_slots = vec![NO_SLOT; library.string_count];
        let mut slots: Vec<PostingBuilder> = Vec::new();
        let matched_value = Cell::new(0);
        let mut tiles: Vec<TileExtent> = Vec::new();
        let mut retained = key.len();
        if retained > max_bytes {
            return Err(Error::Limit("text tag index bytes"));
        }
        let mut insert = |feature: Feature<'_, S>, tags: &mut _, strings: &mut _, leading: bool| {
            if !types.matches(feature) {
                return Ok(false);
            }
            let encoded = if leading {
                Some(EncodedValue::Global(matched_value.get()))
            } else if code.is_some() {
                feature.encoded_tag(key, code, tags, strings)?
            } else {
                feature.encoded_local_tag(key, tags, strings)?
            };
            let dictionary_slot = match encoded {
                Some(EncodedValue::Global(value)) => dictionary_slots.get_mut(usize::from(value)),
                _ => None,
            };
            let mut added_bytes = 0;
            let id = match dictionary_slot.as_deref().copied() {
                Some(id) if id != NO_SLOT => id,
                _ => {
                    let value = match encoded {
                        Some(EncodedValue::Global(code)) => library.global_string(code)?,
                        Some(EncodedValue::Local { field, delta }) => {
                            let offset = feature.local_value_target(field, delta)?;
                            strings.seek(offset)?;
                            let value = feature.local_string_window(strings, offset)?;
                            // An exact operand with a global code only matches
                            // values stored with a global code.
                            if dictionary.has_global_value(&value) {
                                return Ok(false);
                            }
                            value
                        }
                        _ => return Ok(false),
                    };
                    let id = if let Some(&id) = by_value.get(value.as_ref()) {
                        id
                    } else {
                        added_bytes = size_of::<Posting>() + value.len();
                        let id = slots.len();
                        slots.push(PostingBuilder::default());
                        by_value.insert(value.into_owned(), id);
                        id
                    };
                    if let Some(slot) = dictionary_slot {
                        *slot = id;
                    }
                    id
                }
            };

            let new_tile = tiles.last().is_none_or(|tile| {
                tile.start() != feature.tile_start() || tile.end() != feature.tile_end()
            });
            let tile_index = if new_tile {
                tiles.len()
            } else {
                tiles.len() - 1
            };
            let tile =
                u32::try_from(tile_index).map_err(|_| Error::Limit("text tag index tiles"))?;
            let posting = &mut slots[id];
            let new_run = posting.runs.last().is_none_or(|run| run.tile != tile);
            let bytes = size_of::<u32>()
                + added_bytes
                + if new_tile { size_of::<TileExtent>() } else { 0 }
                + if new_run { size_of::<TileRun>() } else { 0 };
            retained = retained
                .checked_add(bytes)
                .filter(|&bytes| bytes <= max_bytes)
                .ok_or(Error::Limit("text tag index bytes"))?;

            if new_tile {
                tiles.push(feature.tile);
            }
            if new_run {
                let start = u32::try_from(posting.offsets.len())
                    .map_err(|_| Error::Limit("text tag index posting length"))?;
                posting.runs.push(TileRun { tile, start });
            }
            // Feature construction validates the anchor and tile extent.
            posting
                .offsets
                .push((feature.anchor() - feature.tile_start()) as u32);
            Ok(false)
        };
        let result = if let Some(code) = code {
            features.next_equality(
                &mut insert,
                || Ok(true),
                TextKey {
                    code,
                    matched_value: &matched_value,
                },
            )
        } else {
            features.next_matching(|feature, tags, strings| insert(feature, tags, strings, false))
        };
        result.transpose()?;
        Ok(Self {
            retained,
            key: key.into(),
            types,
            tiles: tiles.into_boxed_slice(),
            postings: by_value
                .into_iter()
                .map(|(value, id)| {
                    let PostingBuilder { offsets, runs } = std::mem::take(&mut slots[id]);
                    Posting {
                        value: value.into_boxed_str(),
                        offsets: offsets.into_boxed_slice(),
                        runs: runs.into_boxed_slice(),
                    }
                })
                .collect(),
        })
    }

    fn posting(&self, value: &str) -> Option<&Posting> {
        let index = self
            .postings
            .binary_search_by(|posting| posting.value.as_ref().cmp(value))
            .ok()?;
        self.postings.get(index)
    }

    pub(crate) fn matches_key(&self, key: &str, types: Types) -> bool {
        self.key.as_ref() == key && self.types == types
    }

    pub(crate) fn matching<'a, S: Source>(
        &'a self,
        library: &'a Library<S>,
        value: &str,
    ) -> IndexedMatches<'a, S> {
        let (offsets, runs) = self.posting(value).map_or((&[][..], &[][..]), |posting| {
            (posting.offsets.as_ref(), posting.runs.as_ref())
        });
        IndexedMatches {
            library,
            offsets,
            runs,
            end: offsets.len(),
            next_tile_remaining: next_tile_remaining(runs, offsets.len()),
            tile: first_tile(runs, &self.tiles),
            tiles: &self.tiles,
        }
    }

    /// Size of retained key and value strings, feature offsets, tile runs and
    /// tile extents, including posting headers.
    pub(crate) fn retained_bytes(&self) -> usize {
        self.retained
    }

    pub(crate) fn bytes_within(&self, limit: usize) -> Result<usize> {
        if self.retained > limit {
            Err(Error::Limit("text tag index bytes"))
        } else {
            Ok(self.retained)
        }
    }
}

pub(crate) struct IndexedMatches<'a, S> {
    library: &'a Library<S>,
    offsets: &'a [u32],
    runs: &'a [TileRun],
    // Offset-array position immediately after this iterator's final feature.
    end: usize,
    next_tile_remaining: usize,
    tile: TileExtent,
    tiles: &'a [TileExtent],
}

fn first_tile(runs: &[TileRun], tiles: &[TileExtent]) -> TileExtent {
    runs.first()
        .map_or(TileExtent::default(), |run| tiles[run.tile as usize])
}

fn next_tile_remaining(runs: &[TileRun], end: usize) -> usize {
    // Zero also represents a boundary past the end of this chunk.
    runs.get(1)
        .map_or(0, |run| end.saturating_sub(run.start as usize))
}

#[cfg(feature = "parallel")]
impl<'a, S: Source + Sync> IndexedMatches<'a, S> {
    pub(crate) fn remaining(&self) -> usize {
        self.offsets.len()
    }

    pub(crate) fn par_chunks(
        self,
        size: usize,
    ) -> impl rayon::iter::IndexedParallelIterator<Item = Self> {
        use rayon::prelude::*;
        let first = self.end - self.offsets.len();
        self.offsets
            .par_chunks(size)
            .enumerate()
            .map(move |(index, offsets)| {
                let position = first + index * size;
                let end = position + offsets.len();
                // A nonempty chunk begins within an existing tile run, even
                // when the serial prefix stopped exactly on a run boundary.
                let run = self
                    .runs
                    .partition_point(|run| run.start as usize <= position)
                    - 1;
                let runs = &self.runs[run..];
                Self {
                    offsets,
                    runs,
                    end,
                    next_tile_remaining: next_tile_remaining(runs, end),
                    tile: first_tile(runs, self.tiles),
                    ..self
                }
            })
    }
}

impl<'a, S: Source> Iterator for IndexedMatches<'a, S> {
    type Item = Result<Feature<'a, S>>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let (&offset, rest) = self.offsets.split_first()?;
        if self.offsets.len() == self.next_tile_remaining {
            self.runs = &self.runs[1..];
            self.next_tile_remaining = next_tile_remaining(self.runs, self.end);
            self.tile = self.tiles[self.runs[0].tile as usize];
        }
        let tile = self.tile;
        let result = Feature::at_source(
            self.library,
            &self.library.source,
            tile.start() + u64::from(offset),
            tile,
        );
        self.offsets = if result.is_ok() { rest } else { &[] };
        Some(result)
    }
}

impl<S: Source> std::iter::FusedIterator for IndexedMatches<'_, S> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Query,
        decode::{Decode, add},
    };

    fn building_payload(source: &[u8]) -> Result<u64> {
        let library = Library::new(source)?;
        let query = Query::parse("*[building=yes]")?;
        let prepared = query.prepare(&library)?;
        let feature = prepared.iter().next().expect("fixture has a building")?;
        let mut table = source.relative(feature.anchor() + 8, !1)?;
        let building = library
            .dictionary()?
            .key_code("building")
            .expect("global building key");
        loop {
            let word = source.u16(table)?;
            if (word >> 2) & 8191 == building {
                assert_eq!(word & 3, 1, "fixture has a dictionary-coded building value");
                return Ok(table + 2);
            }
            assert_eq!(word & 0x8000, 0);
            table += 4 + u64::from(word & 2);
        }
    }

    #[test]
    fn duplicate_dictionary_value_spelling_merges_postings() -> Result<()> {
        let original = include_bytes!("../tests/fixtures/global.gol").as_slice();
        let mut bytes = original.to_vec();
        let payload = building_payload(original)? as usize;
        bytes[payload..payload + 2].copy_from_slice(&4u16.to_le_bytes());
        let dictionary = original.u32(84)? as usize;
        let inner = bytes[dictionary..]
            .windows(6)
            .position(|word| word == b"\x05inner")
            .unwrap();
        let start = dictionary + inner + 1;
        bytes[start..start + 5].copy_from_slice(b"outer");
        let library = Library::new(bytes)?;
        let index = TextTagIndex::build(&library, "building", Types::ALL, 1024)?;
        let query = Query::parse("*[building=outer]")?;
        let expected = query
            .prepare(&library)?
            .iter()
            .collect::<Result<Vec<_>>>()?;
        let actual = index
            .matching(&library, "outer")
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(expected.len(), 1);
        assert_eq!(actual.len(), 1);
        assert_eq!(expected[0].id(), actual[0].id());
        Ok(())
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn parallel_chunks_resume_after_lazy_first_result_and_cross_tiles() -> Result<()> {
        use rayon::prelude::*;

        let library = Library::new(include_bytes!("../tests/fixtures/geometry.gol").as_slice())?;
        // Pick one valid anchor in each of three distinct tiles. Repeated
        // offsets give deterministic run and chunk lengths.
        let mut sample = Vec::new();
        for feature in library.features() {
            let feature = feature?;
            if !sample
                .iter()
                .any(|prior: &Feature<'_, _>| prior.tile_start() == feature.tile_start())
            {
                sample.push(feature);
                if sample.len() == 3 {
                    break;
                }
            }
        }
        assert_eq!(sample.len(), 3);
        let tiles: Vec<_> = sample.iter().map(|feature| feature.tile).collect();
        let mut runs = Vec::new();
        let mut offsets = Vec::new();
        let mut expected = Vec::new();
        for (i, (feature, count)) in sample.iter().zip([1, 5, 7]).enumerate() {
            runs.push(TileRun {
                tile: i as u32,
                start: offsets.len() as u32,
            });
            let offset = (feature.anchor() - feature.tile_start()) as u32;
            offsets.extend(std::iter::repeat_n(offset, count));
            expected.extend(std::iter::repeat_n(feature.id(), count));
        }
        let matches = || IndexedMatches {
            library: &library,
            offsets: &offsets,
            runs: &runs,
            end: offsets.len(),
            next_tile_remaining: next_tile_remaining(&runs, offsets.len()),
            tile: first_tile(&runs, &tiles),
            tiles: &tiles,
        };
        for prefix in [0, 1, 2, 6, 7, 13] {
            let mut matches = matches();
            let serial = matches
                .by_ref()
                .take(prefix)
                .map(|feature| Ok(feature?.id()))
                .collect::<Result<Vec<_>>>()?;
            assert_eq!(serial, expected[..prefix]);
            assert_eq!(matches.remaining(), expected.len() - prefix);
            let batches = matches
                .par_chunks(3)
                .map(|batch| {
                    batch
                        .map(|feature| Ok(feature?.id()))
                        .collect::<Result<Vec<_>>>()
                })
                .collect::<Vec<_>>();
            let actual: Vec<_> = batches
                .into_iter()
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            assert_eq!(actual, expected[prefix..]);
        }
        Ok(())
    }

    fn verify_build_error(bytes: &[u8], key: &str) -> Result<()> {
        let library = Library::new(bytes)?;
        let query = Query::parse(&format!("*[{key}=unused_value]"))?;
        let prepared = query.prepare(&library)?;
        let baseline = prepared.iter().find_map(|result| result.err()).unwrap();
        let indexed = TextTagIndex::build(&library, key, Types::ALL, 1024 * 1024)
            .err()
            .expect("a failed build must publish no postings");
        assert_eq!(baseline.to_string(), indexed.to_string());
        Ok(())
    }

    #[test]
    fn build_checks_damaged_dictionary_values_and_local_keys() -> Result<()> {
        let original = include_bytes!("../tests/fixtures/global.gol").as_slice();
        let mut bytes = original.to_vec();
        let payload = building_payload(original)? as usize;
        bytes[payload..payload + 2].copy_from_slice(&u16::MAX.to_le_bytes());
        verify_build_error(&bytes, "building")?;

        let original = include_bytes!("../tests/fixtures/stress.gol").as_slice();
        let library = Library::new(original)?;
        let feature = library
            .features()
            .find_map(|item| {
                let feature = item.ok()?;
                (original.i32(feature.anchor() + 8).ok()? & 1 != 0).then_some(feature)
            })
            .expect("fixture has local keys");
        let table = original.relative(feature.anchor() + 8, !1)?;
        let word = original.i32(table - 4)?;
        let key_start = add(table & !3, i64::from((word & !7) >> 1))?;
        let mut key = key_start;
        assert!(original.varint(&mut key)? > 0);
        // Structurally corrupt local-key length still fails both queries and index builds.
        let mut truncated = original.to_vec();
        truncated[key_start as usize] = 0x7f;
        if let Some(byte) = truncated.get_mut(key_start as usize) {
            // Length 0x80 0x80 ... triggers string-exceeds-format-limit or tile-bound error.
            *byte = 0xff;
        }
        if let Some(next) = truncated.get_mut(key_start as usize + 1) {
            *next = 0xff;
        }
        if let Some(next2) = truncated.get_mut(key_start as usize + 2) {
            *next2 = 0xff;
        }
        verify_build_error(&truncated, "no_such_local_key")?;
        // Invalid UTF-8 in an unmatched local key is skipped during search, while
        // indexing another key succeeds cleanly.
        let mut bytes = original.to_vec();
        bytes[key as usize] = 255;
        let library = Library::new(bytes)?;
        let query = Query::parse("*[no_such_local_key=unused_value]")?;
        assert!(query.prepare(&library)?.iter().next().is_none());
        let empty_index =
            TextTagIndex::build(&library, "no_such_local_key", Types::ALL, 1024 * 1024)?;
        assert!(
            empty_index
                .matching(&library, "unused_value")
                .next()
                .is_none()
        );
        let query = Query::parse("*[name='Grid 1']")?;
        let expected = query
            .prepare(&library)?
            .iter()
            .collect::<Result<Vec<_>>>()?;
        let index = TextTagIndex::build(&library, "name", Types::ALL, 16 * 1024 * 1024)?;
        let actual = index
            .matching(&library, "Grid 1")
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(expected.len(), 1);
        assert_eq!(actual.len(), 1);
        assert_eq!(actual[0].id(), expected[0].id());
        Ok(())
    }
}
