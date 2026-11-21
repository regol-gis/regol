//! Experimental mmap sidecars for immutable GOL files on Unix.
//!
//! The GOL descriptor's file identity, length, mtime and ctime bind a sidecar
//! to its original inode and generation. Copying, rewriting or replacing the
//! GOL requires rebuilding the sidecar.

use crate::{
    Bounds, Feature, Features, Library, PreparedQuery, Query, Result, TagValue,
    decode::ReadWindow,
    feature::TileExtent,
    source::{MappedFile, Source},
};
use std::{
    collections::HashMap,
    fs::{File, Metadata},
    io::{self, BufWriter, Seek, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

mod candidates;
mod direct;
use candidates::{Cursor, Probe, probe_for_host, scalar_probe};

const MAGIC: &[u8; 8] = b"RGLTAG\0\x03";
const STAMP_WORDS: usize = 7;
const STAMP_END: usize = 16 + STAMP_WORDS * 8;
const GOL_HEADER_START: usize = STAMP_END + 16;
const GOL_HEADER_BYTES: usize = 256;
const HEADER: usize = GOL_HEADER_START + GOL_HEADER_BYTES;
const ENTRY: usize = 24;

fn invalid(message: &'static str) -> crate::Error {
    io::Error::new(io::ErrorKind::InvalidData, message).into()
}

fn stamp(metadata: &Metadata) -> [u64; STAMP_WORDS] {
    [
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime() as u64,
        metadata.mtime_nsec() as u64,
        metadata.ctime() as u64,
        metadata.ctime_nsec() as u64,
    ]
}

fn number<const N: usize>(bytes: &[u8]) -> [u8; N] {
    bytes[..N].try_into().expect("checked sidecar field width")
}

fn u32_at(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(number(bytes))
}

fn u64_at(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(number(bytes))
}

/// On-disk representation to use for an experimental tag sidecar.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// Sorted postings of matching physical records within each tile leaf.
    Exact,
    /// A contiguous 32-bit filter word per physical record in each leaf.
    Words,
}

impl Kind {
    fn code(self) -> u8 {
        match self {
            Self::Exact => 0,
            Self::Words => 1,
        }
    }
}

/// Size and coverage of a newly built tag sidecar.
#[derive(Debug)]
pub struct BuildStats {
    /// Total bytes in the sidecar.
    pub bytes: u64,
    /// Spatial-index leaves examined.
    pub leaves: u64,
    /// Leaves represented in the sidecar.
    pub represented_leaves: u64,
    /// Feature records examined across the leaves.
    pub records: u64,
    /// Match counts for the indexed tag pairs, in pair order.
    pub pair_matches: Vec<u64>,
}

#[derive(Clone, Copy)]
struct Entry {
    leaf: u64,
    offset: u64,
    bytes: u32,
    count: u32,
}

impl Entry {
    fn write(self, output: &mut impl Write) -> io::Result<()> {
        output.write_all(&self.leaf.to_le_bytes())?;
        output.write_all(&self.offset.to_le_bytes())?;
        output.write_all(&self.bytes.to_le_bytes())?;
        output.write_all(&self.count.to_le_bytes())
    }
}

fn open_gol(path: &Path) -> io::Result<(File, [u64; STAMP_WORDS])> {
    let file = File::open(path)?;
    let identity = stamp(&file.metadata()?);
    Ok((file, identity))
}

/// Build a sidecar from checked GOL traversal. A successful build publishes
/// the output atomically and requires a new path. Up to 32 distinct text
/// key/value pairs share the traversal.
///
/// # Safety
/// Keep the GOL file immutable for the duration of this call.
#[allow(unsafe_code)]
pub unsafe fn build(
    gol_path: &Path,
    output_path: &Path,
    kind: Kind,
    pairs: &[(String, String)],
) -> Result<BuildStats> {
    if pairs.is_empty() || pairs.len() > 32 {
        return Err(invalid("expected 1 to 32 sidecar pairs"));
    }
    let (file, identity) = open_gol(gol_path)?;
    // SAFETY: The caller keeps the mapped descriptor immutable until return.
    let mut gol = Library::new(unsafe { MappedFile::from_immutable_file(&file)? })?;
    gol.set_adaptive_text_index_bytes(0);

    let mut by_key: HashMap<&str, Vec<(&str, usize, crate::Predicate)>> = HashMap::new();
    for (index, (key, value)) in pairs.iter().enumerate() {
        let values = by_key.entry(key).or_default();
        if values.iter().any(|&(previous, _, _)| previous == value) {
            return Err(invalid("duplicate sidecar pair"));
        }
        values.push((value, index, crate::Predicate::sidecar_exact(key, value)));
    }

    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut output = tempfile::NamedTempFile::new_in(parent)?;
    let mut writer = BufWriter::with_capacity(256 * 1024, output.as_file_mut());
    writer.write_all(MAGIC)?;
    writer.write_all(&[kind.code(), pairs.len() as u8])?;
    writer.write_all(&[0; 6])?;
    for part in identity {
        writer.write_all(&part.to_le_bytes())?;
    }
    writer.write_all(&[0; 16])?; // Patched with the leaf directory range.
    writer.write_all(gol_header(&gol))?;
    let mut written = HEADER as u64;
    for (key, value) in pairs {
        for text in [key, value] {
            let length =
                u16::try_from(text.len()).map_err(|_| invalid("sidecar string too long"))?;
            writer.write_all(&length.to_le_bytes())?;
            writer.write_all(text.as_bytes())?;
            written += 2 + u64::from(length);
        }
    }

    let mut traversal = Features::new(&gol, None);
    let mut tags = ReadWindow::new(gol.source());
    let mut strings = ReadWindow::new(gol.source());
    let mut entries = Vec::new();
    let mut words = Vec::new();
    let mut wide = Vec::new();
    let mut offsets = (0..pairs.len()).map(|_| Vec::new()).collect::<Vec<_>>();
    let mut stats = BuildStats {
        bytes: 0,
        leaves: 0,
        represented_leaves: 0,
        records: 0,
        pair_matches: vec![0; pairs.len()],
    };

    while let Some((leaf, node, tile)) = traversal.next_sidecar_leaf()? {
        words.clear();
        wide.clear();
        for list in &mut offsets {
            list.clear();
        }
        let mut count = 0u32;
        let mut position = leaf;
        let mut present = 0u32;
        let mut any_wide = false;
        loop {
            traversal.charge_sidecar_build_record()?;
            let anchor = position + if node { 8 } else { 16 };
            let feature = Feature::at_source(&gol, gol.source(), anchor, tile)?;
            if (feature.kind() == crate::FeatureType::Node) != node {
                return Err(invalid(
                    "sidecar build found a record inconsistent with its leaf type",
                ));
            }
            let flags = feature.flags();
            let mut mask = 0u32;
            if flags & 0x800 == 0 {
                feature.bounds()?;
                // Visit all encoded tags. Check matching spellings with the
                // GOQL evaluator, which also checks their encoding.
                for tag in feature.tags()? {
                    let tag = tag?;
                    let TagValue::Text(text) = tag.value else {
                        continue;
                    };
                    let Some(values) = by_key.get(tag.key.as_ref()) else {
                        continue;
                    };
                    for (value, index, predicate) in values {
                        let bit = 1u32 << index;
                        if text.as_ref() == *value
                            && mask & bit == 0
                            && predicate.sidecar_matches(feature, &mut tags, &mut strings)?
                        {
                            mask |= bit;
                            stats.pair_matches[*index] += 1;
                            if matches!(kind, Kind::Exact) {
                                offsets[*index].push((position - leaf) as u32);
                            }
                        }
                    }
                }
            }
            if matches!(kind, Kind::Words) {
                words.push(mask);
                if node {
                    if count % 32 == 0 {
                        wide.push(0u32);
                    }
                    if flags & 4 != 0 {
                        *wide.last_mut().expect("current node group") |= 1 << (count % 32);
                        any_wide = true;
                    }
                }
            }
            present |= mask;
            count += 1;
            if flags & 1 != 0 {
                break;
            }
            position += if node { 20 + u64::from(flags & 4) } else { 32 };
        }
        stats.records += u64::from(count);
        stats.leaves += 1;

        let start = written;
        match kind {
            Kind::Exact if present != 0 => {
                for list in offsets.iter().filter(|list| !list.is_empty()) {
                    put_words(&mut writer, &[list.len() as u32], &mut written)?;
                    put_words(&mut writer, list, &mut written)?;
                }
            }
            Kind::Exact => {}
            Kind::Words => {
                put_words(&mut writer, &words, &mut written)?;
                if any_wide {
                    put_words(&mut writer, &wide, &mut written)?;
                }
            }
        }
        if written != start {
            entries.push(Entry {
                leaf,
                offset: start,
                bytes: u32::try_from(written - start)
                    .map_err(|_| invalid("sidecar leaf payload too large"))?,
                count: match kind {
                    Kind::Exact => present,
                    Kind::Words => count,
                },
            });
        }
    }
    entries.sort_unstable_by_key(|entry| entry.leaf);
    if entries.windows(2).any(|pair| pair[0].leaf == pair[1].leaf) {
        return Err(invalid("repeated GOL leaf in sidecar build"));
    }
    let directory = written;
    for entry in &entries {
        entry.write(&mut writer)?;
    }
    let directory_end = directory + entries.len() as u64 * ENTRY as u64;
    let total = if matches!(kind, Kind::Exact) {
        direct::write_runs(&mut writer, &gol, pairs, &stats.pair_matches, directory_end)?
    } else {
        directory_end
    };
    writer.seek(io::SeekFrom::Start(STAMP_END as u64))?;
    writer.write_all(&directory.to_le_bytes())?;
    writer.write_all(&directory_end.to_le_bytes())?;
    writer.flush()?;
    drop(writer);
    output.as_file().sync_all()?;
    if stamp(&file.metadata()?) != identity {
        return Err(invalid("GOL changed during sidecar build"));
    }
    stats.represented_leaves = entries.len() as u64;
    stats.bytes = total;
    output
        .persist_noclobber(output_path)
        .map_err(|error| error.error)?;
    Ok(stats)
}

fn put_words(output: &mut impl Write, words: &[u32], written: &mut u64) -> io::Result<()> {
    for word in words {
        output.write_all(&word.to_le_bytes())?;
    }
    *written += (words.len() * 4) as u64;
    Ok(())
}

fn gol_header(gol: &Library<MappedFile>) -> &[u8] {
    // A successfully opened library checks these bytes.
    &gol.source().as_bytes().expect("mapped file")[..GOL_HEADER_BYTES]
}

fn lower_bound(entries: &[u8], location: u64) -> usize {
    let mut left = 0;
    let mut right = entries.len() / ENTRY;
    while left < right {
        let middle = left + (right - left) / 2;
        if u64_at(&entries[middle * ENTRY..]) < location {
            left = middle + 1;
        } else {
            right = middle;
        }
    }
    left * ENTRY
}

struct Sidecar {
    map: MappedFile,
    kind: Kind,
    pairs: Vec<(String, String)>,
    data_start: usize,
    directory: usize,
    directory_end: usize,
    direct: Vec<Option<std::ops::Range<usize>>>,
    probe: Probe,
}

impl Sidecar {
    fn new(map: MappedFile, identity: [u64; STAMP_WORDS], gol_header: &[u8]) -> Result<Self> {
        let bytes = map.as_bytes().expect("mapped file");
        if bytes.len() < HEADER || &bytes[..8] != MAGIC {
            return Err(invalid("invalid tag sidecar header"));
        }
        let kind = match bytes[8] {
            0 => Kind::Exact,
            1 => Kind::Words,
            _ => return Err(invalid("unknown tag sidecar kind")),
        };
        let pair_count = bytes[9];
        if !(1..=32).contains(&pair_count) {
            return Err(invalid("invalid tag sidecar pair count"));
        }
        if bytes[16..STAMP_END]
            .chunks_exact(8)
            .map(u64_at)
            .ne(identity)
            || &bytes[GOL_HEADER_START..HEADER] != gol_header
        {
            return Err(invalid(
                "tag sidecar belongs to a different GOL file generation",
            ));
        }
        let table = if matches!(kind, Kind::Exact) {
            bytes
                .len()
                .checked_sub(pair_count as usize * 16)
                .ok_or_else(|| invalid("missing direct posting table"))?
        } else {
            bytes.len()
        };
        let directory = usize::try_from(u64_at(&bytes[STAMP_END..]))
            .map_err(|_| invalid("invalid tag sidecar directory"))?;
        let directory_end = usize::try_from(u64_at(&bytes[STAMP_END + 8..]))
            .map_err(|_| invalid("invalid tag sidecar directory"))?;
        if directory < HEADER
            || directory_end < directory
            || directory_end > table
            || (directory_end - directory) % ENTRY != 0
        {
            return Err(invalid("invalid tag sidecar directory"));
        }
        let mut direct = Vec::new();
        for row in bytes[table..].chunks_exact(16) {
            let length = u64_at(&row[8..]);
            if length == direct::DENSE {
                direct.push(None);
                continue;
            }
            let start = usize::try_from(u64_at(row))
                .map_err(|_| invalid("invalid direct posting start"))?;
            let end = usize::try_from(length)
                .ok()
                .and_then(|length| start.checked_add(length))
                .filter(|&end| start >= directory_end && end <= table)
                .ok_or_else(|| invalid("invalid direct posting range"))?;
            direct.push(Some(start..end));
        }
        let mut sp = &bytes[HEADER..directory];
        let read_string = |remaining: &mut &[u8]| -> Result<String> {
            let (length, rest) = remaining
                .split_first_chunk::<2>()
                .ok_or_else(|| invalid("truncated tag sidecar pair"))?;
            let length = usize::from(u16::from_le_bytes(*length));
            let (value, rest) = rest
                .split_at_checked(length)
                .ok_or_else(|| invalid("truncated tag sidecar pair"))?;
            *remaining = rest;
            Ok(std::str::from_utf8(value)
                .map_err(|_| invalid("non UTF-8 tag sidecar pair"))?
                .into())
        };
        let mut pairs = Vec::with_capacity(pair_count as usize);
        for _ in 0..pair_count {
            pairs.push((read_string(&mut sp)?, read_string(&mut sp)?));
        }
        let data_start = directory - sp.len();
        Ok(Self {
            map,
            kind,
            pairs,
            data_start,
            directory,
            directory_end,
            direct,
            probe: probe_for_host(),
        })
    }

    fn tile_directory(&self, tile: TileExtent) -> &[u8] {
        let bytes = self.map.as_bytes().expect("mapped file");
        let entries = &bytes[self.directory..self.directory_end];
        let start = lower_bound(entries, tile.start());
        let length = lower_bound(&entries[start..], tile.end());
        &entries[start..start + length]
    }

    fn candidates(&self, entries: &[u8], leaf: u64, node: bool, mask: u32) -> Result<Cursor<'_>> {
        let index = lower_bound(entries, leaf);
        let Some(row) = entries[index..]
            .first_chunk::<ENTRY>()
            .filter(|row| u64_at(*row) == leaf)
        else {
            return match self.kind {
                Kind::Exact => Ok(Cursor::Empty),
                Kind::Words => Err(invalid("missing leaf in filter-word sidecar")),
            };
        };
        let count = u32_at(&row[20..]);
        if matches!(self.kind, Kind::Exact) && count & mask == 0 {
            return Ok(Cursor::Empty);
        }
        let offset = usize::try_from(u64_at(&row[8..]))
            .map_err(|_| invalid("tag sidecar leaf offset overflows"))?;
        let size = u32_at(&row[16..]) as usize;
        let bytes = self.map.as_bytes().expect("mapped file");
        let payload = bytes[..self.directory]
            .get(offset..)
            .filter(|_| offset >= self.data_start)
            .and_then(|tail| tail.get(..size))
            .ok_or_else(|| invalid("invalid tag sidecar leaf extent"))?;
        match self.kind {
            Kind::Exact => Cursor::exact(payload, leaf, count, mask),
            Kind::Words => Cursor::words(payload, leaf, node, mask, count, self.probe),
        }
    }
}

/// Opt-in queries bound to an immutable GOL descriptor.
pub struct IndexedLibrary {
    gol: Library<MappedFile>,
    sidecar: Option<Sidecar>,
    leaf_control: bool,
}

impl IndexedLibrary {
    /// Open a GOL with an optional locally built sidecar. A supplied sidecar
    /// with a mismatched binding or invalid header fails explicitly.
    ///
    /// # Safety
    /// Keep the GOL and the trusted, locally generated sidecar immutable until
    /// this library is dropped. Sidecar payload completeness is trusted.
    #[allow(unsafe_code)]
    pub unsafe fn open(gol_path: &Path, sidecar_path: Option<&Path>) -> Result<Self> {
        let (file, identity) = open_gol(gol_path)?;
        // SAFETY: The caller keeps both files immutable for the mapping lifetime.
        let mut gol = Library::new(unsafe { MappedFile::from_immutable_file(&file)? })?;
        let sidecar = sidecar_path
            .map(|path| {
                // SAFETY: The caller keeps the sidecar immutable through use.
                let map = unsafe { MappedFile::open(path)? };
                Sidecar::new(map, identity, gol_header(&gol))
            })
            .transpose()?;
        gol.set_adaptive_text_index_bytes(0);
        Ok(Self {
            gol,
            sidecar,
            leaf_control: false,
        })
    }

    /// Keep the exact index on the leaf traversal even for nationwide queries.
    pub fn use_leaf_postings_control(&mut self) {
        self.leaf_control = true;
    }

    /// Select the scalar filter-word implementation for comparison runs.
    pub fn use_scalar_filter(&mut self) {
        if let Some(sidecar) = &mut self.sidecar {
            sidecar.probe = scalar_probe;
        }
    }

    /// Prepare a query using the same GOL dictionary and category planning.
    pub fn prepare<'a, 'q>(&'a self, query: &'q Query) -> Result<IndexedQuery<'a, 'q>> {
        let prepared = query.prepare(&self.gol)?;
        let sidecar = self.sidecar.as_ref().and_then(|sidecar| {
            query
                .sidecar_mask(&sidecar.pairs)
                .map(|mask| (sidecar, mask))
        });
        Ok(IndexedQuery {
            prepared,
            sidecar,
            gol: &self.gol,
            simple_equality: query.sidecar_simple_equality(),
            leaf_control: self.leaf_control,
        })
    }
}

/// A prepared query that can use a matching tag sidecar.
pub struct IndexedQuery<'a, 'q> {
    prepared: PreparedQuery<'a, 'q, MappedFile>,
    sidecar: Option<(&'a Sidecar, u32)>,
    gol: &'a Library<MappedFile>,
    simple_equality: bool,
    leaf_control: bool,
}

impl<'a> IndexedQuery<'a, '_> {
    /// Name of the plan selected for the query and spatial window.
    pub fn plan(&self, bounds: Option<Bounds>) -> &'static str {
        match self.plan_in(bounds) {
            None => "gol",
            Some(Plan::Direct(_)) => "direct",
            Some(Plan::Leaves(sidecar, _)) => match sidecar.kind {
                Kind::Exact => "postings",
                Kind::Words => "words",
            },
        }
    }

    fn plan_in(&self, bounds: Option<Bounds>) -> Option<Plan<'a>> {
        let (sidecar, mask) = self.sidecar?;
        if matches!(sidecar.kind, Kind::Exact)
            && bounds.is_none()
            && self.simple_equality
            && !self.leaf_control
        {
            let range = sidecar.direct[mask.trailing_zeros() as usize].as_ref()?;
            let bytes = sidecar.map.as_bytes().expect("mapped file");
            return Some(Plan::Direct(&bytes[range.clone()]));
        }
        Some(Plan::Leaves(sidecar, mask))
    }

    /// Stream results with automatic fallback for uncovered predicates.
    pub fn iter(
        &self,
        bounds: Option<Bounds>,
    ) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, MappedFile>>> + '_ {
        let mut indexed = self.indexed_iter(bounds);
        let mut fallback = indexed.is_none().then(|| self.ordinary_iter(bounds));
        std::iter::from_fn(move || match indexed.as_mut() {
            Some(indexed) => indexed.next(),
            None => fallback.as_mut()?.next(),
        })
        .fuse()
    }

    /// Stream candidates through the sidecar when it covers the query.
    pub fn indexed_iter(
        &self,
        bounds: Option<Bounds>,
    ) -> Option<impl std::iter::FusedIterator<Item = Result<Feature<'a, MappedFile>>> + '_> {
        let (sidecar, mask) = match self.plan_in(bounds)? {
            Plan::Direct(bytes) => {
                return Some(Matches::Direct(direct::Direct::new(
                    &self.prepared,
                    self.gol,
                    bytes,
                )));
            }
            Plan::Leaves(sidecar, mask) => (sidecar, mask),
        };
        Some(Matches::Leaves(SidecarMatches {
            prepared: &self.prepared,
            traversal: self.prepared.features(bounds),
            sidecar,
            mask,
            candidates: Cursor::Empty,
            tile_start: u64::MAX,
            tile_directory: &[],
            node: false,
            done: false,
        }))
    }

    /// Stream the standard query directly, including the ordinary fallback.
    pub fn ordinary_iter(
        &self,
        bounds: Option<Bounds>,
    ) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, MappedFile>>> + '_ {
        self.prepared.execute_sidecar_fallback(bounds)
    }
}

enum Plan<'a> {
    Direct(&'a [u8]),
    Leaves(&'a Sidecar, u32),
}

// Keep short bounded queries free of an extra heap allocation per cursor.
#[allow(clippy::large_enum_variant)]
enum Matches<'a, 'p, 'q> {
    Direct(direct::Direct<'a, 'p, 'q>),
    Leaves(SidecarMatches<'a, 'p, 'q>),
}

impl<'a> Iterator for Matches<'a, '_, '_> {
    type Item = Result<Feature<'a, MappedFile>>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Direct(direct) => direct.next(),
            Self::Leaves(leaves) => leaves.next(),
        }
    }
}

impl std::iter::FusedIterator for Matches<'_, '_, '_> {}

struct SidecarMatches<'a, 'p, 'q> {
    prepared: &'p PreparedQuery<'a, 'q, MappedFile>,
    traversal: Features<'a, MappedFile>,
    sidecar: &'a Sidecar,
    mask: u32,
    candidates: Cursor<'a>,
    tile_start: u64,
    tile_directory: &'a [u8],
    node: bool,
    done: bool,
}

impl<'a> SidecarMatches<'a, '_, '_> {
    fn next_match(&mut self) -> Result<Option<Feature<'a, MappedFile>>> {
        loop {
            if let Some(position) = self.candidates.next_position() {
                if let Some(feature) = self.traversal.sidecar_candidate(
                    position,
                    self.node,
                    |feature, tags, strings| {
                        self.prepared
                            .matches_sidecar_candidate(feature, tags, strings)
                    },
                )? {
                    return Ok(Some(feature));
                }
            } else {
                let Some((leaf, node, tile)) = self.traversal.next_sidecar_leaf()? else {
                    return Ok(None);
                };
                if tile.start() != self.tile_start {
                    self.tile_start = tile.start();
                    self.tile_directory = self.sidecar.tile_directory(tile);
                }
                self.candidates =
                    self.sidecar
                        .candidates(self.tile_directory, leaf, node, self.mask)?;
                self.node = node;
            }
        }
    }
}

impl<'a> Iterator for SidecarMatches<'a, '_, '_> {
    type Item = Result<Feature<'a, MappedFile>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let result = self.next_match().transpose();
        self.done = !matches!(result, Some(Ok(_)));
        result
    }
}

impl std::iter::FusedIterator for SidecarMatches<'_, '_, '_> {}
