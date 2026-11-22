//! Catalog and incremental tile loading for GeoDesk Geo-Object Bundles (GOB 2.0).
//!
//! Open a [`GobCatalog`] with the archive header, catalog and metadata chunk.
//! Use its tile IDs and byte ranges to fetch only the chunks needed by a query.
//! [`GobStore`] imports those chunks and exposes the existing [`Library`] API.

use std::{
    collections::BTreeMap,
    io::{self, Write},
    sync::atomic::{AtomicU32, Ordering},
};

use flate2::{Decompress, FlushDecompress, Status};

use crate::{Error, Library, Limits, Result, decode::Decode, source::Source};

mod catalog;
mod tile;

pub use catalog::{GobCatalog, TileEntry};

const PAGE: u64 = 4096;
const SEGMENT: u64 = 1 << 30;
const NO_MISSING_TILE: u32 = u32::MAX;
const MIN_CHUNK_BYTES: usize = 10;

fn invalid(offset: u64, message: &'static str) -> Error {
    Error::InvalidGob { offset, message }
}

fn put_u32(bytes: &mut [u8], offset: usize, word: u32) {
    bytes[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
}

fn update_header_checksum(bytes: &mut [u8]) {
    let checksum = crc32c::crc32c(&bytes[..504]);
    put_u32(bytes, 504, checksum);
}

fn write_padding(output: &mut impl Write, mut len: u64) -> io::Result<()> {
    let padding = [0; PAGE as usize];
    while len != 0 {
        let n = len.min(PAGE);
        output.write_all(&padding[..n as usize])?;
        len -= n;
    }
    Ok(())
}

fn inflate(chunk: &[u8], offset: u64, max_bytes: usize) -> Result<Vec<u8>> {
    if chunk.len() < MIN_CHUNK_BYTES {
        return Err(invalid(offset, "truncated sealed chunk"));
    }
    let len = chunk.u32(0)? as usize;
    let checksum = chunk.u32(4)?;
    if len > max_bytes {
        return Err(Error::Limit("uncompressed GOB chunk bytes"));
    }
    // The extra byte lets the inflater consume the end marker even when the
    // declared output length is exactly correct.
    let capacity = len
        .checked_add(1)
        .ok_or(Error::Limit("uncompressed GOB chunk bytes"))?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Limit("uncompressed GOB chunk allocation"))?;
    output.resize(capacity, 0);
    let mut decoder = Decompress::new(false);
    let status = decoder
        .decompress(&chunk[8..], &mut output, FlushDecompress::Finish)
        .map_err(|_| invalid(offset + 8, "invalid raw DEFLATE stream"))?;
    if status != Status::StreamEnd
        || decoder.total_in() != (chunk.len() - 8) as u64
        || decoder.total_out() != len as u64
    {
        return Err(invalid(offset, "sealed chunk size mismatch"));
    }
    output.truncate(len);
    if crc32c::crc32c(&output) != checksum {
        return Err(invalid(offset + 4, "sealed chunk checksum mismatch"));
    }
    Ok(output)
}

/// A GOB snapshot with a collection of incrementally loaded feature tiles.
///
/// Unloaded tiles remain in the spatial directory. A query that needs one
/// reports [`Error::MissingTile`], including when a relation references a tile
/// outside the query bounds. The same catalog may describe a partial archive.
pub struct GobStore {
    catalog: GobCatalog,
    library: Library<GobSource>,
    max_bytes: usize,
    retained_bytes: usize,
}

impl GobStore {
    /// Open a store with the default query limits and a retained byte budget.
    pub fn new(catalog: GobCatalog, max_bytes: usize) -> Result<Self> {
        Self::with_limits(catalog, max_bytes, Limits::default())
    }

    /// Open a store with caller-specified query limits and retained byte budget.
    ///
    /// The budget covers retained tile buffers, catalog, and GOL metadata and
    /// dictionary buffers. Transient conversion and query allocations are
    /// separate. Tile inflation is capped at the remaining retained byte budget.
    pub fn with_limits(mut catalog: GobCatalog, max_bytes: usize, limits: Limits) -> Result<Self> {
        let mut retained_bytes = catalog.retained_bytes();
        if retained_bytes > max_bytes {
            return Err(Error::Limit("retained GOB store bytes"));
        }
        let metadata = std::mem::take(&mut catalog.metadata);
        let mut library = Library::with_limits(
            GobSource {
                len: metadata.len() as u64,
                metadata,
                tiles: BTreeMap::new(),
                missing: AtomicU32::new(NO_MISSING_TILE),
            },
            limits,
        )?;
        retained_bytes = retained_bytes
            .checked_add(library.dictionary()?.heap_bytes())
            .ok_or(Error::Limit("retained GOB store bytes"))?;
        if retained_bytes > max_bytes {
            return Err(Error::Limit("retained GOB store bytes"));
        }
        library.set_adaptive_text_index_bytes(0);
        Ok(Self {
            catalog,
            library,
            max_bytes,
            retained_bytes,
        })
    }

    /// The archive catalog for this snapshot.
    pub fn catalog(&self) -> &GobCatalog {
        &self.catalog
    }

    /// Query the snapshot using the loaded tiles and shared GOL metadata.
    pub fn library(&self) -> &Library<GobSource> {
        &self.library
    }

    /// Whether this tile is available for queries.
    pub fn is_loaded(&self, tip: u32) -> bool {
        self.catalog.bounds(tip).is_some()
            && self
                .library
                .index_word(self.library.tile_index + u64::from(tip) * 4)
                .is_ok_and(|word| word & 3 == 2 && word >> 2 != 0)
    }

    /// Bytes of retained metadata and tile buffers counted toward the budget.
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Write the current snapshot as a standard GOL 2.0 file.
    ///
    /// The archive's directory and metadata are preserved. Unloaded tile
    /// entries remain unavailable in the resulting file. Padding is streamed
    /// and resident tile buffers are written directly.
    pub fn write_gol(&self, mut output: impl Write) -> Result<()> {
        let source = &self.library.source;
        output.write_all(&source.metadata)?;
        let mut offset = source.metadata.len() as u64;
        for (&start, tile) in &source.tiles {
            write_padding(&mut output, start - offset)?;
            output.write_all(tile)?;
            offset = start + tile.len() as u64;
        }
        write_padding(&mut output, source.len() - offset)?;
        Ok(())
    }

    /// Take the first missing tile ID observed since the last call.
    ///
    /// Clear it before a query to associate a later ID with that query. Query
    /// APIs already return the ID in [`Error::MissingTile`] when it is needed.
    pub fn take_missing_tile(&self) -> Option<u32> {
        let tip = self
            .library
            .source
            .missing
            .swap(NO_MISSING_TILE, Ordering::Relaxed);
        (tip != NO_MISSING_TILE).then_some(tip)
    }

    /// Check and import one sealed tile chunk from its catalog entry.
    ///
    /// Each successful import publishes one independently allocated tile.
    /// Failed imports leave prior tiles intact. Repeated calls for a loaded
    /// tile with the same chunk length leave that tile in place.
    pub fn import_tile(&mut self, tip: u32, chunk: &[u8]) -> Result<()> {
        let entry = self.catalog.tile(tip).ok_or(Error::MissingTile(tip))?;
        if chunk.len() != entry.len as usize {
            return Err(invalid(
                entry.offset,
                "tile chunk length differs from catalog",
            ));
        }
        if self.is_loaded(tip) {
            return Ok(());
        }
        let remaining = self.max_bytes - self.retained_bytes;
        let payload = inflate(chunk, entry.offset, remaining)?;
        let bounds = self
            .catalog
            .bounds(tip)
            .expect("catalog entries have validated tile coordinates");
        let tile = tile::convert(
            &payload,
            bounds,
            self.library.has_way_node_ids(),
            &self.library.categories,
            self.library.string_count,
            remaining,
        )?;
        if tile.capacity() > remaining {
            return Err(Error::Limit("retained GOB store bytes"));
        }
        let bytes = tile.capacity();
        let in_directory = (self.library.tile_index + u64::from(tip) * 4) as usize;
        let tiles = self.library.tile_count + 1;
        self.library.source.publish(in_directory, tile, tiles)?;
        self.library.tile_count = tiles;
        self.retained_bytes += bytes;
        Ok(())
    }
}

/// Sparse GOL byte source used by [`GobStore`].
pub struct GobSource {
    metadata: Vec<u8>,
    tiles: BTreeMap<u64, Vec<u8>>,
    len: u64,
    missing: AtomicU32,
}

impl GobSource {
    fn publish(&mut self, directory_offset: usize, tile: Vec<u8>, tiles: u32) -> Result<()> {
        let mut start = self.len.next_multiple_of(PAGE);
        let len = tile.len() as u64;
        if len > SEGMENT {
            return Err(Error::Limit("GOL tile blob size"));
        }
        if start % SEGMENT + len > SEGMENT {
            start = start.next_multiple_of(SEGMENT);
        }
        let page = start / PAGE;
        if page >= 1 << 30 {
            return Err(Error::Limit("GOL tile address space"));
        }
        self.tiles.insert(start, tile);
        self.len = start + len;
        put_u32(&mut self.metadata, directory_offset, (page as u32) << 2 | 2);
        put_u32(&mut self.metadata, 24, self.len.div_ceil(PAGE) as u32);
        put_u32(&mut self.metadata, 156, tiles);
        update_header_checksum(&mut self.metadata);
        Ok(())
    }

    fn segment_at(&self, offset: u64) -> Option<(u64, &[u8])> {
        if offset < self.metadata.len() as u64 {
            return Some((0, &self.metadata));
        }
        let (&start, tile) = self.tiles.range(..=offset).next_back()?;
        (offset - start < tile.len() as u64).then_some((start, tile.as_slice()))
    }
}

impl Source for GobSource {
    fn len(&self) -> u64 {
        self.len.next_multiple_of(PAGE)
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        let end = offset
            .checked_add(output.len() as u64)
            .filter(|&end| end <= self.len())
            .ok_or(io::ErrorKind::UnexpectedEof)?;
        if let Some(bytes) = self.view(offset, output.len())? {
            output.copy_from_slice(bytes);
            return Ok(());
        }
        output.fill(0);
        let mut copy_overlap = |base: u64, bytes: &[u8]| {
            let from = offset.max(base);
            let to = end.min(base + bytes.len() as u64);
            if from < to {
                output[(from - offset) as usize..(to - offset) as usize]
                    .copy_from_slice(&bytes[(from - base) as usize..(to - base) as usize]);
            }
        };
        copy_overlap(0, &self.metadata);
        if let Some((&start, tile)) = self.tiles.range(..offset).next_back() {
            copy_overlap(start, tile);
        }
        for (&start, tile) in self.tiles.range(offset..end) {
            copy_overlap(start, tile);
        }
        Ok(())
    }

    fn view(&self, offset: u64, len: usize) -> io::Result<Option<&[u8]>> {
        Ok(self
            .retained_tail(offset)
            .and_then(|bytes| bytes.get(..len)))
    }

    fn retained_tail(&self, offset: u64) -> Option<&[u8]> {
        let (start, bytes) = self.segment_at(offset)?;
        bytes.get((offset - start) as usize..)
    }

    fn segment(&self, offset: u64) -> io::Result<Option<(u64, &[u8])>> {
        Ok(self.segment_at(offset))
    }

    fn missing_tile(&self, tip: u32) {
        let _ = self.missing.compare_exchange(
            NO_MISSING_TILE,
            tip,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}
