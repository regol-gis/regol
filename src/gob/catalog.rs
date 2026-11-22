use crate::{Bounds, Error, Result, decode::Decode};

use super::{MIN_CHUNK_BYTES, PAGE, inflate, invalid, put_u32, update_header_checksum};

const HEADER_BYTES: usize = 64;
const MAX_TILES: usize = 8_000_000;
const MAX_METADATA_BYTES: usize = 64 * 1024 * 1024;
const UNSEEN: u32 = u32::MAX;
const DIRECTORY_WORD: u32 = u32::MAX - 1;

/// Byte range of a sealed tile chunk in a GOB archive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TileEntry {
    /// Tile index position in the snapshot's spatial directory.
    pub tip: u32,
    /// Absolute byte offset in the GOB.
    pub offset: u64,
    /// Compressed chunk length, including the sealed-chunk header.
    pub len: u32,
}

/// A validated GOB 2.0 header, table of chunk locations and spatial directory.
pub struct GobCatalog {
    archive_len: u64,
    revision: u32,
    tileset_id: [u8; 16],
    entries: Vec<TileEntry>,
    // Indexed by TIP. Non-tile words carry sentinel values.
    tiles: Vec<u32>,
    pub(super) metadata: Vec<u8>,
}

impl GobCatalog {
    /// Byte length to fetch at offset zero to open the catalog.
    ///
    /// Requires the first 64 bytes. The result includes the checked catalog's
    /// CRC32C and the sealed metadata chunk.
    pub fn prefix_len(header: &[u8]) -> Result<usize> {
        let header = Header::read(header)?;
        Ok(header.catalog_len() + header.metadata_len)
    }

    /// Open the archive prefix, validating the catalog and metadata.
    ///
    /// The buffer may also contain tile bytes, which are left untouched.
    pub fn open(prefix: &[u8]) -> Result<Self> {
        let header = Header::read(prefix)?;
        let catalog_len = header.catalog_len();
        let prefix_len = catalog_len + header.metadata_len;
        if prefix.len() < prefix_len {
            return Err(invalid(
                prefix.len() as u64,
                "truncated catalog or metadata",
            ));
        }
        let catalog_checksum = catalog_len - 4;
        if crc32c::crc32c(&prefix[..catalog_checksum]) != prefix.u32(catalog_checksum as u64)? {
            return Err(invalid(
                catalog_checksum as u64,
                "catalog checksum mismatch",
            ));
        }
        let metadata = inflate(
            &prefix[catalog_len..prefix_len],
            catalog_len as u64,
            MAX_METADATA_BYTES,
        )?;
        let sections = Sections::read(&metadata)?;
        let directory = Directory::read(sections.directory, sections.zooms()?)?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(header.tile_count)
            .map_err(|_| Error::Limit("GOB catalog allocation"))?;
        let mut offset = prefix_len as u64;
        for i in 0..header.tile_count {
            let p = (HEADER_BYTES + i * 8) as u64;
            let tip = prefix.u32(p)?;
            let len = prefix.u32(p + 4)?;
            if directory.bounds(tip).is_none() {
                return Err(invalid(p, "catalog entry points outside the tile tree"));
            }
            if (len as usize) < MIN_CHUNK_BYTES {
                return Err(invalid(p + 4, "catalog entry has a short chunk"));
            }
            entries.push(TileEntry { tip, offset, len });
            // The entry count and lengths are bounded by their header fields.
            offset += u64::from(len);
        }
        entries.sort_unstable_by_key(|entry| entry.tip);
        if entries.windows(2).any(|pair| pair[0].tip == pair[1].tip) {
            return Err(invalid(HEADER_BYTES as u64, "duplicate catalog tile"));
        }
        let gol_metadata = sections.as_gol_metadata(header)?;
        Ok(Self {
            archive_len: offset,
            revision: header.revision,
            tileset_id: header.tileset_id,
            entries,
            tiles: directory.tiles,
            metadata: gol_metadata,
        })
    }

    /// Total archive length declared by the chunk ranges, in bytes.
    pub fn archive_len(&self) -> u64 {
        self.archive_len
    }

    /// Replication sequence of this GOB snapshot.
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// Raw tileset ID. Corresponding GOLs carry the same ID at byte 64.
    pub fn tileset_id(&self) -> [u8; 16] {
        self.tileset_id
    }

    /// Number of compressed tile chunks in the archive.
    pub fn tile_count(&self) -> usize {
        self.entries.len()
    }

    /// Find the archived chunk for a tile ID.
    pub fn tile(&self, tip: u32) -> Option<TileEntry> {
        self.entries
            .binary_search_by_key(&tip, |entry| entry.tip)
            .ok()
            .map(|index| self.entries[index])
    }

    /// IDs of all tiles intersecting the projected bounds, in ascending order.
    ///
    /// Includes covering ancestors. In a partial archive, a returned ID may
    /// lack a corresponding [`TileEntry`]. Traversal of feature links can
    /// require additional tiles.
    pub fn tiles_in_bounds(&self, bounds: Bounds) -> Vec<u32> {
        self.tiles
            .iter()
            .enumerate()
            .filter_map(|(tip, &tile)| {
                tile_bounds(tile)
                    .filter(|tile| bounds.intersects(*tile))
                    .map(|_| tip as u32)
            })
            .collect()
    }

    pub(super) fn bounds(&self, tip: u32) -> Option<Bounds> {
        tile_bounds(*self.tiles.get(tip as usize)?)
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.metadata.capacity()
            + self.entries.capacity() * std::mem::size_of::<TileEntry>()
            + self.tiles.capacity() * std::mem::size_of::<u32>()
    }
}

#[derive(Clone, Copy)]
struct Header {
    tileset_id: [u8; 16],
    flags: u32,
    tile_count: usize,
    revision: u32,
    timestamp: [u8; 8],
    metadata_len: usize,
}

impl Header {
    fn read(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_BYTES {
            return Err(invalid(0, "truncated header"));
        }
        if bytes.u32(0)? != 0xe0f6_b060 {
            return Err(invalid(0, "invalid magic"));
        }
        if bytes.u32(4)? != 2 {
            return Err(invalid(4, "unsupported format version, expected 2.0"));
        }
        let flags = bytes.u32(24)?;
        if flags & !1 != 0 {
            return Err(invalid(24, "unsupported bundle flags"));
        }
        let tile_count = bytes.u32(28)? as usize;
        if tile_count > MAX_TILES {
            return Err(Error::Limit("GOB catalog entries"));
        }
        if bytes.u32(32)? != 0 {
            return Err(invalid(32, "incremental revision bundles are unsupported"));
        }
        let metadata_len = bytes.u32(48)? as usize;
        if metadata_len < MIN_CHUNK_BYTES {
            return Err(invalid(48, "truncated sealed metadata"));
        }
        if metadata_len > MAX_METADATA_BYTES {
            return Err(Error::Limit("compressed GOB metadata bytes"));
        }
        Ok(Self {
            tileset_id: bytes.array(8)?,
            flags,
            tile_count,
            revision: bytes.u32(36)?,
            timestamp: bytes.array(40)?,
            metadata_len,
        })
    }

    fn catalog_len(self) -> usize {
        HEADER_BYTES + self.tile_count * 8 + 4
    }
}

struct Sections<'a> {
    properties: &'a [u8],
    settings: &'a [u8],
    directory: &'a [u8],
    strings: &'a [u8],
    indexed_keys: &'a [u8],
}

impl<'a> Sections<'a> {
    fn read(bytes: &'a [u8]) -> Result<Self> {
        let mut known = [None; 5];
        let mut p = 0;
        while p < bytes.len() as u64 {
            let kind = bytes.u8(p)?;
            p += 1;
            let len = bytes
                .varint(&mut p)
                .map_err(|_| invalid(p, "invalid metadata section length"))?;
            let end = p
                .checked_add(len)
                .filter(|&end| end <= bytes.len() as u64)
                .ok_or_else(|| invalid(p, "truncated metadata section"))?;
            if (1..=5).contains(&kind) {
                let slot = &mut known[usize::from(kind - 1)];
                if slot.replace(&bytes[p as usize..end as usize]).is_some() {
                    return Err(invalid(p, "duplicate metadata section"));
                }
            }
            p = end;
        }
        let [
            Some(properties),
            Some(settings),
            Some(directory),
            Some(strings),
            Some(indexed_keys),
        ] = known
        else {
            return Err(invalid(0, "missing metadata section"));
        };
        if settings.len() != 12 {
            return Err(invalid(0, "invalid metadata settings size"));
        }
        let strings_count = string_table(strings, 1)?;
        if strings_count == 0 || strings.get(2) != Some(&0) {
            return Err(invalid(0, "missing empty global string"));
        }
        string_table(properties, 2)?;
        if indexed_keys.len() < 4 {
            return Err(invalid(0, "truncated key schema"));
        }
        let keys = indexed_keys.u32(0)?;
        if keys > 8190 || indexed_keys.len() != 4 + keys as usize * 4 {
            return Err(invalid(0, "invalid key schema size"));
        }
        for &word in indexed_keys[4..].as_chunks::<4>().0 {
            let word = u32::from_le_bytes(word);
            let key = word as u16;
            if key == 0 || key >= strings_count || key > 8190 || !(1..=30).contains(&(word >> 16)) {
                return Err(invalid(0, "invalid indexed key or category"));
            }
        }
        Ok(Self {
            properties,
            settings,
            directory,
            strings,
            indexed_keys,
        })
    }

    fn zooms(&self) -> Result<Vec<u8>> {
        let bits = self.settings.u32(0)?;
        if bits & 1 == 0 || bits & !0x1fff != 0 {
            return Err(invalid(0, "invalid tile zoom levels"));
        }
        let zooms: Vec<u8> = (0..=12).filter(|zoom| bits & (1 << zoom) != 0).collect();
        if zooms.windows(2).any(|pair| pair[1] - pair[0] > 3) {
            return Err(invalid(0, "tile zoom step exceeds three"));
        }
        Ok(zooms)
    }

    fn as_gol_metadata(&self, header: Header) -> Result<Vec<u8>> {
        let page = PAGE as usize;
        let mut p = page;
        let tables = [
            (88, self.indexed_keys),
            (84, self.strings),
            (92, self.properties),
        ];
        let len = tables.iter().map(|(_, table)| table.len()).sum::<usize>();
        let directory_start = (page + len).next_multiple_of(page);
        let total = directory_start + self.directory.len();
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total)
            .map_err(|_| Error::Limit("GOB store metadata allocation"))?;
        bytes.resize(page, 0);
        put_u32(&mut bytes, 0, 0x1ce5_0d6e);
        put_u32(&mut bytes, 4, 2);
        put_u32(&mut bytes, 20, len as u32);
        put_u32(&mut bytes, 24, total.div_ceil(page) as u32);
        bytes[64..80].copy_from_slice(&header.tileset_id);
        put_u32(&mut bytes, 80, header.flags);
        for (field, table) in tables {
            put_u32(&mut bytes, field, p as u32);
            bytes.extend_from_slice(table);
            p += table.len();
        }
        bytes[96..108].copy_from_slice(self.settings);
        put_u32(&mut bytes, 108, (self.directory.len() / 4 - 1) as u32);
        let metadata_crc = crc32c::crc32c(&bytes[page..]);
        put_u32(&mut bytes, 112, metadata_crc);
        put_u32(&mut bytes, 116, crc32c::crc32c(self.directory));
        put_u32(&mut bytes, 128, header.revision);
        bytes[136..144].copy_from_slice(&header.timestamp);
        put_u32(&mut bytes, 152, (directory_start / page) as u32);
        update_header_checksum(&mut bytes);
        bytes.resize(directory_start, 0);
        bytes.extend_from_slice(self.directory);
        Ok(bytes)
    }
}

fn string_table(bytes: &[u8], strings_per_entry: u32) -> Result<u16> {
    let count = bytes
        .u16(0)
        .map_err(|_| invalid(0, "missing string table count"))?;
    let mut p = 2;
    for _ in 0..u32::from(count) * strings_per_entry {
        bytes
            .next_string(&mut p)
            .map_err(|_| invalid(p, "invalid metadata string"))?;
    }
    if p != bytes.len() as u64 {
        return Err(invalid(p, "trailing metadata string bytes"));
    }
    Ok(count)
}

struct Directory<'a> {
    words: &'a [u8],
    zooms: Vec<u8>,
    tiles: Vec<u32>,
}

fn tile_bounds(tile: u32) -> Option<Bounds> {
    (tile < DIRECTORY_WORD)
        .then(|| Bounds::of_tile(tile & 0xfff, (tile >> 12) & 0xfff, (tile >> 24) as u8))
}

impl<'a> Directory<'a> {
    fn read(words: &'a [u8], zooms: Vec<u8>) -> Result<Self> {
        if words.len() < 8
            || !words.len().is_multiple_of(4)
            || words.u32(0)? as usize != words.len() - 4
        {
            return Err(invalid(0, "invalid tile directory length"));
        }
        let len = words.len() / 4;
        let mut tiles = Vec::new();
        tiles
            .try_reserve_exact(len)
            .map_err(|_| Error::Limit("GOB tile directory allocation"))?;
        tiles.resize(len, UNSEEN);
        tiles[0] = DIRECTORY_WORD;
        let mut directory = Self {
            words,
            zooms,
            tiles,
        };
        directory.visit(1, 0, 0, 0)?;
        Ok(directory)
    }

    fn bounds(&self, tip: u32) -> Option<Bounds> {
        tile_bounds(*self.tiles.get(tip as usize)?)
    }

    fn claim(&mut self, tip: usize) -> Result<u32> {
        let p = (tip as u64) * 4;
        let word = self
            .tiles
            .get_mut(tip)
            .filter(|word| **word == UNSEEN)
            .ok_or_else(|| invalid(p, "out-of-range or repeated directory word"))?;
        *word = DIRECTORY_WORD;
        self.words.u32(p)
    }

    fn visit(&mut self, mut tip: usize, level: usize, col: u32, row: u32) -> Result<()> {
        let zoom = self.zooms[level];
        let mut word = self.claim(tip)?;
        let branch = word & 3 == 1;
        if branch {
            let delta = (word as i32 & !3) / 4;
            tip = tip
                .checked_add_signed(delta as isize)
                .ok_or_else(|| invalid(tip as u64 * 4, "invalid tile branch offset"))?;
            word = self.claim(tip)?;
        }
        if word != 0 {
            return Err(invalid(
                tip as u64 * 4,
                "nonzero blank tile directory entry",
            ));
        }
        self.tiles[tip] = col | row << 12 | u32::from(zoom) << 24;

        if !branch && level != 0 {
            return Ok(());
        }
        let Some(&next_zoom) = self.zooms.get(level + 1) else {
            return if branch {
                Err(invalid(tip as u64 * 4, "tile branch exceeds zoom levels"))
            } else {
                Ok(())
            };
        };
        let extent = 1u32 << (next_zoom - zoom);
        let mut mask = u64::from(self.claim(tip + 1)?);
        let mut child = tip + 2;
        if extent == 8 {
            mask |= u64::from(self.claim(child)?) << 32;
            child += 1;
        } else if mask >> (extent * extent) != 0 {
            return Err(invalid(tip as u64 * 4 + 4, "invalid tile child mask"));
        }
        while mask != 0 {
            let bit = mask.trailing_zeros();
            mask &= mask - 1;
            self.visit(
                child,
                level + 1,
                col * extent + bit % extent,
                row * extent + bit / extent,
            )?;
            child += 1;
        }
        Ok(())
    }
}
