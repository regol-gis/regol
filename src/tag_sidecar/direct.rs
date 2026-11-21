use super::{invalid, u32_at};
use crate::{
    Feature, Library, Predicate, PreparedQuery, Query, Result, Selector, Types, decode::ReadWindow,
    feature::TileExtent, source::MappedFile,
};
use std::io::Write;

pub(super) const DENSE: u64 = u64::MAX;
const DIRECT_COUNT_LIMIT: u64 = 250_000;

/// Build direct nationwide runs with the ordinary query, including category
/// gates and the unbounded duplicate policy. Bounded queries can need a
/// different copy of a feature and use the physical leaf postings instead.
pub(super) fn write_runs(
    output: &mut impl Write,
    gol: &Library<MappedFile>,
    pairs: &[(String, String)],
    matches: &[u64],
    mut written: u64,
) -> Result<u64> {
    let mut ranges = Vec::with_capacity(pairs.len());
    let mut offsets = Vec::new();
    for ((key, value), &matches) in pairs.iter().zip(matches) {
        if matches > DIRECT_COUNT_LIMIT {
            ranges.push((0, DENSE));
            continue;
        }
        let start = written;
        if matches != 0 {
            let query =
                Query::new(Selector::new(Types::ALL).and(Predicate::sidecar_exact(key, value)));
            let prepared = query.prepare(gol)?;
            let mut tile: Option<TileExtent> = None;
            for feature in prepared.iter() {
                let feature = feature?;
                if let Some(previous) = tile.filter(|tile| tile.start() != feature.tile_start()) {
                    write_tile(output, previous, &offsets, &mut written)?;
                    offsets.clear();
                }
                tile = Some(feature.tile);
                offsets.push((feature.anchor() - feature.tile_start()) as u32);
            }
            if let Some(tile) = tile {
                write_tile(output, tile, &offsets, &mut written)?;
                offsets.clear();
            }
        }
        ranges.push((start, written - start));
    }
    for (start, length) in ranges {
        output.write_all(&start.to_le_bytes())?;
        output.write_all(&length.to_le_bytes())?;
        written += 16;
    }
    Ok(written)
}

fn write_tile(
    output: &mut impl Write,
    tile: TileExtent,
    offsets: &[u32],
    written: &mut u64,
) -> Result<()> {
    let page = (tile.start() >> 12) as u32;
    let length = (tile.end() - tile.start()) as u32;
    let count =
        u32::try_from(offsets.len()).map_err(|_| invalid("direct posting group is too long"))?;
    super::put_words(output, &[page, length, count], written)?;
    Ok(super::put_words(output, offsets, written)?)
}

pub(super) struct Direct<'a, 'p, 'q> {
    prepared: &'p PreparedQuery<'a, 'q, MappedFile>,
    gol: &'a Library<MappedFile>,
    groups: &'a [u8],
    offsets: &'a [u8],
    tile: TileExtent,
    tags: ReadWindow<'a, MappedFile>,
    strings: ReadWindow<'a, MappedFile>,
    done: bool,
}

impl<'a, 'p, 'q> Direct<'a, 'p, 'q> {
    pub(super) fn new(
        prepared: &'p PreparedQuery<'a, 'q, MappedFile>,
        gol: &'a Library<MappedFile>,
        groups: &'a [u8],
    ) -> Self {
        Self {
            prepared,
            gol,
            groups,
            offsets: &[],
            tile: TileExtent::default(),
            tags: ReadWindow::new(gol.source()),
            strings: ReadWindow::new(gol.source()),
            done: false,
        }
    }

    fn next_match(&mut self) -> Result<Option<Feature<'a, MappedFile>>> {
        loop {
            if let Some((offset, rest)) = self.offsets.split_first_chunk::<4>() {
                self.offsets = rest;
                let anchor = self.tile.start() + u64::from(u32::from_le_bytes(*offset));
                let feature = Feature::at_source(self.gol, self.gol.source(), anchor, self.tile)?;
                if feature.flags() & 0x800 == 0
                    && self.prepared.matches_sidecar_candidate(
                        feature,
                        &mut self.tags,
                        &mut self.strings,
                    )?
                {
                    return Ok(Some(feature));
                }
            } else if self.groups.is_empty() {
                return Ok(None);
            } else {
                let (header, rest) = self
                    .groups
                    .split_first_chunk::<12>()
                    .ok_or_else(|| invalid("short direct tile header"))?;
                let start = u64::from(u32_at(header)) << 12;
                self.tile = TileExtent::new(start, start + u64::from(u32_at(&header[4..])))?;
                let count = (u32_at(&header[8..]) as usize)
                    .checked_mul(4)
                    .ok_or_else(|| invalid("direct posting too large"))?;
                (self.offsets, self.groups) = rest
                    .split_at_checked(count)
                    .ok_or_else(|| invalid("short direct tile group"))?;
            }
        }
    }
}

impl<'a> Iterator for Direct<'a, '_, '_> {
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
