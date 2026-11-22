use super::{Body, Feature, Link, Member, Role, Tags, Tile, Value, decode, push};
use crate::{Bounds, Error, Result};
use std::cmp::Reverse;

const MAX_BLOB_SIZE: usize = 1 << 30;
const INDEX_LIMIT: usize = 20;
const BRANCH_FANOUT: usize = 8;
const LEAF_SIZE: usize = 32;

pub(super) fn write(tile: Tile<'_>, max_output_bytes: usize) -> Result<Vec<u8>> {
    let limit = MAX_BLOB_SIZE.min(max_output_bytes);
    if limit < 32 {
        return Err(Error::Limit("converted GOB tile bytes"));
    }
    Encoder {
        limit,
        bytes: vec![0; 28],
        strings: zero_offsets(tile.strings.len())?,
        tags: Vec::new(),
        parents: Vec::new(),
        anchors: zero_offsets(tile.features.len())?,
    }
    .tile(&tile)
}

fn zero_offsets(len: usize) -> Result<Vec<usize>> {
    let mut offsets = Vec::new();
    offsets
        .try_reserve_exact(len)
        .map_err(|_| Error::Limit("GOB tile offset allocation"))?;
    offsets.resize(len, 0);
    Ok(offsets)
}

struct Encoder {
    limit: usize,
    bytes: Vec<u8>,
    strings: Vec<usize>,
    tags: Vec<usize>,
    parents: Vec<usize>,
    anchors: Vec<usize>,
}

impl Encoder {
    fn tile(mut self, tile: &Tile<'_>) -> Result<Vec<u8>> {
        self.strings(tile)?;
        self.indexes(tile)?;
        self.exports(&tile.exports)?;
        for table in &tile.tags {
            let anchor = self.tag_table(table)?;
            push(&mut self.tags, anchor)?;
        }
        self.align(4)?;
        for parents in &tile.parents {
            push(&mut self.parents, self.bytes.len())?;
            self.links(parents, false)?;
        }
        for (index, feature) in tile.features.iter().enumerate() {
            let anchor = self.anchors[index];
            let flags = u32::from(!tile.tags[feature.tags].local.is_empty());
            ptr(&mut self.bytes, anchor + 8, self.tags[feature.tags], flags);
            self.body(feature, anchor)?;
        }
        let payload_len = self.bytes.len();
        self.reserve(4)?;
        put(&mut self.bytes, 0, payload_len as u32);
        let crc = crc32c::crc32c(&self.bytes);
        word(&mut self.bytes, crc);
        Ok(self.bytes)
    }

    fn reserve(&mut self, additional: usize) -> Result<()> {
        let required = self
            .bytes
            .len()
            .checked_add(additional)
            .filter(|&n| n <= self.limit)
            .ok_or(Error::Limit("converted GOB tile bytes"))?;
        if required > self.bytes.capacity() {
            let capacity = (self.bytes.capacity() * 2).max(required).min(self.limit);
            self.bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .map_err(|_| Error::Limit("converted GOB tile bytes"))?;
        }
        Ok(())
    }

    fn align(&mut self, alignment: usize) -> Result<()> {
        let len = self.bytes.len();
        self.zeros(len.next_multiple_of(alignment) - len)?;
        Ok(())
    }

    fn zeros(&mut self, len: usize) -> Result<usize> {
        self.reserve(len)?;
        let pos = self.bytes.len();
        self.bytes.resize(pos + len, 0);
        Ok(pos)
    }

    fn strings(&mut self, tile: &Tile<'_>) -> Result<()> {
        let mut is_key = vec![false; tile.strings.len()];
        for table in &tile.tags {
            for &(key, _) in &table.local {
                is_key[key] = true;
            }
        }
        for (number, string) in tile.strings.iter().enumerate() {
            self.reserve(string.len() + 5)?;
            if is_key[number] {
                self.align(4)?;
            }
            self.strings[number] = self.bytes.len();
            varint(&mut self.bytes, string.len() as u64);
            self.bytes.extend_from_slice(string);
        }
        self.align(4)?;
        Ok(())
    }

    fn indexes(&mut self, tile: &Tile<'_>) -> Result<()> {
        let mut groups: [Vec<Record>; 4] = std::array::from_fn(|_| Vec::new());
        for (index, feature) in tile.features.iter().enumerate() {
            push(
                &mut groups[feature.index()],
                Record {
                    index,
                    mask: tile.tags[feature.tags].categories,
                    order: spatial_order(feature.bounds),
                },
            )?;
        }
        for (index, group) in groups.iter_mut().enumerate() {
            if !group.is_empty() {
                self.index(index, group, &tile.features)?;
            }
        }
        Ok(())
    }

    fn index(
        &mut self,
        number: usize,
        records: &mut [Record],
        features: &[Feature<'_>],
    ) -> Result<()> {
        records.sort_unstable_by_key(|r| (r.mask, r.order, r.index));
        // Preserve frequent masks. For the others, a union mask admits extra
        // candidates when testing indexed keys without excluding any matches.
        let mut masks: Vec<_> = records
            .chunk_by(|a, b| a.mask == b.mask)
            .map(|group| (Reverse(group.len()), group[0].mask))
            .collect();
        if masks.len() > INDEX_LIMIT {
            masks.select_nth_unstable(INDEX_LIMIT - 1);
            let (keep, merge) = masks.split_at_mut(INDEX_LIMIT - 1);
            let bits = merge.iter().fold(0, |all, &(_, mask)| all | mask);
            keep.sort_unstable_by_key(|&(_, mask)| mask);
            for record in records.iter_mut() {
                if keep
                    .binary_search_by_key(&record.mask, |&(_, m)| m)
                    .is_err()
                {
                    record.mask = bits;
                }
            }
            records.sort_unstable_by_key(|r| (r.mask, r.order, r.index));
        }

        let same_mask = |a: &Record, b: &Record| a.mask == b.mask;
        let count = records.chunk_by(same_mask).count();
        let roots = self.zeros(count * 8)?;
        ptr(&mut self.bytes, 8 + number * 4, roots, 0);
        for (i, group) in records.chunk_by(same_mask).enumerate() {
            let target = self.bytes.len();
            let root = roots + i * 8;
            ptr(&mut self.bytes, root, target, u32::from(i + 1 == count));
            put(&mut self.bytes, root + 4, group[0].mask);
            let mut capacity = LEAF_SIZE;
            while group.len().div_ceil(capacity) > BRANCH_FANOUT {
                capacity *= BRANCH_FANOUT;
            }
            self.branches(group, features, capacity)?;
        }
        Ok(())
    }

    fn branches(
        &mut self,
        group: &[Record],
        features: &[Feature<'_>],
        capacity: usize,
    ) -> Result<()> {
        let count = group.len().div_ceil(capacity);
        let branches = self.zeros(20 * count)?;
        let leaves = capacity == LEAF_SIZE;
        for (index, records) in group.chunks(capacity).enumerate() {
            let flags = (u32::from(leaves) << 1) | u32::from(index + 1 == count);
            let branch = branches + index * 20;
            let target = self.bytes.len();
            ptr(&mut self.bytes, branch, target, flags);
            let bbox = records
                .iter()
                .map(|r| features[r.index].bounds)
                .reduce(decode::union)
                .expect("nonempty branch");
            for (i, v) in coordinates(bbox).into_iter().enumerate() {
                put(&mut self.bytes, branch + 4 + i * 4, v as u32);
            }
            if leaves {
                self.leaf(records, features)?;
            } else {
                self.branches(records, features, capacity / BRANCH_FANOUT)?;
            }
        }
        Ok(())
    }

    fn leaf(&mut self, records: &[Record], features: &[Feature<'_>]) -> Result<()> {
        for (index, record) in records.iter().enumerate() {
            let feature = &features[record.index];
            let node = matches!(feature.body, Body::Node);
            let start = self.zeros(if !node {
                32
            } else if feature.parents.is_some() {
                24
            } else {
                20
            })?;
            let coord_count = if node { 2 } else { 4 };
            for (i, v) in coordinates(feature.bounds)
                .into_iter()
                .take(coord_count)
                .enumerate()
            {
                put(&mut self.bytes, start + i * 4, v as u32);
            }
            let anchor = start + 4 * coord_count;
            self.anchors[record.index] = anchor;
            let header = (feature.id << 12)
                | u64::from(feature.flags)
                | u64::from(index + 1 == records.len());
            self.bytes[anchor..anchor + 8].copy_from_slice(&header.to_le_bytes());
        }
        Ok(())
    }

    fn exports(&mut self, exports: &[Option<usize>]) -> Result<()> {
        if !exports.is_empty() {
            let total = exports
                .len()
                .checked_add(1)
                .and_then(|len| len.checked_mul(4))
                .ok_or(Error::Limit("converted GOB export table bytes"))?;
            let start = self.zeros(total)?;
            let first = start + 4;
            ptr(&mut self.bytes, 24, first, 0);
            put(&mut self.bytes, start, exports.len() as u32);
            for (i, slot) in exports.iter().enumerate() {
                if let Some(index) = slot {
                    ptr(&mut self.bytes, first + i * 4, self.anchors[*index], 0);
                }
            }
        }
        Ok(())
    }

    fn tag_table(&mut self, table: &Tags) -> Result<usize> {
        let local_bytes = table
            .local
            .iter()
            .map(|&(_, value)| 4 + value.width())
            .sum::<usize>();
        let global_bytes = table
            .global
            .iter()
            .map(|&(_, value)| 2 + value.width())
            .sum::<usize>();
        self.reserve(local_bytes + global_bytes)?;
        let anchor = self.bytes.len() + local_bytes;
        for (i, &(key, value)) in table.local.iter().rev().enumerate() {
            self.value(value);
            let flags = value.kind() | (u32::from(i == 0) << 2);
            let key = ((delta(anchor & !3, self.strings[key]) * 2) as u32) | flags;
            word(&mut self.bytes, key);
        }
        for (i, &(key, value)) in table.global.iter().enumerate() {
            let last = if i + 1 == table.global.len() {
                0x8000
            } else {
                0
            };
            half(&mut self.bytes, (key << 2) | value.kind() as u16 | last);
            self.value(value);
        }
        Ok(anchor)
    }

    fn value(&mut self, value: Value) {
        match value {
            Value::Narrow(n) | Value::Global(n) => half(&mut self.bytes, n),
            Value::Wide(n) => word(&mut self.bytes, n),
            Value::Local(n) => {
                let distance = delta(self.bytes.len(), self.strings[n]);
                word(&mut self.bytes, distance as u32);
            }
        }
    }

    fn body(&mut self, feature: &Feature<'_>, anchor: usize) -> Result<()> {
        if matches!(feature.body, Body::Node) {
            if let Some(index) = feature.parents {
                ptr(&mut self.bytes, anchor + 12, self.parents[index], 0);
            }
            return Ok(());
        }
        self.reserve(5)?;
        self.align(2)?;
        if let Body::Way { nodes, .. } = &feature.body {
            self.links(nodes, true)?;
        }
        if let Some(index) = feature.parents {
            self.reserve(4)?;
            let distance = delta(self.bytes.len(), self.parents[index]);
            word(&mut self.bytes, distance as u32);
        }
        let start = self.bytes.len();
        ptr(&mut self.bytes, anchor + 12, start, 0);
        match &feature.body {
            Body::Node => unreachable!("node body was already written"),
            Body::Way {
                count,
                first,
                deltas_and_ids,
                ..
            } => {
                let count = u64::from(*count);
                let x = zigzag(i64::from(first.x) - i64::from(feature.bounds.min.x));
                let y = zigzag(i64::from(first.y) - i64::from(feature.bounds.min.y));
                self.reserve(
                    varint_len(count) + varint_len(x) + varint_len(y) + deltas_and_ids.len(),
                )?;
                varint(&mut self.bytes, count);
                varint(&mut self.bytes, x);
                varint(&mut self.bytes, y);
                self.bytes.extend_from_slice(deltas_and_ids);
            }
            Body::Relation(members) => self.members(members)?,
        }
        Ok(())
    }

    fn links(&mut self, links: &[Link], backwards: bool) -> Result<()> {
        if backwards {
            let len: usize = links.iter().map(|link| link.width(false)).sum();
            let start = self.zeros(len)?;
            let mut cursor = start + len;
            for (index, &link) in links.iter().enumerate() {
                let last = if index + 1 == links.len() { 1 } else { 0 };
                // In a way's node table each 16-bit part, including the low
                // part of a delta, is consumed from right to left.
                let end = cursor;
                cursor -= link.width(false);
                let low = end - 2;
                match link {
                    Link::Local(n) => {
                        let relative = (delta(low, self.anchors[n]) * 2) | last;
                        self.bytes[cursor..end]
                            .copy_from_slice(&(relative as u32).rotate_left(16).to_le_bytes());
                    }
                    Link::Foreign {
                        export_delta,
                        tile_delta,
                    } => {
                        let changed_tile = i32::from(tile_delta.is_some()) << 2;
                        let exported = (export_delta << 4) | 2 | changed_tile | last;
                        let next = put_backwards_delta(&mut self.bytes, end, exported, 8);
                        if let Some(tile_delta) = tile_delta {
                            put_backwards_delta(&mut self.bytes, next, tile_delta << 1, 1);
                        }
                    }
                }
            }
        } else {
            for (index, &link) in links.iter().enumerate() {
                let last = i32::from(index + 1 == links.len());
                self.reference(link, last, false)?;
            }
        }
        Ok(())
    }

    fn members(&mut self, members: &[Member]) -> Result<()> {
        if members.is_empty() {
            self.zeros(4)?;
        }
        for (i, member) in members.iter().enumerate() {
            let last = i32::from(i + 1 == members.len());
            let role_flag = if member.role.is_some() { 4 } else { 0 };
            self.reference(member.link, last | role_flag, true)?;
            match member.role {
                Some(Role::Global(code)) => {
                    self.reserve(2)?;
                    half(&mut self.bytes, (code << 1) | 1);
                }
                Some(Role::Local(n)) => {
                    self.reserve(4)?;
                    let relative = delta(self.bytes.len(), self.strings[n]) * 2;
                    word(&mut self.bytes, relative as u32);
                }
                None => {}
            }
        }
        Ok(())
    }

    fn reference(&mut self, link: Link, flags: i32, member: bool) -> Result<()> {
        self.reserve(link.width(member))?;
        match link {
            Link::Local(n) => {
                let mut base = self.bytes.len();
                if member {
                    base &= !3;
                }
                let relative = delta(base, self.anchors[n]) * 2;
                word(&mut self.bytes, (relative | flags) as u32);
            }
            Link::Foreign {
                export_delta,
                tile_delta,
            } => {
                let shift = if member { 5 } else { 4 };
                let changed_tile = i32::from(tile_delta.is_some()) << (shift - 2);
                let exported = (export_delta << shift) | 2 | changed_tile | flags;
                push_tagged_delta(&mut self.bytes, exported, 1 << (shift - 1));
                if let Some(tile_delta) = tile_delta {
                    push_tagged_delta(&mut self.bytes, tile_delta << 1, 1);
                }
            }
        }
        Ok(())
    }
}

struct Record {
    index: usize,
    mask: u32,
    order: u64,
}

fn coordinates(b: Bounds) -> [i32; 4] {
    [b.min.x, b.min.y, b.max.x, b.max.y]
}

// Interleave the bits of the feature center to group nearby features in each
// leaf. Branch bounds use the complete feature extents.
fn spatial_order(bounds: Bounds) -> u64 {
    fn spread(min: i32, max: i32) -> u64 {
        let mid = (i64::from(min) + i64::from(max)) >> 1;
        let mut n = (mid - i64::from(i32::MIN)) as u64;
        n = (n | n << 16) & 0x0000_ffff_0000_ffff;
        n = (n | n << 8) & 0x00ff_00ff_00ff_00ff;
        n = (n | n << 4) & 0x0f0f_0f0f_0f0f_0f0f;
        n = (n | n << 2) & 0x3333_3333_3333_3333;
        (n | n << 1) & 0x5555_5555_5555_5555
    }
    spread(bounds.min.x, bounds.max.x) | (spread(bounds.min.y, bounds.max.y) << 1)
}

fn push_tagged_delta(bytes: &mut Vec<u8>, encoded: i32, wide_bit: i32) {
    if let Ok(narrow) = i16::try_from(encoded) {
        half(bytes, narrow as u16);
    } else {
        word(bytes, (encoded | wide_bit) as u32);
    }
}

fn put_backwards_delta(bytes: &mut [u8], end: usize, encoded: i32, wide_bit: i32) -> usize {
    if let Ok(narrow) = i16::try_from(encoded) {
        bytes[end - 2..end].copy_from_slice(&narrow.to_le_bytes());
        end - 2
    } else {
        let wide = ((encoded | wide_bit) as u32).rotate_left(16);
        put(bytes, end - 4, wide);
        end - 4
    }
}

fn delta(from: usize, to: usize) -> i32 {
    to as i32 - from as i32
}

fn ptr(bytes: &mut [u8], pos: usize, target: usize, flags: u32) {
    put(bytes, pos, delta(pos, target) as u32 | flags);
}

fn put(bytes: &mut [u8], pos: usize, value: u32) {
    bytes[pos..pos + 4].copy_from_slice(&value.to_le_bytes());
}

fn word(bytes: &mut Vec<u8>, n: u32) {
    bytes.extend(n.to_le_bytes());
}

fn half(bytes: &mut Vec<u8>, n: u16) {
    bytes.extend(n.to_le_bytes());
}

fn zigzag(n: i64) -> u64 {
    ((n as u64) << 1) ^ ((n >> 63) as u64)
}

fn varint_len(n: u64) -> usize {
    (u64::BITS - n.leading_zeros()).max(1).div_ceil(7) as usize
}

fn varint(bytes: &mut Vec<u8>, mut n: u64) {
    while n >= 128 {
        bytes.push(n as u8 | 128);
        n >>= 7;
    }
    bytes.push(n as u8);
}
