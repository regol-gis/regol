use crate::{Error, FeatureType, Library, Result, decode::Decode, source::Source};

const PAGE: u64 = 4096;
const SEGMENT: u64 = 1 << 30;

/// Counts observed after a complete, successful check of the active snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrityReport {
    /// Number of verified resident feature tiles.
    pub tiles: u32,
    /// Number of logical OSM features visited by the spatial index.
    pub features: u64,
}

impl<S: Source> Library<S> {
    /// Replication sequence stored in the active snapshot header. Zero may mean unspecified.
    pub fn revision(&self) -> Result<u64> {
        let offset = 128 + u64::from(self.active_snapshot()) * 64;
        Ok(u64::from(self.source.u32(offset)?))
    }

    /// Tag keys in the library's spatial index schema, in declaration order.
    pub fn indexed_keys(&self) -> impl Iterator<Item = Result<std::borrow::Cow<'_, str>>> {
        self.categories
            .iter()
            .map(|&(code, _)| self.global_string(code))
    }

    /// Verify the active snapshot's header, metadata, tile directory and tile checksums,
    /// allocated extents, R-tree bounds, and reachable feature records, tags, vertices and links.
    ///
    /// Requires complete coverage and stable file contents. A missing tile, bad checksum,
    /// malformed field or exceeded traversal limit returns an error. Verification visits
    /// direct relation members without recursively assembling their geometry. It does not
    /// validate OSM polygon topology, historical snapshots, free pages or updater-only indexes.
    pub fn check_integrity(&self) -> Result<IntegrityReport> {
        let source = &self.source;
        let mut buffer = vec![0; 64 * 1024];
        let header = source.array::<508>(0)?;
        if crc32c::crc32c(&header[..504]) != source.u32(504)? {
            return Err(Error::invalid(504, "header checksum mismatch"));
        }

        let pages = u64::from(source.u32(24)?);
        if pages == 0 || pages > source.len().div_ceil(PAGE) {
            return Err(Error::invalid(24, "allocated pages extend beyond file"));
        }
        let metadata_end = PAGE + u64::from(source.u32(20)?);
        let directory_size = self.tile_index_end - self.tile_index;
        if directory_size > SEGMENT
            || self.tile_index / SEGMENT != (self.tile_index_end - 1) / SEGMENT
            || directory_size - 4 != u64::from(source.u32(108)?) * 4
        {
            return Err(Error::invalid(
                self.tile_index,
                "invalid tile directory size",
            ));
        }
        if metadata_end > self.tile_index || self.tile_index_end > pages * PAGE {
            return Err(Error::invalid(
                self.tile_index,
                "overlapping or unallocated metadata and directory",
            ));
        }
        let meta_crc = checksum(source, PAGE, metadata_end, &mut buffer, |_, _| {})?;
        if meta_crc != source.u32(112)? {
            return Err(Error::invalid(112, "metadata checksum mismatch"));
        }
        check_metadata_sections(source, metadata_end)?;
        self.dictionary()?;
        self.indexed_keys().try_for_each(|key| key.map(drop))?;
        self.properties()?
            .try_for_each(|property| property.map(drop))?;

        // Only resident page words are masked in the immutable catalog checksum.
        // Sorting their locations lets us hash the directory in bounded-size reads.
        let mut tiles = Vec::new();
        self.features().visit_tile_pages(|entry, start| {
            if tiles.len() >= self.tile_count() as usize {
                return Err(Error::invalid(
                    entry,
                    "more tiles than the active snapshot declares",
                ));
            }
            tiles.push((entry, start));
            Ok(())
        })?;
        if tiles.len() != self.tile_count() as usize {
            return Err(Error::invalid(self.tile_index, "tile count mismatch"));
        }
        tiles.sort_unstable();
        if tiles.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(Error::invalid(self.tile_index, "repeated directory entry"));
        }
        let mut locations = tiles.iter().peekable();
        let catalog_crc = checksum(
            source,
            self.tile_index,
            self.tile_index_end,
            &mut buffer,
            |offset, bytes| {
                while let Some(&&(entry, _)) = locations.peek() {
                    if entry >= offset + bytes.len() as u64 {
                        break;
                    }
                    let index = (entry - offset) as usize;
                    bytes[index..index + 4].fill(0);
                    locations.next();
                }
            },
        )?;
        if catalog_crc != source.u32(116)? {
            return Err(Error::invalid(116, "tile directory checksum mismatch"));
        }

        tiles.sort_unstable_by_key(|&(_, start)| start);
        let mut spatial = self.features();
        let mut previous_end = metadata_end;
        for (_, start) in tiles {
            let length = u64::from(source.u32(start)? & 0x3fff_ffff);
            let end = start + length + 4;
            if length < 28
                || start < previous_end
                || end > pages * PAGE
                || start / SEGMENT != (end - 1) / SEGMENT
                || (start < self.tile_index_end && self.tile_index < end)
            {
                return Err(Error::invalid(start, "invalid or overlapping tile extent"));
            }
            let crc = checksum(source, start, end - 4, &mut buffer, |_, _| {})?;
            if crc != source.u32(end - 4)? {
                return Err(Error::invalid(end - 4, "tile checksum mismatch"));
            }
            spatial.verify_tile_tree_bounds(start, end - 4)?;
            previous_end = end;
        }

        let mut features = 0;
        for feature in self.features() {
            let feature = feature?;
            feature.bounds()?;
            feature.tags()?.try_for_each(|tag| tag.map(drop))?;
            feature
                .parent_relations()?
                .try_for_each(|parent| parent.map(drop))?;
            match feature.kind() {
                FeatureType::Node => {}
                FeatureType::Way => {
                    feature
                        .feature_nodes()?
                        .try_for_each(|node| node.map(drop))?;
                    feature.way_nodes()?.try_for_each(|node| node.map(drop))?;
                }
                FeatureType::Relation => {
                    feature.members()?.try_for_each(|member| member.map(drop))?
                }
            }
            features += 1;
        }
        Ok(IntegrityReport {
            tiles: self.tile_count(),
            features,
        })
    }
}

fn check_metadata_sections(source: &impl Source, end: u64) -> Result<()> {
    // Table pointers alone do not ensure that the header, entries and
    // payload all fit inside the checksummed metadata.
    let mut tables = [
        (u64::from(source.u32(84)?), 84),
        (u64::from(source.u32(88)?), 88),
        (u64::from(source.u32(92)?), 92),
    ];
    for (start, field) in tables {
        if !(field == 92 && start == 0 || (PAGE..end).contains(&start)) {
            return Err(Error::invalid(field, "metadata pointer outside metadata"));
        }
    }
    tables.sort_unstable();
    for (i, &(start, field)) in tables.iter().enumerate() {
        if start == 0 {
            continue;
        }
        let limit = tables.get(i + 1).map_or(end, |&(start, _)| start);
        let invalid = || Error::invalid(start, "metadata table exceeds its allocated range");
        if field == 88 {
            if start + 4 > limit || start + 4 + 4 * u64::from(source.u32(start)?) > limit {
                return Err(invalid());
            }
            continue;
        }
        if start + 2 > limit {
            return Err(invalid());
        }
        let strings_per_entry = if field == 92 { 2 } else { 1 };
        let count = u64::from(source.u16(start)?) * strings_per_entry;
        let mut offset = start + 2;
        for _ in 0..count {
            if offset >= limit {
                return Err(invalid());
            }
            let length = source.varint(&mut offset)?;
            offset = offset
                .checked_add(length)
                .filter(|&end| end <= limit)
                .ok_or_else(invalid)?;
        }
    }
    Ok(())
}

fn checksum(
    source: &impl Source,
    mut offset: u64,
    end: u64,
    buffer: &mut [u8],
    mut normalize: impl FnMut(u64, &mut [u8]),
) -> Result<u32> {
    source.check(offset, end - offset)?;
    let mut crc = 0;
    while offset < end {
        let length = (end - offset).min(buffer.len() as u64) as usize;
        let chunk = &mut buffer[..length];
        source.read_exact_at(offset, chunk)?;
        normalize(offset, chunk);
        crc = crc32c::crc32c_append(crc, chunk);
        offset += length as u64;
    }
    Ok(crc)
}
