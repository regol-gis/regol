use crate::{
    Error, Result, Types,
    adaptive_text_index::{AdaptiveTextIndex, DEFAULT_TEXT_INDEX_BYTES},
    decode::Decode,
    source::{PagedFile, Source},
    spatial::{Bounds, Features},
    text_index::TextTagIndex,
};
use std::{borrow::Cow, path::Path};

mod dictionary;
pub(crate) use dictionary::Dictionary;
use once_cell::sync::OnceCell;

/// Resource ceilings for untrusted files. Iterators fail rather than silently
/// truncate when a limit is reached. Defaults can be adjusted for large data.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum work budget for a traversal.
    pub max_traversal_steps: u64,
    /// Maximum depth for nested tree or relation traversal.
    pub max_tree_depth: usize,
    /// Maximum number of coordinates decoded for a way or assembled geometry.
    pub max_coordinates: u64,
    /// Maximum tag entries processed for a feature.
    pub max_tags: u64,
    /// Maximum member entries processed for a relation.
    pub max_members: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_traversal_steps: 1_000_000_000,
            max_tree_depth: 64,
            max_coordinates: 10_000_000,
            max_tags: 100_000,
            max_members: 10_000_000,
        }
    }
}

/// When to read and validate the global string dictionary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DictionaryLoading {
    /// Verify the entire dictionary before returning a library.
    #[default]
    Eager,
    /// Verify it on the first operation that needs a spelling. Unfiltered
    /// spatial queries, coordinate reads and counts can skip it entirely.
    OnDemand,
}

/// A GOL file with its string dictionary and spatial indexes. Features borrow this library.
pub struct Library<S = PagedFile> {
    pub(crate) source: S,
    pub(crate) strings: OnceCell<Dictionary>,
    pub(crate) string_count: usize,
    string_offset: u64,
    pub(crate) tile_index: u64,
    pub(crate) tile_index_end: u64,
    pub(crate) zooms: Vec<u8>,
    pub(crate) limits: Limits,
    pub(crate) way_node_ids: bool,
    pub(crate) categories: Vec<(u16, u32)>,
    active_snapshot: u8,
    tile_count: u32,
    pub(crate) text_indexes: Vec<TextTagIndex>,
    pub(crate) adaptive_text_index: AdaptiveTextIndex,
}
impl Library<PagedFile> {
    /// Open a GOL file using safe, lazily retained pages and default limits.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::new(PagedFile::open(path)?)
    }
    /// Open a file with eager or on-demand dictionary validation. On-demand
    /// mode also allocates its page directory in groups as regions are read.
    pub fn open_with_dictionary_loading(
        path: impl AsRef<Path>,
        loading: DictionaryLoading,
    ) -> Result<Self> {
        let source = match loading {
            DictionaryLoading::Eager => PagedFile::open(path)?,
            DictionaryLoading::OnDemand => PagedFile::from_file_sparse(std::fs::File::open(path)?)?,
        };
        Self::with_dictionary_loading(source, Limits::default(), loading)
    }
}

impl<S: Source> Library<S> {
    /// Open an existing byte source with default resource limits.
    pub fn new(source: S) -> Result<Self> {
        Self::with_limits(source, Limits::default())
    }

    /// Open an existing byte source with caller-specified resource limits.
    pub fn with_limits(source: S, limits: Limits) -> Result<Self> {
        Self::with_dictionary_loading(source, limits, DictionaryLoading::Eager)
    }

    /// Open a source with explicit dictionary loading and traversal limits.
    /// In on-demand mode, a malformed spelling or dictionary I/O error is
    /// reported by the first operation that uses the dictionary.
    pub fn with_dictionary_loading(
        source: S,
        limits: Limits,
        loading: DictionaryLoading,
    ) -> Result<Self> {
        source.check(0, 256)?;
        if source.u32(0)? != 0x1ce5_0d6e {
            return Err(Error::invalid(0, "invalid magic"));
        }
        let version = source.u32(4)?;
        if version != 2 {
            return Err(Error::UnsupportedVersion(version));
        }
        let active = source.u8(16)?;
        if active > 1 {
            return Err(Error::invalid(16, "invalid active snapshot"));
        }
        let snapshot = 128 + u64::from(active) * 64;
        let tile_count = source.u32(snapshot + 28)?;
        let tile_index = u64::from(source.u32(snapshot + 24)?) * 4096;
        if tile_index < 4096 {
            return Err(Error::invalid(snapshot + 24, "invalid tile index page"));
        }
        let tile_index_len = u64::from(source.u32(tile_index)?);
        if tile_index_len < 4 || tile_index_len % 4 != 0 {
            return Err(Error::invalid(tile_index, "invalid tile index length"));
        }
        source.check(tile_index, tile_index_len + 4)?;
        let tile_index_end = tile_index + 4 + tile_index_len;
        let zoom_bits = source.u32(96)?;
        if zoom_bits & 1 == 0 || zoom_bits & !0x1fff != 0 {
            return Err(Error::invalid(96, "invalid zoom levels"));
        }
        let zooms: Vec<u8> = (0..=12).filter(|z| zoom_bits & (1 << z) != 0).collect();
        if zooms.windows(2).any(|z| z[1] - z[0] > 3) {
            return Err(Error::invalid(96, "zoom step exceeds three"));
        }
        let string_offset = u64::from(source.u32(84)?);
        let string_count = source.u16(string_offset)?;
        if string_count == 0 {
            return Err(Error::invalid(string_offset, "missing empty global string"));
        }
        source.check(string_offset + 2, u64::from(string_count))?;
        let strings = OnceCell::new();
        if loading == DictionaryLoading::Eager {
            strings
                .get_or_try_init(|| Dictionary::read(&source, string_offset + 2, string_count))?;
        }
        let way_node_ids = source.u32(80)? & 1 != 0;
        let schema = u64::from(source.u32(88)?);
        let count = source.u32(schema)?;
        if count > 8190 {
            return Err(Error::invalid(schema, "too many indexed keys"));
        }
        source.check(schema + 4, u64::from(count) * 4)?;
        let mut categories = Vec::with_capacity(count as usize);
        for i in 0..count {
            let word = source.u32(schema + 4 + u64::from(i) * 4)?;
            let category = word >> 16;
            if !(1..=30).contains(&category) {
                return Err(Error::invalid(schema, "invalid key category"));
            }
            categories.push((word as u16, 1 << (category - 1)));
        }
        Ok(Self {
            source,
            strings,
            string_count: usize::from(string_count),
            string_offset,
            tile_index,
            tile_index_end,
            zooms,
            limits,
            way_node_ids,
            categories,
            active_snapshot: active,
            tile_count,
            text_indexes: Vec::new(),
            adaptive_text_index: AdaptiveTextIndex::new(DEFAULT_TEXT_INDEX_BYTES),
        })
    }

    /// Build an in-memory text tag index for one key and set of feature types.
    ///
    /// The index accelerates unbounded prepared streams for one exact text
    /// predicate on this key and these types. Values can vary between queries.
    /// The build performs a full checked traversal of the relevant candidates
    /// and applies the library's traversal and tag limits. Stable source bytes
    /// are required for the life of the index.
    ///
    /// Returns retained bytes for keys, values, posting headers and locations,
    /// and tile extents. Temporary build allocations and source buffers are
    /// outside this limit. Repeated registration reuses an existing index for
    /// this key and these types, including one learned from earlier queries.
    pub fn index_text_key(&mut self, key: &str, types: Types, max_bytes: usize) -> Result<usize> {
        if let Some(index) = self
            .text_indexes
            .iter()
            .find(|index| index.matches_key(key, types))
        {
            return index.bytes_within(max_bytes);
        }
        let index = if let Some(learned) = self
            .adaptive_text_index
            .index
            .get()
            .filter(|index| index.matches_key(key, types))
        {
            learned.bytes_within(max_bytes)?;
            self.adaptive_text_index
                .index
                .take()
                .expect("checked learned index")
        } else {
            TextTagIndex::build(self, key, types, max_bytes)?
        };
        let bytes = index.retained_bytes();
        self.text_indexes.push(index);
        Ok(bytes)
    }

    /// Set the byte budget for one automatically learned text index.
    ///
    /// The default budget is 16 MiB. Only contiguous sources can learn an index
    /// after repeated complete, expensive streams on the same indexed tag key
    /// and feature types. The triggering stream pays for construction when it
    /// reaches the end. Other streams continue normally while construction runs.
    ///
    /// Changing the budget clears the learned index and its query history.
    /// A zero budget disables learning. Explicitly registered indexes are retained.
    pub fn set_adaptive_text_index_bytes(&mut self, max_bytes: usize) {
        self.adaptive_text_index = AdaptiveTextIndex::new(max_bytes);
    }

    /// Retained heap payload of registered and learned text indexes, in bytes.
    pub fn text_index_bytes(&self) -> usize {
        self.text_indexes
            .iter()
            .chain(self.adaptive_text_index.index.get())
            .map(TextTagIndex::retained_bytes)
            .sum()
    }

    /// Iterate the active snapshot's features with checked traversal.
    pub fn features(&self) -> Features<'_, S> {
        Features::new(self, None)
    }
    /// Features whose stored bounding boxes intersect `bounds`, inclusively.
    /// This is not an exact geometry intersection predicate.
    pub fn in_bounds(&self, bounds: Bounds) -> Features<'_, S> {
        Features::new(self, Some(bounds))
    }
    /// Resolve a global dictionary code to its spelling. An undefined code returns an error.
    pub fn global_string(&self, code: u16) -> Result<Cow<'_, str>> {
        self.dictionary()?
            .get(code)
            .map(Cow::Borrowed)
            .ok_or_else(|| Error::invalid(84, "undefined global string code"))
    }
    /// Borrow the byte source backing this library.
    pub fn source(&self) -> &S {
        &self.source
    }
    /// Return the resource limits used by this library.
    pub fn limits(&self) -> Limits {
        self.limits
    }
    /// Whether IDs of geometry-only way vertices were retained by the producer.
    pub fn has_way_node_ids(&self) -> bool {
        self.way_node_ids
    }
    /// Return the active snapshot slot selected by the header.
    pub fn active_snapshot(&self) -> u8 {
        self.active_snapshot
    }
    /// Return the tile count declared by the active snapshot.
    pub fn tile_count(&self) -> u32 {
        self.tile_count
    }
    /// Iterate the store's key/value metadata properties.
    pub fn properties(&self) -> Result<Properties<'_, S>> {
        let pointer = u64::from(self.source.u32(92)?);
        if pointer == 0 {
            return Ok(Properties {
                library: self,
                p: 0,
                remaining: 0,
            });
        }
        let remaining = self.source.u16(pointer)?;
        Ok(Properties {
            library: self,
            p: pointer + 2,
            remaining,
        })
    }
    #[inline]
    pub(crate) fn dictionary(&self) -> Result<&Dictionary> {
        self.strings.get_or_try_init(|| {
            Dictionary::read(
                &self.source,
                self.string_offset + 2,
                self.string_count as u16,
            )
        })
    }

    pub(crate) fn is_sole_category_key(&self, code: u16) -> bool {
        let Some(&(_, bits)) = self.categories.iter().find(|(key, _)| *key == code) else {
            return false;
        };
        self.categories
            .iter()
            .filter(|(_, other)| *other == bits)
            .count()
            == 1
    }

    pub(crate) fn index_word(&self, p: u64) -> Result<u32> {
        if p < self.tile_index + 4 || p.checked_add(4).is_none_or(|e| e > self.tile_index_end) {
            return Err(Error::invalid(p, "tile pointer outside index"));
        }
        self.source.u32(p)
    }
    pub(crate) fn tile_page(&self, tip: u32) -> Result<u64> {
        let word = self.index_word(self.tile_index + u64::from(tip) * 4)?;
        if word & 3 != 2 || word >> 2 == 0 {
            return Err(Error::MissingTile(tip));
        }
        let offset = u64::from(word >> 2) * 4096;
        self.source.check(offset, 28)?;
        Ok(offset)
    }
}

/// Library metadata as lazy UTF-8 key/value pairs.
pub struct Properties<'a, S> {
    library: &'a Library<S>,
    p: u64,
    remaining: u16,
}
impl<'a, S: Source> Iterator for Properties<'a, S> {
    type Item = Result<(Cow<'a, str>, Cow<'a, str>)>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;
        let result = self
            .library
            .source
            .next_string(&mut self.p)
            .and_then(|key| {
                self.library
                    .source
                    .next_string(&mut self.p)
                    .map(|value| (key, value))
            });
        if result.is_err() {
            self.remaining = 0;
        }
        Some(result)
    }
}
impl<S: Source> std::iter::FusedIterator for Properties<'_, S> {}
