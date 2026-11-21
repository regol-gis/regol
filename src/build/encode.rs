// Write the spatial leaves and their tag tables first. Once every feature has
// a local anchor, bodies, backlinks and exports can refer to one another.
use super::{
    directory::Placement,
    stage::BuiltDataset,
    strings::{FxHashMap, StrMeta},
    varint,
};
use anyhow::{Result, ensure};
use std::cmp::Reverse;

pub(super) const MAX_KEY_INDEXES: u8 = 20;
// Narrower branch arrays prune smaller windows while keeping feature leaves
// large enough for sequential scans.
pub(super) const RTREE_BRANCH_FANOUT: usize = 8;
pub(super) const RTREE_LEAF_SIZE: usize = 32;

pub(super) fn put(bytes: &mut [u8], offset: usize, word: u32) {
    bytes[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
}
pub(super) fn string(bytes: &mut Vec<u8>, s: &str) {
    varint(bytes, s.len() as u64);
    bytes.extend(s.as_bytes());
}
#[inline]
fn delta(from: usize, to: usize) -> i32 {
    // GOL tile length (and hence any intra-tile distance) is below 1 GiB.
    to as i32 - from as i32
}
#[inline]
fn pointer(bytes: &mut [u8], from: usize, to: usize, flags: u32) {
    put(bytes, from, delta(from, to) as u32 | flags);
}
#[inline]
fn align(bytes: &mut Vec<u8>) {
    bytes.resize(bytes.len().next_multiple_of(4), 0);
}
#[inline]
fn word(bytes: &mut Vec<u8>, w: u32) {
    bytes.extend(w.to_le_bytes());
}

struct TagSet<'a> {
    tags: std::borrow::Cow<'a, [(u32, u32)]>,
    categories: u32,
    value_order: u32,
}

struct Record {
    feat_idx: u32,
    id: i64,
    flags: u16,
    tex: Option<usize>,
    tags: usize,
    categories: u32,
    spatial_order: u64,
    bounds: [i32; 4],
}
impl Record {
    #[inline]
    fn index(&self) -> usize {
        if self.flags & 2 != 0 {
            2
        } else {
            match self.id % 4 {
                0 => 0,
                1 => 1,
                _ => 3,
            }
        }
    }
}

// Hilbert order of feature centers keeps spatial neighbors together. At each
// bit the current quadrant supplies two index bits and orients the next one.
fn spatial_order(bounds: [i32; 4]) -> u64 {
    let center = |min, max| (((i64::from(min) + i64::from(max)) >> 1) - i64::from(i32::MIN)) as u32;
    let mut x = center(bounds[0], bounds[2]);
    let mut y = center(bounds[1], bounds[3]);
    let mut index = 0;
    for bit in (0..32).rev() {
        let east = (x >> bit) & 1;
        let north = (y >> bit) & 1;
        index = (index << 2) | u64::from((3 * east) ^ north);
        if north == 0 {
            if east != 0 {
                x = !x;
                y = !y;
            }
            std::mem::swap(&mut x, &mut y);
        }
    }
    index
}

#[inline]
fn tag_categories(tags: &[(u32, u32)], meta: &[StrMeta]) -> u32 {
    tags.iter()
        .fold(0u32, |mask, &(k, _)| mask | meta[k as usize].category_bit)
}

// Group values of the leading indexed key within a small spatial run.
// A feature's name is usually unique and provides no useful clustering.
#[inline]
fn tag_value_order(tags: &[(u32, u32)], meta: &[StrMeta], name_str_id: Option<u32>) -> u32 {
    tags.iter()
        .filter(|&&(k, _)| Some(k) != name_str_id)
        .filter_map(|&(k, v)| {
            let bit = meta[k as usize].category_bit;
            if bit == 0 {
                return None;
            }
            Some((bit.trailing_zeros() << 16) | u32::from(meta[v as usize].dict_code))
        })
        .min()
        .unwrap_or(u32::MAX)
}

struct Tile<'a> {
    bytes: Vec<u8>,
    strings: FxHashMap<u32, usize>,
    anchors: FxHashMap<u32, usize>,
    dataset: &'a BuiltDataset,
    placement: &'a Placement,
    tag_sets: &'a [TagSet<'a>],
    tables: Vec<Option<(usize, bool)>>,
    branch_fanout: usize,
    leaf_size: usize,
}

pub(super) fn tile(
    dataset: &BuiltDataset,
    placement: &Placement,
    tile_idx: usize,
    tip: u32,
    branch_fanout: usize,
    leaf_size: usize,
) -> Result<Vec<u8>> {
    let entries = placement.tile_slice(tile_idx);
    let mut tag_numbers = FxHashMap::<std::borrow::Cow<'_, [(u32, u32)]>, usize>::default();
    let mut tag_sets: Vec<TagSet<'_>> = Vec::new();
    let mut tag_scratch: Vec<(u32, u32)> = Vec::new();
    let mut records = Vec::with_capacity(entries.len());

    for &(feat_idx, dup) in entries {
        let fi = feat_idx as usize;
        let f = dataset.feature(fi)?;
        let ftags = dataset.read_tags_cow(f.tag_start, f.tag_len as usize, &mut tag_scratch)?;
        let tags = if let Some(&n) = tag_numbers.get(ftags.as_ref()) {
            n
        } else {
            let categories = tag_categories(ftags.as_ref(), &dataset.str_meta);
            let value_order =
                tag_value_order(ftags.as_ref(), &dataset.str_meta, dataset.name_str_id);
            let n = tag_sets.len();
            tag_sets.push(TagSet {
                tags: ftags.clone(),
                categories,
                value_order,
            });
            tag_numbers.insert(ftags, n);
            n
        };
        let tex = if placement.home_tip[fi] == tip && placement.tex[fi] != u32::MAX {
            Some(placement.tex[fi] as usize)
        } else {
            None
        };
        records.push(Record {
            feat_idx,
            id: f.obj_id,
            flags: dataset.flags[fi] | dup,
            tex,
            tags,
            categories: tag_sets[tags].categories,
            spatial_order: spatial_order(f.bounds),
            bounds: f.bounds,
        });
    }
    drop(tag_numbers);

    let mut tile = Tile {
        bytes: Vec::with_capacity(28 + entries.len() * 64),
        strings: FxHashMap::default(),
        anchors: FxHashMap::with_capacity_and_hasher(records.len(), Default::default()),
        dataset,
        placement,
        tag_sets: &tag_sets,
        tables: vec![None; tag_sets.len()],
        branch_fanout,
        leaf_size,
    };
    tile.bytes.resize(28, 0);
    tile.write_local_strings();
    records.sort_unstable_by_key(|r| (r.index(), r.categories, r.spatial_order, r.id));
    for group in records.chunk_by_mut(|a, b| a.index() == b.index()) {
        tile.index(group);
    }
    let export_count = records.iter().filter(|r| r.tex.is_some()).count();
    if export_count != 0 {
        word(&mut tile.bytes, export_count as u32);
        let exports = tile.bytes.len();
        pointer(&mut tile.bytes, 24, exports, 0);
        tile.bytes.resize(exports + 4 * export_count, 0);
        for r in &records {
            if let Some(tex) = r.tex {
                pointer(
                    &mut tile.bytes,
                    exports + 4 * tex,
                    tile.anchors[&r.feat_idx],
                    0,
                );
            }
        }
    }
    for r in &records {
        tile.body(r)?;
        ensure!(
            tile.bytes.len() < (1 << 30) - 4,
            "tile {tip} exceeds 1 GiB format limit"
        );
    }
    let payload = tile.bytes.len() as u32;
    put(&mut tile.bytes, 0, payload);
    let checksum = crc32c::crc32c(&tile.bytes);
    word(&mut tile.bytes, checksum);
    Ok(tile.bytes)
}

impl Tile<'_> {
    fn write_local_strings(&mut self) {
        let meta = &self.dataset.str_meta;
        // A local key's relative pointer has flag bits and needs alignment.
        // Write keys first so a spelling shared by a key and a value is aligned.
        for set in self.tag_sets {
            for &(key, _) in set.tags.as_ref() {
                if !meta[key as usize].has_key_code() {
                    self.intern(key);
                }
            }
        }
        // Value pointers are byte offsets. Pack their text outside the index
        // so a dictionary-only query can stream over records and tables.
        for set in self.tag_sets {
            for &(_, value) in set.tags.as_ref() {
                let m = meta[value as usize];
                if !m.has_dict_code() && m.number_kind == 0 && !self.strings.contains_key(&value) {
                    let offset = self.bytes.len();
                    string(&mut self.bytes, self.dataset.strings.get(value));
                    self.strings.insert(value, offset);
                }
            }
        }
        align(&mut self.bytes);
    }

    fn index(&mut self, records: &mut [Record]) {
        // Preserve the busiest exact masks. Low-frequency masks share a root
        // whose union may admit extra candidates but cannot hide a match.
        let limit = usize::from(MAX_KEY_INDEXES);
        let mut sizes: Vec<_> = records
            .chunk_by(|a, b| a.categories == b.categories)
            .map(|group| (Reverse(group.len()), group[0].categories))
            .collect();
        if sizes.len() > limit {
            sizes.select_nth_unstable(limit - 1);
            let tail = &sizes[limit - 1..];
            let union = tail.iter().fold(0, |mask, (_, bits)| mask | bits);
            let mut keep: Vec<_> = sizes[..limit - 1].iter().map(|&(_, mask)| mask).collect();
            keep.sort_unstable();
            for r in records.iter_mut() {
                if keep.binary_search(&r.categories).is_err() {
                    r.categories = union;
                }
            }
            records.sort_unstable_by_key(|r| (r.categories, r.spatial_order, r.id));
        }
        // Reorder only within a single Hilbert leaf, preserving all branch bounds.
        let leaf_size = self.leaf_size;
        let branch_fanout = self.branch_fanout;
        for group in records.chunk_by_mut(|a, b| a.categories == b.categories) {
            for nearby in group.chunks_mut(leaf_size) {
                nearby.sort_unstable_by_key(|r| {
                    (
                        self.tag_sets[r.tags].value_order,
                        r.tags,
                        r.spatial_order,
                        r.id,
                    )
                });
            }
        }
        let root = self.bytes.len();
        pointer(&mut self.bytes, 8 + records[0].index() * 4, root, 0);
        let same_mask = |a: &Record, b: &Record| a.categories == b.categories;
        let count = records.chunk_by(same_mask).count();
        self.bytes.resize(root + count * 8, 0);
        for (i, group) in records.chunk_by(same_mask).enumerate() {
            let slot = root + i * 8;
            let start = self.bytes.len();
            pointer(&mut self.bytes, slot, start, u32::from(i + 1 == count));
            put(&mut self.bytes, slot + 4, group[0].categories);
            let mut capacity = leaf_size;
            while group.len().div_ceil(capacity) > branch_fanout {
                capacity *= branch_fanout;
            }
            self.branches(group, capacity);
        }
    }

    // Emit pointers towards the leaves in file order, including the final short branch.
    fn branches(&mut self, records: &[Record], capacity: usize) {
        let base = self.bytes.len();
        let count = records.len().div_ceil(capacity);
        self.bytes.resize(base + 20 * count, 0);
        for (i, group) in records.chunks(capacity).enumerate() {
            let slot = base + i * 20;
            let target = self.bytes.len();
            let flags = u32::from(i + 1 == count) | if capacity == self.leaf_size { 2 } else { 0 };
            pointer(&mut self.bytes, slot, target, flags);
            let mut bounds = group[0].bounds;
            for r in group {
                bounds[0] = bounds[0].min(r.bounds[0]);
                bounds[1] = bounds[1].min(r.bounds[1]);
                bounds[2] = bounds[2].max(r.bounds[2]);
                bounds[3] = bounds[3].max(r.bounds[3]);
            }
            for (j, value) in bounds.into_iter().enumerate() {
                put(&mut self.bytes, slot + 4 + j * 4, value as u32);
            }
            if capacity > self.leaf_size {
                self.branches(group, capacity / self.branch_fanout);
            } else {
                self.leaf(group);
            }
        }
        // Sibling leaves form a contiguous record stream. Follow them with a
        // second stream of tag tables in first-use order for this branch.
        if capacity == self.leaf_size {
            self.write_tables(records);
        }
    }

    fn leaf(&mut self, records: &[Record]) {
        for (i, r) in records.iter().enumerate() {
            let node = r.id % 4 == 0;
            for coordinate in &r.bounds[..if node { 2 } else { 4 }] {
                word(&mut self.bytes, *coordinate as u32);
            }
            self.anchors.insert(r.feat_idx, self.bytes.len());
            let header =
                ((r.id as u64 >> 2) << 12) | u64::from(r.flags) | u64::from(i + 1 == records.len());
            self.bytes.extend(header.to_le_bytes());
            word(&mut self.bytes, 0); // tag table
            if !node || r.flags & 4 != 0 {
                word(&mut self.bytes, 0);
            }
        }
    }

    fn write_tables(&mut self, records: &[Record]) {
        for r in records {
            let (offset, local) = match self.tables[r.tags] {
                Some(table) => table,
                None => {
                    let table = self.tags(self.tag_sets[r.tags].tags.as_ref());
                    self.tables[r.tags] = Some(table);
                    table
                }
            };
            pointer(
                &mut self.bytes,
                self.anchors[&r.feat_idx] + 8,
                offset,
                u32::from(local),
            );
        }
        align(&mut self.bytes); // the next branch or leaf uses two pointer flag bits
    }

    fn intern(&mut self, str_id: u32) -> usize {
        if let Some(&offset) = self.strings.get(&str_id) {
            return offset;
        }
        align(&mut self.bytes); // also usable as a local tag key
        let offset = self.bytes.len();
        string(&mut self.bytes, self.dataset.strings.get(str_id));
        self.strings.insert(str_id, offset);
        offset
    }

    fn tags(&mut self, tags: &[(u32, u32)]) -> (usize, bool) {
        let mut local = Vec::new();
        let mut common = Vec::new();
        let meta = &self.dataset.str_meta;
        for &(key_id, val_id) in tags {
            let vm = meta[val_id as usize];
            let value = if vm.has_dict_code() {
                Value::Global(vm.dict_code)
            } else if let Some(n) = vm.number_value() {
                n
            } else {
                Value::Text(self.intern(val_id))
            };
            let km = meta[key_id as usize];
            if km.has_key_code() {
                common.push((km.key_code, value));
            } else {
                local.push((self.dataset.strings.get(key_id), self.intern(key_id), value));
            }
        }
        // Local entries run backwards; their key words use a shared aligned origin.
        align(&mut self.bytes);
        // Scanning backward must visit uncommon keys alphabetically for GOQL
        // matchers that consume multiple local-key predicates in one pass.
        local.sort_unstable_by(|a, b| b.0.cmp(a.0));
        let table = self.bytes.len() + local.iter().map(|(_, _, v)| 4 + v.width()).sum::<usize>();
        for (i, (_, key, value)) in local.iter().enumerate() {
            value.write(&mut self.bytes);
            word(
                &mut self.bytes,
                (delta(table & !3, *key) * 2) as u32 | value.kind() | if i == 0 { 4 } else { 0 },
            );
        }
        common.sort_unstable_by_key(|&(k, _)| k);
        if common.is_empty() {
            word(&mut self.bytes, 0x8001);
        }
        for (i, (code, value)) in common.iter().enumerate() {
            let flags = value.kind() as u16 | if i + 1 == common.len() { 0x8000 } else { 0 };
            self.bytes.extend((code << 2 | flags).to_le_bytes());
            value.write(&mut self.bytes);
        }
        (table, !local.is_empty())
    }

    fn body(&mut self, r: &Record) -> Result<()> {
        let fi = r.feat_idx as usize;
        let anchor = self.anchors[&r.feat_idx];
        align(&mut self.bytes);
        let parents = if r.flags & 4 != 0 {
            let targets = self
                .dataset
                .parent_links(fi)
                .iter()
                .map(|&parent_fi| self.target(parent_fi))
                .collect::<Result<Vec<_>>>()?;
            let start = self.bytes.len();
            self.write_link_targets(&targets, false);
            Some(start)
        } else {
            None
        };
        match r.id % 4 {
            0 => {
                if let Some(p) = parents {
                    pointer(&mut self.bytes, anchor + 12, p, 0);
                }
            }
            1 => {
                let fnodes = self.dataset.way_fnodes(fi);
                if !fnodes.is_empty() {
                    let targets = fnodes
                        .iter()
                        .map(|&node_fi| self.target(node_fi))
                        .collect::<Result<Vec<_>>>()?;
                    self.write_link_targets(&targets, true);
                }
                self.body_pointer(anchor, parents);
                self.dataset.append_way_coords(fi, &mut self.bytes)?;
            }
            _ => {
                let raw = self.dataset.rel_members(fi);
                let mut members = Vec::with_capacity(raw.len());
                for &(child_fi, role_id) in raw {
                    let rm = self.dataset.str_meta[role_id as usize];
                    let role = if rm.has_dict_code() {
                        Role::Code(rm.dict_code)
                    } else {
                        Role::Local(self.intern(role_id))
                    };
                    members.push((self.target(child_fi)?, role));
                }
                align(&mut self.bytes);
                self.body_pointer(anchor, parents);
                write_members(&mut self.bytes, &members);
            }
        }
        Ok(())
    }

    fn body_pointer(&mut self, anchor: usize, parents: Option<usize>) {
        if let Some(p) = parents {
            let pos = self.bytes.len();
            word(&mut self.bytes, delta(pos, p) as u32);
        }
        let body = self.bytes.len();
        pointer(&mut self.bytes, anchor + 12, body, 0);
    }

    #[inline]
    fn target(&self, feat_idx: u32) -> Result<Target> {
        if let Some(&p) = self.anchors.get(&feat_idx) {
            return Ok(Target::Local(p));
        }
        let i = feat_idx as usize;
        let tex = self.placement.tex[i];
        ensure!(
            tex != u32::MAX,
            "missing export index for foreign target {}",
            self.dataset.feature(i)?.obj_id
        );
        Ok(Target::Foreign {
            tip: self.placement.home_tip[i] as i32,
            tex: tex as i32,
        })
    }

    fn write_link_targets(&mut self, targets: &[Target], backwards: bool) {
        let len: usize = targets
            .iter()
            .map(|t| if matches!(t, Target::Local(_)) { 4 } else { 8 })
            .sum();
        let start = self.bytes.len();
        self.bytes.resize(start + len, 0);
        let mut cursor = if backwards { start + len } else { start };
        let (mut last_tip, mut last_tex) = (0x4000, 0x800);
        for (i, t) in targets.iter().enumerate() {
            let last = i32::from(i + 1 == targets.len());
            match *t {
                Target::Local(p) => {
                    if backwards {
                        cursor -= 4;
                        let w = ((delta(cursor + 2, p) * 2) | last) as u32;
                        put(&mut self.bytes, cursor, w.rotate_left(16));
                    } else {
                        put(
                            &mut self.bytes,
                            cursor,
                            ((delta(cursor, p) * 2) | last) as u32,
                        );
                        cursor += 4;
                    }
                }
                Target::Foreign { tip, tex } => {
                    let w = ((tex - last_tex) << 4 | 14 | last) as u32;
                    let d = (((tip - last_tip) * 2) | 1) as u32;
                    if backwards {
                        cursor -= 8;
                        put(&mut self.bytes, cursor, d.rotate_left(16));
                        put(&mut self.bytes, cursor + 4, w.rotate_left(16));
                    } else {
                        put(&mut self.bytes, cursor, w);
                        put(&mut self.bytes, cursor + 4, d);
                        cursor += 8;
                    }
                    (last_tip, last_tex) = (tip, tex);
                }
            }
        }
    }
}

fn write_members(bytes: &mut Vec<u8>, members: &[(Target, Role)]) {
    let (mut last_tip, mut last_tex) = (0x4000, 0x400);
    let mut last_role = Role::Code(0); // the initial role is the empty string
    for (i, &(target, role)) in members.iter().enumerate() {
        let changed_role = role != last_role;
        let flags = (i32::from(changed_role) << 2) | i32::from(i + 1 == members.len());
        let pos = bytes.len();
        match target {
            Target::Local(p) => word(bytes, ((delta(pos & !3, p) * 2) | flags) as u32),
            Target::Foreign { tip, tex } => {
                let changed_tile = tip != last_tip;
                tagged_delta(
                    bytes,
                    (tex - last_tex) << 5 | 2 | flags | (i32::from(changed_tile) << 3),
                    16,
                );
                if changed_tile {
                    tagged_delta(bytes, (tip - last_tip) << 1, 1);
                }
                (last_tip, last_tex) = (tip, tex);
            }
        }
        if changed_role {
            let pos = bytes.len();
            match role {
                Role::Code(c) => bytes.extend((c * 2 + 1).to_le_bytes()),
                Role::Local(p) => word(bytes, (delta(pos, p) * 2) as u32),
            }
            last_role = role;
        }
    }
}

// Foreign member references and changed tile numbers both have a signed
// 16-bit form and a 32-bit form selected by a low flag bit.
fn tagged_delta(bytes: &mut Vec<u8>, encoded: i32, wide_bit: i32) {
    if let Ok(narrow) = i16::try_from(encoded) {
        bytes.extend(narrow.to_le_bytes());
    } else {
        word(bytes, (encoded | wide_bit) as u32);
    }
}

#[derive(Clone, Copy)]
enum Target {
    Local(usize),
    Foreign { tip: i32, tex: i32 },
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Code(u16),
    Local(usize),
}
pub(super) enum Value {
    Narrow(u16),
    Global(u16),
    Wide(u32),
    Text(usize),
}
impl Value {
    fn kind(&self) -> u32 {
        match self {
            Self::Narrow(_) => 0,
            Self::Global(_) => 1,
            Self::Wide(_) => 2,
            Self::Text(_) => 3,
        }
    }
    fn width(&self) -> usize {
        2 + (self.kind() & 2) as usize
    }
    fn write(&self, bytes: &mut Vec<u8>) {
        match *self {
            Self::Narrow(v) | Self::Global(v) => bytes.extend(v.to_le_bytes()),
            Self::Wide(v) => word(bytes, v),
            Self::Text(p) => {
                let pos = bytes.len();
                word(bytes, delta(pos, p) as u32);
            }
        }
    }
}
pub(super) fn number(s: &str) -> Option<Value> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len == 0 || len > 12 {
        return None;
    }
    let negative = bytes[0] == b'-';
    let mut i = usize::from(negative);
    let int_start = i;
    let mut magnitude = 0u64;
    while i < len && bytes[i].is_ascii_digit() {
        magnitude = magnitude * 10 + u64::from(bytes[i] - b'0');
        i += 1;
    }
    let int_len = i - int_start;
    if int_len == 0 || (int_len > 1 && bytes[int_start] == b'0') {
        return None;
    }
    let scale = if i < len {
        if bytes[i] != b'.' {
            return None;
        }
        i += 1;
        let frac_start = i;
        while i < len && bytes[i].is_ascii_digit() {
            magnitude = magnitude * 10 + u64::from(bytes[i] - b'0');
            i += 1;
        }
        let frac_len = i - frac_start;
        if i != len || frac_len == 0 || frac_len > 3 {
            return None;
        }
        frac_len as u8
    } else {
        0
    };
    // Canonical TagValue::Number never formats zero with a leading minus sign.
    if negative && magnitude == 0 {
        return None;
    }
    let mantissa = if negative {
        if magnitude > 256 {
            return None;
        }
        -(magnitude as i32)
    } else {
        let max_mantissa = (1u64 << 30) - 257;
        if magnitude > max_mantissa {
            return None;
        }
        magnitude as i32
    };
    let n = (mantissa + 256) as u32;
    if scale == 0 && n <= 65535 {
        Some(Value::Narrow(n as u16))
    } else {
        Some(Value::Wide(n << 2 | u32::from(scale)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_parser_matches_canonical_tag_value_round_trip() {
        let cases = [
            "",
            "-",
            "+",
            ".",
            "-.",
            "0.",
            "1.",
            ".5",
            "-.5",
            "0",
            "-0",
            "00",
            "01",
            "-01",
            "-0.0",
            "0.0",
            "0.00",
            "0.000",
            "0.0000",
            "-0.1",
            "-0.01",
            "-0.001",
            "-0.256",
            "-0.257",
            "-256",
            "-257",
            "65279",
            "65280",
            "1073741567",
            "1073741568",
            "1073741.567",
            "1073741.568",
            "12.34",
            "12.340",
            "12.3456",
            "+12",
            "1e3",
            "123456789012",
            "1234567890123",
            "00.5",
            "-00.5",
            "3.4",
            "100",
            "-1",
        ];
        for s in cases {
            let reference = (|| -> Option<(u8, u32)> {
                if !matches!(s.as_bytes().first(), Some(b'0'..=b'9' | b'-')) {
                    return None;
                }
                let (integer, fraction) = s.split_once('.').unwrap_or((s, ""));
                if s.len() > 12 || fraction.len() > 3 || s.starts_with('+') || s.ends_with('.') {
                    return None;
                }
                let scale = fraction.len() as u8;
                let mantissa: i32 = format!("{integer}{fraction}").parse().ok()?;
                let n = u32::try_from(mantissa.checked_add(256)?).ok()?;
                if n >= 1 << 30 || (crate::TagValue::Number { mantissa, scale }).to_string() != s {
                    return None;
                }
                if scale == 0 && n <= 65535 {
                    Some((0, n))
                } else {
                    Some((2, n << 2 | u32::from(scale)))
                }
            })();
            let actual = number(s).map(|v| match v {
                Value::Narrow(n) => (0, u32::from(n)),
                Value::Wide(w) => (2, w),
                _ => unreachable!(),
            });
            assert_eq!(actual, reference, "case {s:?}");
        }
    }

    #[test]
    fn hilbert_order_visits_adjacent_cells_across_the_coordinate_range() {
        let mut cells = Vec::new();
        for x in 0..8i32 {
            for y in 0..8i32 {
                let coordinate = |cell: i32| ((cell as u32) << 29 ^ (1 << 31)) as i32;
                let (px, py) = (coordinate(x), coordinate(y));
                cells.push((spatial_order([px, py, px, py]), x, y));
            }
        }
        cells.sort_unstable();
        for pair in cells.windows(2) {
            let [(_, x1, y1), (_, x2, y2)] = pair else {
                unreachable!()
            };
            assert_eq!(x1.abs_diff(*x2) + y1.abs_diff(*y2), 1);
        }
    }

    #[test]
    fn foreign_members_use_implicit_roles_tiles_and_signed_deltas() {
        let mut bytes = Vec::new();
        write_members(
            &mut bytes,
            &[
                (
                    Target::Foreign {
                        tip: 0x4000,
                        tex: 0x400,
                    },
                    Role::Code(0),
                ),
                (
                    Target::Foreign {
                        tip: 0x4000,
                        tex: 0x401,
                    },
                    Role::Code(5),
                ),
                (
                    Target::Foreign {
                        tip: 0x4001,
                        tex: 0x400,
                    },
                    Role::Code(5),
                ),
                (
                    Target::Foreign {
                        tip: 0x9000,
                        tex: 0x8400,
                    },
                    Role::Code(0),
                ),
            ],
        );
        let mut expected = Vec::new();
        for w in [2i16, 38, 11, -22, 2] {
            expected.extend(w.to_le_bytes());
        }
        expected.extend(((0x8000i32 << 5) | 31).to_le_bytes());
        expected.extend(((0x4fffi32 << 1) | 1).to_le_bytes());
        expected.extend(1u16.to_le_bytes());
        assert_eq!(bytes, expected);
    }
}
