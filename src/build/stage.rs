//! In-memory and transparently spilled staging + parallel OSM reference/geometry resolution.
use super::{
    BuildOptions, BuildReport,
    dictionary::{Dictionary, is_countable_word},
    signed,
    spill::{
        BitRank, BlobWriter, BytesStore, MemoryBudget, PAGE_SIZE, PagedBlob, PodCodec, PodStore,
        RawSpillSink, SpillNodeChunk, SpillTagChunk, SpillWayChunk, read_file_exact_at,
        sequential_reader,
    },
    strings::{StrMeta, StringPool},
    varint, worker_count,
};
#[cfg(test)]
use super::{Tags, object_id};
use anyhow::{Result, ensure};
use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::BinaryHeap,
    fs::File,
    io::Read,
    sync::atomic::{AtomicU8, AtomicU64, Ordering},
};

const NODE_TAGGED: u8 = 1 << 0;
const NODE_SHARED_XY: u8 = 1 << 1;
const NODE_REL_MEMBER: u8 = 1 << 2;
const NODE_USED_BY_WAY: u8 = 1 << 3;
const NODE_IS_FEATURE: u8 = 1 << 4;
const NODE_WAY_LINK: u8 = 1 << 5;

const NODES_PER_PAGE: usize = PAGE_SIZE / 16; // 4,096 16-byte RawNodes per 64 KiB page
const TAG_SPILL_COUNT: usize = 256_000;

#[derive(Clone, Copy)]
pub(super) struct RawNode {
    pub id: i64,
    pub x: i32,
    pub y: i32,
}

impl PodCodec for RawNode {
    const SIZE: usize = 16;

    #[inline]
    fn encode(&self, dst: &mut [u8]) {
        dst[0..8].copy_from_slice(&self.id.to_le_bytes());
        dst[8..12].copy_from_slice(&self.x.to_le_bytes());
        dst[12..16].copy_from_slice(&self.y.to_le_bytes());
    }

    #[inline]
    fn decode(src: &[u8]) -> Self {
        Self {
            id: i64::from_le_bytes(src[0..8].try_into().unwrap()),
            x: i32::from_le_bytes(src[8..12].try_into().unwrap()),
            y: i32::from_le_bytes(src[12..16].try_into().unwrap()),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct RawTaggedNode {
    pub id: i64,
    pub tag_start: usize,
    pub tag_len: u32,
}

#[derive(Clone, Copy)]
pub(super) struct RawWay {
    pub id: i64,
    pub tag_start: usize,
    pub ref_start: usize,
    pub tag_len: u32,
    pub ref_len: u32,
}

impl PodCodec for RawWay {
    const SIZE: usize = 32;

    #[inline]
    fn encode(&self, dst: &mut [u8]) {
        dst[0..8].copy_from_slice(&self.id.to_le_bytes());
        dst[8..16].copy_from_slice(&(self.tag_start as u64).to_le_bytes());
        dst[16..24].copy_from_slice(&(self.ref_start as u64).to_le_bytes());
        dst[24..28].copy_from_slice(&self.tag_len.to_le_bytes());
        dst[28..32].copy_from_slice(&self.ref_len.to_le_bytes());
    }

    #[inline]
    fn decode(src: &[u8]) -> Self {
        Self {
            id: i64::from_le_bytes(src[0..8].try_into().unwrap()),
            tag_start: u64::from_le_bytes(src[8..16].try_into().unwrap()) as usize,
            ref_start: u64::from_le_bytes(src[16..24].try_into().unwrap()) as usize,
            tag_len: u32::from_le_bytes(src[24..28].try_into().unwrap()),
            ref_len: u32::from_le_bytes(src[28..32].try_into().unwrap()),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct RawRelation {
    pub id: i64,
    pub tag_start: usize,
    pub mem_start: usize,
    pub tag_len: u32,
    pub mem_len: u32,
}

#[derive(Clone, Copy)]
pub(super) struct RawMember {
    pub child_id: i64,
    pub role: u32,
    pub kind: u8,
}

/// Per-worker staging buffer collected during parallel PBF decoding.
pub(super) struct WorkerStage {
    pub strings: StringPool,
    pub freqs: Vec<(u64, u64)>,
    pub countable: Vec<bool>,
    pub node_chunks: Vec<(usize, bool, Vec<RawNode>)>,
    pub tagged_nodes: Vec<RawTaggedNode>,
    pub ways: Vec<RawWay>,
    pub way_refs: Vec<i64>,
    pub relations: Vec<RawRelation>,
    pub rel_members: Vec<RawMember>,
    pub tags: Vec<(u32, u32)>,
    pub tags_base: usize,
    pub spilled_node_chunks: Vec<SpillNodeChunk>,
    pub spilled_way_chunks: Vec<SpillWayChunk>,
    pub spilled_tag_chunks: Vec<SpillTagChunk>,
    accounted_bytes: u64,
}

impl WorkerStage {
    pub(super) fn new() -> Self {
        Self {
            strings: StringPool::new(),
            freqs: Vec::new(),
            countable: Vec::new(),
            node_chunks: Vec::new(),
            tagged_nodes: Vec::new(),
            ways: Vec::new(),
            way_refs: Vec::new(),
            relations: Vec::new(),
            rel_members: Vec::new(),
            tags: Vec::new(),
            tags_base: 0,
            spilled_node_chunks: Vec::new(),
            spilled_way_chunks: Vec::new(),
            spilled_tag_chunks: Vec::new(),
            accounted_bytes: 0,
        }
    }

    #[inline]
    pub(super) fn next_tag_offset(&self) -> usize {
        self.tags_base + self.tags.len()
    }

    #[inline]
    pub(super) fn intern(&mut self, s: &str) -> u32 {
        let (id, is_new) = self.strings.intern_ex(s);
        if is_new {
            self.freqs.push((0, 0));
            self.countable.push(is_countable_word(s));
        }
        id
    }

    #[inline]
    pub(super) fn count(&mut self, id: u32, is_key: bool) {
        let i = id as usize;
        if self.countable[i] {
            let entry = &mut self.freqs[i];
            entry.0 += u64::from(is_key);
            entry.1 += 1;
        }
    }

    fn buffered_bytes(&self) -> u64 {
        let node_bytes: u64 = self
            .node_chunks
            .iter()
            .map(|c| (c.2.len() as u64) * 16)
            .sum();
        node_bytes
            + (self.ways.len() as u64) * 32
            + (self.way_refs.len() as u64) * 8
            + (self.tags.len() as u64) * 8
            + (self.tagged_nodes.len() as u64) * 24
            + (self.relations.len() as u64) * 32
            + (self.rel_members.len() as u64) * 16
    }

    /// Account for newly buffered bytes and transparently spill buffered raw
    /// nodes, ways, way_refs, and tags to disk once the memory budget is reached.
    pub(super) fn maybe_spill(
        &mut self,
        budget: &MemoryBudget,
        sink: &RawSpillSink,
        force: bool,
    ) -> Result<()> {
        if !force && !budget.is_spilling() {
            let cur = self.buffered_bytes();
            let delta = cur.saturating_sub(self.accounted_bytes);
            self.accounted_bytes = cur;
            if !budget.account(delta) {
                return Ok(());
            }
        }
        if !force && !budget.is_spilling() {
            return Ok(());
        }

        // Flush node chunks immediately when spilling.
        if !self.node_chunks.is_empty() {
            let mut raw = Vec::new();
            for (seq, sorted, chunk) in self.node_chunks.drain(..) {
                if chunk.is_empty() {
                    continue;
                }
                let (mut min_id, mut max_id) = (i64::MAX, i64::MIN);
                raw.clear();
                raw.resize(chunk.len() * RawNode::SIZE, 0);
                for (i, n) in chunk.iter().enumerate() {
                    min_id = min_id.min(n.id);
                    max_id = max_id.max(n.id);
                    n.encode(&mut raw[i * RawNode::SIZE..(i + 1) * RawNode::SIZE]);
                }
                let meta =
                    sink.spill_nodes(budget, seq, sorted, min_id, max_id, &raw, chunk.len())?;
                self.spilled_node_chunks.push(meta);
            }
        }

        // Flush ways + way_refs immediately per block when spilling so chunks
        // stay non-overlapping by way ID for sorted OSM PBFs.
        if !self.ways.is_empty() {
            let mut min_id = i64::MAX;
            let mut wbuf = vec![0u8; self.ways.len() * RawWay::SIZE];
            for (i, w) in self.ways.iter().enumerate() {
                min_id = min_id.min(w.id);
                w.encode(&mut wbuf[i * RawWay::SIZE..(i + 1) * RawWay::SIZE]);
            }
            let mut rbuf = vec![0u8; self.way_refs.len() * 8];
            for (i, &r) in self.way_refs.iter().enumerate() {
                rbuf[i * 8..(i + 1) * 8].copy_from_slice(&r.to_le_bytes());
            }
            let meta = sink.spill_ways(
                budget,
                min_id,
                &wbuf,
                self.ways.len(),
                &rbuf,
                self.way_refs.len(),
            )?;
            self.spilled_way_chunks.push(meta);
            self.ways.clear();
            self.way_refs.clear();
            if force {
                self.ways.shrink_to_fit();
                self.way_refs.shrink_to_fit();
            }
        }

        // Flush tags in chunks, advancing `tags_base` so logical tag offsets stay exact.
        if !self.tags.is_empty() && (force || self.tags.len() >= TAG_SPILL_COUNT) {
            let mut tbuf = vec![0u8; self.tags.len() * 8];
            for (i, pair) in self.tags.iter().enumerate() {
                pair.encode(&mut tbuf[i * 8..(i + 1) * 8]);
            }
            let meta = sink.spill_tags(budget, &tbuf, self.tags.len())?;
            self.spilled_tag_chunks.push(meta);
            self.tags_base += self.tags.len();
            self.tags.clear();
            if force {
                self.tags.shrink_to_fit();
            }
        }

        self.accounted_bytes = self.buffered_bytes();
        Ok(())
    }
}

/// Merged raw PBF data prior to reference resolution.
pub(super) struct RawStage {
    workers: Vec<WorkerStage>,
    strings: StringPool,
    freqs: Vec<(u64, u64)>,
    remaps: Vec<Vec<u32>>,
    spill_sink: RawSpillSink,
}

impl RawStage {
    pub(super) fn from_workers(mut workers: Vec<WorkerStage>, spill_sink: RawSpillSink) -> Self {
        if workers.len() == 1 {
            let mut w = workers.pop().unwrap();
            let strings = std::mem::replace(&mut w.strings, StringPool::new());
            let freqs = std::mem::take(&mut w.freqs);
            w.countable = Vec::new();
            return Self {
                workers: vec![w],
                strings,
                freqs,
                remaps: Vec::new(),
                spill_sink,
            };
        }
        let mut strings = StringPool::new();
        let mut freqs: Vec<(u64, u64)> = Vec::new();
        let mut remaps = Vec::with_capacity(workers.len());
        for w in &mut workers {
            let mut remap = Vec::with_capacity(w.strings.len());
            for local_id in 0..w.strings.len() {
                let s = w.strings.get(local_id as u32);
                let (gid, is_new) = strings.intern_ex(s);
                if is_new {
                    freqs.push((0, 0));
                }
                let f = w.freqs[local_id];
                let entry = &mut freqs[gid as usize];
                entry.0 += f.0;
                entry.1 += f.1;
                remap.push(gid);
            }
            w.strings = StringPool::new();
            w.freqs = Vec::new();
            w.countable = Vec::new();
            remaps.push(remap);
        }
        Self {
            workers,
            strings,
            freqs,
            remaps,
            spill_sink,
        }
    }

    pub(super) fn has_spilled(&self) -> bool {
        self.workers.iter().any(|w| {
            !w.spilled_node_chunks.is_empty()
                || !w.spilled_way_chunks.is_empty()
                || !w.spilled_tag_chunks.is_empty()
        })
    }

    pub(super) fn build_dictionary(&self, options: BuildOptions) -> Dictionary {
        Dictionary::build(&self.strings, &self.freqs, options)
    }

    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self {
            workers: vec![WorkerStage::new()],
            strings: StringPool::new(),
            freqs: Vec::new(),
            remaps: Vec::new(),
            spill_sink: RawSpillSink::new(),
        }
    }

    #[cfg(test)]
    pub(super) fn add_node(&mut self, id: i64, lon: f64, lat: f64, tags: &Tags) -> Result<()> {
        let (x, y) = project(id, lon, lat)?;
        object_id(0, id)?;
        let (tag_start, tag_len) = self.push_test_tags(tags);
        let w = &mut self.workers[0];
        if w.node_chunks.is_empty() {
            w.node_chunks.push((0, true, Vec::new()));
        }
        let (_, sorted, nodes) = &mut w.node_chunks[0];
        if nodes.last().is_some_and(|last| id <= last.id) {
            *sorted = false;
        }
        nodes.push(RawNode { id, x, y });
        if tag_len > 0 {
            w.tagged_nodes.push(RawTaggedNode {
                id,
                tag_start,
                tag_len,
            });
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn add_way(&mut self, id: i64, tags: &Tags, refs: Vec<i64>) -> Result<()> {
        object_id(1, id)?;
        for &n in &refs {
            object_id(0, n)?;
        }
        let (tag_start, tag_len) = self.push_test_tags(tags);
        let w = &mut self.workers[0];
        let ref_start = w.way_refs.len();
        let ref_len = refs.len() as u32;
        w.way_refs.extend(refs);
        w.ways.push(RawWay {
            id,
            tag_start,
            ref_start,
            tag_len,
            ref_len,
        });
        Ok(())
    }

    #[cfg(test)]
    fn push_test_tags(&mut self, tags: &Tags) -> (usize, u32) {
        let start = self.workers[0].tags.len();
        for (k, v) in tags.keys.iter().zip(&tags.values) {
            let kid = self.intern_and_count(k, true);
            let vid = self.intern_and_count(v, false);
            self.workers[0].tags.push((kid, vid));
        }
        (start, tags.keys.len() as u32)
    }

    #[cfg(test)]
    fn intern_and_count(&mut self, s: &str, is_key: bool) -> u32 {
        let (id, is_new) = self.strings.intern_ex(s);
        if is_new {
            self.freqs.push((0, 0));
        }
        if is_countable_word(s) {
            let entry = &mut self.freqs[id as usize];
            entry.0 += u64::from(is_key);
            entry.1 += 1;
        }
        id
    }
}

pub(super) fn project(id: i64, lon: f64, lat: f64) -> Result<(i32, i32)> {
    ensure!(
        (-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat),
        "invalid coordinates for node {id}"
    );
    // Keep the builder's persisted rounding and latitude cutoffs stable.
    use crate::coordinates::WORLD_WIDTH as WIDTH;
    let x = (lon * WIDTH / 360.0).round() as i32;
    let lat = lat.clamp(-85.05112878, 85.051128776);
    let y = (((lat + 90.0) * std::f64::consts::PI / 360.0).tan().ln() * WIDTH
        / std::f64::consts::TAU)
        .round() as i32;
    Ok((x, y))
}

#[derive(Clone, Copy)]
pub(super) struct FeatureRec {
    pub obj_id: i64,
    pub bounds: [i32; 4],
    pub tag_start: usize,
    pub tag_len: u32,
}

impl PodCodec for FeatureRec {
    const SIZE: usize = 36;

    #[inline]
    fn encode(&self, dst: &mut [u8]) {
        dst[0..8].copy_from_slice(&self.obj_id.to_le_bytes());
        dst[8..12].copy_from_slice(&self.bounds[0].to_le_bytes());
        dst[12..16].copy_from_slice(&self.bounds[1].to_le_bytes());
        dst[16..20].copy_from_slice(&self.bounds[2].to_le_bytes());
        dst[20..24].copy_from_slice(&self.bounds[3].to_le_bytes());
        dst[24..32].copy_from_slice(&(self.tag_start as u64).to_le_bytes());
        dst[32..36].copy_from_slice(&self.tag_len.to_le_bytes());
    }

    #[inline]
    fn decode(src: &[u8]) -> Self {
        Self {
            obj_id: i64::from_le_bytes(src[0..8].try_into().unwrap()),
            bounds: [
                i32::from_le_bytes(src[8..12].try_into().unwrap()),
                i32::from_le_bytes(src[12..16].try_into().unwrap()),
                i32::from_le_bytes(src[16..20].try_into().unwrap()),
                i32::from_le_bytes(src[20..24].try_into().unwrap()),
            ],
            tag_start: u64::from_le_bytes(src[24..32].try_into().unwrap()) as usize,
            tag_len: u32::from_le_bytes(src[32..36].try_into().unwrap()),
        }
    }
}

/// Complete resolved dataset (in-memory or transparently disk-backed) shared
/// read-only during directory build and parallel tile encoding.
pub(super) struct BuiltDataset {
    pub strings: StringPool,
    pub str_meta: Vec<StrMeta>,
    pub name_str_id: Option<u32>,
    pub tags: PodStore<(u32, u32)>,
    pub features: PodStore<FeatureRec>,
    pub flags: Vec<u16>,
    /// Index of the first way in `features` (`0..way_base` are nodes).
    pub way_base: usize,
    /// Index of the first relation in `features` (`way_base..rel_base` are ways).
    pub rel_base: usize,
    /// Flat delta-encoded way coordinates + node IDs, indexed by `way_coord_spans`.
    pub way_coords: BytesStore,
    pub way_coord_spans: PodStore<(u64, u32)>,
    /// Feature indices of way vertices that are also GOL node features.
    pub way_fnodes: Vec<u32>,
    pub way_fnode_offsets: Vec<u32>,
    /// Surviving relation members `(child_feat_idx, role_str_id)`.
    pub rel_members: Vec<(u32, u32)>,
    pub rel_member_offsets: Vec<usize>,
    /// Deduplicated sorted parent relation `feat_idx` for each feature.
    pub parent_links: Vec<u32>,
    pub parent_offsets: Vec<u32>,
}

impl BuiltDataset {
    #[inline]
    pub(super) fn feature(&self, fi: usize) -> Result<FeatureRec> {
        self.features.get(fi)
    }

    #[inline]
    pub(super) fn read_tags_cow<'a>(
        &'a self,
        start: usize,
        len: usize,
        scratch: &mut Vec<(u32, u32)>,
    ) -> Result<Cow<'a, [(u32, u32)]>> {
        match &self.tags {
            PodStore::Mem(v) => Ok(Cow::Borrowed(&v[start..start + len])),
            PodStore::Disk { .. } => {
                let slice = self.tags.slice_or_read(start, len, scratch)?;
                Ok(Cow::Owned(slice.to_vec()))
            }
        }
    }

    #[cfg(test)]
    pub(super) fn feature_tags_vec(&self, fi: usize) -> Result<Vec<(u32, u32)>> {
        let f = self.feature(fi)?;
        let mut scratch = Vec::new();
        Ok(self
            .tags
            .slice_or_read(f.tag_start, f.tag_len as usize, &mut scratch)?
            .to_vec())
    }

    #[inline]
    pub(super) fn append_way_coords(&self, fi: usize, dst: &mut Vec<u8>) -> Result<()> {
        let wi = fi - self.way_base;
        let (start, len) = self.way_coord_spans.get(wi)?;
        self.way_coords.append_range(start, len as usize, dst)
    }

    #[inline]
    pub(super) fn way_fnodes(&self, fi: usize) -> &[u32] {
        let wi = fi - self.way_base;
        &self.way_fnodes
            [self.way_fnode_offsets[wi] as usize..self.way_fnode_offsets[wi + 1] as usize]
    }

    #[inline]
    pub(super) fn rel_members(&self, fi: usize) -> &[(u32, u32)] {
        let ri = fi - self.rel_base;
        &self.rel_members[self.rel_member_offsets[ri]..self.rel_member_offsets[ri + 1]]
    }

    #[inline]
    pub(super) fn parent_links(&self, fi: usize) -> &[u32] {
        &self.parent_links[self.parent_offsets[fi] as usize..self.parent_offsets[fi + 1] as usize]
    }

    #[inline]
    pub(super) fn body_weight_bytes(&self, fi: usize) -> Result<u64> {
        if fi >= self.way_base && fi < self.rel_base {
            let wi = fi - self.way_base;
            let coords = u64::from(self.way_coord_spans.get(wi)?.1);
            let fnodes = (self.way_fnode_offsets[wi + 1] - self.way_fnode_offsets[wi]) as u64;
            Ok(coords + fnodes * 8)
        } else {
            Ok(0)
        }
    }
}

/// Fast bucketed lookup index over a sorted `RawNode` slice.
struct NodeIndex<'a> {
    nodes: &'a [RawNode],
    min_id: i64,
    max_id: i64,
    shift: u32,
    buckets: Vec<usize>,
}

impl<'a> NodeIndex<'a> {
    fn build(nodes: &'a [RawNode]) -> Self {
        if nodes.is_empty() {
            return Self {
                nodes,
                min_id: 0,
                max_id: -1,
                shift: 0,
                buckets: Vec::new(),
            };
        }
        let min_id = nodes[0].id;
        let max_id = nodes[nodes.len() - 1].id;
        let span = (max_id - min_id) as u64;
        let num_buckets = (nodes.len() / 8).next_power_of_two().clamp(1, 1 << 20);
        let mut shift = 0u32;
        while (span >> shift) >= num_buckets as u64 {
            shift += 1;
        }
        let mut buckets = vec![0usize; num_buckets + 1];
        let mut cur_bucket = 0usize;
        for (i, n) in nodes.iter().enumerate() {
            let b = ((n.id - min_id) as u64 >> shift) as usize;
            while cur_bucket < b {
                cur_bucket += 1;
                buckets[cur_bucket] = i;
            }
        }
        for slot in &mut buckets[cur_bucket + 1..] {
            *slot = nodes.len();
        }
        Self {
            nodes,
            min_id,
            max_id,
            shift,
            buckets,
        }
    }

    #[inline]
    fn find(&self, id: i64) -> Option<usize> {
        if id < self.min_id || id > self.max_id {
            return None;
        }
        let b = ((id - self.min_id) as u64 >> self.shift) as usize;
        let lo = self.buckets[b];
        let hi = self.buckets[b + 1];
        self.nodes[lo..hi]
            .binary_search_by_key(&id, |n| n.id)
            .ok()
            .map(|off| lo + off)
    }
}

#[inline]
fn hash_xy(x: i32, y: i32) -> u64 {
    let mut v = (u64::from(x as u32) << 32) | u64::from(y as u32);
    v ^= v >> 30;
    v = v.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    v ^= v >> 27;
    v = v.wrapping_mul(0x94d0_49bb_1331_11eb);
    v ^ (v >> 31)
}

fn mark_shared_xy(nodes: &[RawNode], node_flags: &[AtomicU8], workers: usize) {
    if nodes.len() < 2 {
        return;
    }
    let slots = (nodes.len().saturating_mul(4))
        .next_power_of_two()
        .clamp(1024, 1 << 26);
    let mask = slots - 1;
    // Pack 32 two-bit saturating counters per AtomicU64 (16 MiB for 64M slots).
    let counts: Vec<AtomicU64> = (0..slots / 32).map(|_| AtomicU64::new(0)).collect();

    // Pass 1: 2-bit saturating counting filter (zero false negatives).
    let chunk_size = nodes.len().div_ceil(workers).max(4096);
    if workers <= 1 || nodes.len() <= chunk_size {
        for n in nodes {
            let s = (hash_xy(n.x, n.y) as usize) & mask;
            let word = &counts[s >> 5];
            let shift = (s & 31) * 2;
            let v = word.load(Ordering::Relaxed);
            let c = (v >> shift) & 3;
            if c < 2 {
                word.store(
                    (v & !(3u64 << shift)) | ((c + 1) << shift),
                    Ordering::Relaxed,
                );
            }
        }
    } else {
        std::thread::scope(|scope| {
            for chunk in nodes.chunks(chunk_size) {
                let counts = &counts;
                scope.spawn(move || {
                    for n in chunk {
                        let s = (hash_xy(n.x, n.y) as usize) & mask;
                        let word = &counts[s >> 5];
                        let shift = (s & 31) * 2;
                        if ((word.load(Ordering::Relaxed) >> shift) & 3) < 2 {
                            #[allow(deprecated)]
                            // Keep fetch_update for the minimum supported Rust version.
                            let _ = word.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                                let c = (v >> shift) & 3;
                                (c < 2).then_some((v & !(3u64 << shift)) | ((c + 1) << shift))
                            });
                        }
                    }
                });
            }
        });
    }

    // Pass 2: collect candidates where counter >= 2 into 256 hash buckets.
    const BUCKETS: usize = 256;
    let per_worker_buckets: Vec<Vec<Vec<(i32, i32, u32)>>> =
        if workers <= 1 || nodes.len() <= chunk_size {
            let mut buckets = vec![Vec::new(); BUCKETS];
            for (idx, n) in nodes.iter().enumerate() {
                let h = hash_xy(n.x, n.y);
                let s = (h as usize) & mask;
                let shift = (s & 31) * 2;
                if ((counts[s >> 5].load(Ordering::Relaxed) >> shift) & 3) >= 2 {
                    buckets[(h >> 56) as usize].push((n.x, n.y, idx as u32));
                }
            }
            vec![buckets]
        } else {
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for (ci, chunk) in nodes.chunks(chunk_size).enumerate() {
                    let base = ci * chunk_size;
                    let counts = &counts;
                    handles.push(scope.spawn(move || {
                        let mut buckets = vec![Vec::new(); BUCKETS];
                        for (off, n) in chunk.iter().enumerate() {
                            let h = hash_xy(n.x, n.y);
                            let s = (h as usize) & mask;
                            let shift = (s & 31) * 2;
                            if ((counts[s >> 5].load(Ordering::Relaxed) >> shift) & 3) >= 2 {
                                buckets[(h >> 56) as usize].push((n.x, n.y, (base + off) as u32));
                            }
                        }
                        buckets
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| h.join().expect("shared_xy collector panicked"))
                    .collect()
            })
        };
    drop(counts);

    // Merge and sort each of the 256 buckets in parallel to identify exact duplicate (x, y).
    let mut merged: Vec<Vec<(i32, i32, u32)>> = (0..BUCKETS).map(|_| Vec::new()).collect();
    for worker_buckets in per_worker_buckets {
        for (b, mut vec) in worker_buckets.into_iter().enumerate() {
            if merged[b].is_empty() {
                merged[b] = vec;
            } else {
                merged[b].append(&mut vec);
            }
        }
    }

    let process_bucket = |bucket: &mut Vec<(i32, i32, u32)>| {
        if bucket.len() < 2 {
            return;
        }
        bucket.sort_unstable_by_key(|&(x, y, _)| (x, y));
        let mut i = 0;
        while i + 1 < bucket.len() {
            let (x, y, _) = bucket[i];
            let mut j = i + 1;
            while j < bucket.len() && bucket[j].0 == x && bucket[j].1 == y {
                j += 1;
            }
            if j - i > 1 {
                for &(_, _, idx) in &bucket[i..j] {
                    node_flags[idx as usize]
                        .fetch_or(NODE_SHARED_XY | NODE_IS_FEATURE, Ordering::Relaxed);
                }
            }
            i = j;
        }
    };

    if workers <= 1 {
        for bucket in &mut merged {
            process_bucket(bucket);
        }
    } else {
        let next_b = std::sync::atomic::AtomicUsize::new(0);
        let buckets_per_worker = BUCKETS.div_ceil(workers);
        std::thread::scope(|scope| {
            for slice in merged.chunks_mut(buckets_per_worker) {
                let _ = &next_b;
                scope.spawn(move || {
                    for bucket in slice {
                        process_bucket(bucket);
                    }
                });
            }
        });
    }
}

/// Parallel merge sort for slices when input is not already sorted by key.
fn parallel_sort_by_key<T: Send + Copy, K: Ord + Send>(
    slice: &mut [T],
    key: fn(&T) -> K,
    workers: usize,
) {
    if workers <= 1 || slice.len() < 32_768 {
        slice.sort_unstable_by_key(key);
        return;
    }
    let mid = slice.len() / 2;
    let (left, right) = slice.split_at_mut(mid);
    let left_workers = workers / 2;
    let right_workers = workers - left_workers;
    std::thread::scope(|scope| {
        scope.spawn(move || parallel_sort_by_key(left, key, left_workers));
        parallel_sort_by_key(right, key, right_workers);
    });
    let mut merged = Vec::with_capacity(slice.len());
    let (mut i, mut j) = (0, mid);
    while i < mid && j < slice.len() {
        if key(&slice[i]) <= key(&slice[j]) {
            merged.push(slice[i]);
            i += 1;
        } else {
            merged.push(slice[j]);
            j += 1;
        }
    }
    if i < mid {
        merged.extend_from_slice(&slice[i..mid]);
    } else if j < slice.len() {
        merged.extend_from_slice(&slice[j..]);
    }
    slice.copy_from_slice(&merged);
}

// Closed-way classification follows the CC0 osm-polygon-features tag list.
fn area_way(tags: &[(u32, u32)], strings: &StringPool, meta: &[StrMeta]) -> bool {
    if let Some(&(_, v)) = tags.iter().find(|&&(k, _)| strings.get(k) == "area") {
        match strings.get(v) {
            "no" => return false,
            "yes" => return true,
            _ => {}
        }
    }
    has_area_tag(tags, strings, meta)
}

fn has_area_tag(tags: &[(u32, u32)], strings: &StringPool, meta: &[StrMeta]) -> bool {
    tags.iter().any(|&(k, v)| {
        let vs = strings.get(v);
        if vs == "no" {
            return false;
        }
        let ks = strings.get(k);
        let matches_rule = match ks {
            "building" | "building:part" | "landuse" | "leisure" | "amenity" | "shop"
            | "tourism" | "historic" | "office" | "place" | "area:highway" | "military"
            | "public_transport" | "craft" | "boundary" | "golf" | "indoor" | "ruins" | "area" => {
                true
            }
            "type" => matches!(vs, "multipolygon" | "boundary"),
            "natural" => !matches!(vs, "coastline" | "cliff" | "ridge" | "arete" | "tree_row"),
            "aeroway" => vs != "taxiway",
            "waterway" => matches!(vs, "riverbank" | "dock" | "boatyard" | "dam"),
            "highway" => matches!(vs, "services" | "rest_area" | "escape" | "elevator"),
            "railway" => matches!(vs, "platform" | "station" | "turntable" | "roundhouse"),
            "power" => matches!(vs, "plant" | "substation" | "generator" | "transformer"),
            "barrier" => matches!(
                vs,
                "hedge" | "city_wall" | "ditch" | "retaining_wall" | "wall" | "spikes"
            ),
            "man_made" => !matches!(vs, "cutline" | "embankment" | "pipeline"),
            _ => false,
        };
        if !matches_rule {
            return false;
        }
        // Area rules operate on text values. A frequent decimal may be
        // dictionary text while the same spelling in a small file is numeric.
        let m = meta[v as usize];
        m.number_kind == 0 || m.has_dict_code()
    })
}

#[derive(Clone, Copy)]
struct ResolvedWay {
    id: i64,
    flags: u16,
    bounds: [i32; 4],
    tag_start: usize,
    tag_len: u32,
    task_idx: u32,
    coord_start: usize,
    coord_len: u32,
    fnode_start: usize,
    fnode_len: u32,
}

struct WayTaskOut {
    ways: Vec<ResolvedWay>,
    coords: Vec<u8>,
    fnodes: Vec<u32>,
    omitted: u64,
}

/// Resolve all OSM references, compute geometry & bounding boxes, classify areas,
/// synthesize GeoDesk annotations, and produce the compact `BuiltDataset`.
///
/// When `budget.is_spilling()` or raw chunks were already spilled during PBF
/// decoding, runs the bounded-memory disk-backed resolution pipeline.
pub(super) fn resolve(
    raw: RawStage,
    dictionary: &Dictionary,
    budget: &MemoryBudget,
) -> Result<(BuiltDataset, BuildReport)> {
    if budget.is_spilling() || raw.has_spilled() {
        resolve_spilled(raw, dictionary, budget)
    } else {
        resolve_in_memory(raw, dictionary)
    }
}

fn resolve_in_memory(
    mut raw: RawStage,
    dictionary: &Dictionary,
) -> Result<(BuiltDataset, BuildReport)> {
    let workers = worker_count().min(32);

    // Ensure synthetic annotation strings exist in the global string pool.
    let mut strings = raw.strings;
    let s_yes = strings.intern("yes");
    let s_outer = strings.intern("outer");
    let s_dup = strings.intern("geodesk:duplicate");
    let s_orphan = strings.intern("geodesk:orphan");
    let s_missing = strings.intern("geodesk:missing_members");
    let name_str_id = strings.find("name");
    let mut str_meta = dictionary.build_meta(&strings);

    // Remap per-worker string IDs into global string IDs and flatten tags in parallel.
    let mut tag_bases = Vec::with_capacity(raw.workers.len());
    let mut total_tags = 0usize;
    for w in &raw.workers {
        tag_bases.push(total_tags);
        total_tags += w.tags.len();
    }
    let has_remaps = !raw.remaps.is_empty();
    let mut tags = vec![(0u32, 0u32); total_tags];
    if raw.workers.len() == 1 {
        tags = std::mem::take(&mut raw.workers[0].tags);
    } else {
        std::thread::scope(|scope| {
            let mut rest = tags.as_mut_slice();
            for ((w, remap), &tag_base) in raw
                .workers
                .iter_mut()
                .zip(raw.remaps.iter())
                .zip(&tag_bases)
            {
                let (dst, tail) = rest.split_at_mut(w.tags.len());
                rest = tail;
                scope.spawn(move || {
                    if has_remaps {
                        for (d, &(k, v)) in dst.iter_mut().zip(&w.tags) {
                            *d = (remap[k as usize], remap[v as usize]);
                        }
                        for m in &mut w.rel_members {
                            m.role = remap[m.role as usize];
                        }
                    } else {
                        dst.copy_from_slice(&w.tags);
                    }
                    w.tags = Vec::new();
                    if tag_base != 0 {
                        for n in &mut w.tagged_nodes {
                            n.tag_start += tag_base;
                        }
                        for way in &mut w.ways {
                            if way.tag_len > 0 {
                                way.tag_start += tag_base;
                            }
                        }
                        for rel in &mut w.relations {
                            if rel.tag_len > 0 {
                                rel.tag_start += tag_base;
                            }
                        }
                    }
                });
            }
        });
    }
    raw.remaps = Vec::new();

    // Append constant tag slices for synthetic node annotations.
    let orphan_tag_start = tags.len();
    tags.push((s_orphan, s_yes));
    let dup_tag_start = tags.len();
    tags.push((s_dup, s_yes));
    let both_tag_start = tags.len();
    tags.push((s_dup, s_yes));
    tags.push((s_orphan, s_yes));

    // Flatten raw nodes (fast-path when PBF node chunks are already strictly sorted).
    let mut node_chunks: Vec<(usize, bool, Vec<RawNode>)> = Vec::new();
    for w in &mut raw.workers {
        node_chunks.append(&mut w.node_chunks);
    }
    node_chunks.sort_unstable_by_key(|c| c.0);
    let mut total_nodes = 0usize;
    let mut globally_sorted = true;
    let mut prev_last = i64::MIN;
    let mut chunk_offsets = Vec::with_capacity(node_chunks.len());
    for (_, sorted, chunk) in &node_chunks {
        chunk_offsets.push(total_nodes);
        if !chunk.is_empty() {
            if !*sorted || chunk[0].id <= prev_last {
                globally_sorted = false;
            }
            prev_last = chunk[chunk.len() - 1].id;
            total_nodes += chunk.len();
        }
    }
    ensure!(
        total_nodes <= u32::MAX as usize,
        "extract exceeds 4 billion nodes"
    );

    let mut nodes = if node_chunks.len() == 1 {
        node_chunks.pop().unwrap().2
    } else {
        let mut nodes = vec![RawNode { id: 0, x: 0, y: 0 }; total_nodes];
        if workers <= 1 || total_nodes < 65_536 {
            for (offset, (_, _, chunk)) in chunk_offsets.iter().zip(node_chunks) {
                nodes[*offset..*offset + chunk.len()].copy_from_slice(&chunk);
            }
        } else {
            let chunks_per_worker = node_chunks.len().div_ceil(workers).max(1);
            std::thread::scope(|scope| {
                let mut rest = nodes.as_mut_slice();
                for group in node_chunks.chunks_mut(chunks_per_worker) {
                    let len: usize = group.iter().map(|c| c.2.len()).sum();
                    let (dst, tail) = rest.split_at_mut(len);
                    rest = tail;
                    scope.spawn(move || {
                        let mut pos = 0usize;
                        for (_, _, chunk) in group.iter_mut() {
                            dst[pos..pos + chunk.len()].copy_from_slice(chunk);
                            pos += chunk.len();
                            *chunk = Vec::new();
                        }
                    });
                }
            });
            drop(node_chunks);
        }
        nodes
    };

    if !globally_sorted {
        parallel_sort_by_key(&mut nodes, |n| n.id, workers);
        for pair in nodes.windows(2) {
            ensure!(
                pair[0].id != pair[1].id,
                "duplicate OSM node ID {}",
                pair[0].id
            );
        }
    }

    let node_index = NodeIndex::build(&nodes);
    let node_flags: Vec<AtomicU8> = (0..nodes.len()).map(|_| AtomicU8::new(0)).collect();

    // Mark tagged nodes and resolve their `node_idx`.
    let mut raw_tagged: Vec<RawTaggedNode> = Vec::new();
    for w in &mut raw.workers {
        raw_tagged.append(&mut w.tagged_nodes);
    }
    let mut tagged_by_idx: Vec<(u32, usize, u32)> = raw_tagged
        .into_iter()
        .filter_map(|t| {
            let idx = node_index.find(t.id)?;
            node_flags[idx].fetch_or(NODE_TAGGED | NODE_IS_FEATURE, Ordering::Relaxed);
            Some((idx as u32, t.tag_start, t.tag_len))
        })
        .collect();
    tagged_by_idx.sort_unstable_by_key(|&(idx, _, _)| idx);

    // Mark nodes sharing identical (x, y) coordinates.
    mark_shared_xy(&nodes, &node_flags, workers);

    // Flatten raw relations and mark relation-member nodes as features.
    let mut raw_rels: Vec<RawRelation> = Vec::new();
    let mut raw_members: Vec<RawMember> = Vec::new();
    for w in &mut raw.workers {
        let mem_base = raw_members.len();
        raw_members.append(&mut w.rel_members);
        for mut r in std::mem::take(&mut w.relations) {
            r.mem_start += mem_base;
            raw_rels.push(r);
        }
    }
    raw_rels.sort_unstable_by_key(|r| r.id);
    for pair in raw_rels.windows(2) {
        ensure!(
            pair[0].id != pair[1].id,
            "duplicate OSM relation ID {}",
            pair[0].id
        );
    }
    // Pre-resolve node member indices while `node_index` is alive.
    let mut rel_node_member_idx: Vec<u32> = vec![u32::MAX; raw_members.len()];
    for (mi, m) in raw_members.iter().enumerate() {
        if m.kind != 0 {
            continue;
        }
        if let Some(nidx) = node_index.find(m.child_id) {
            node_flags[nidx].fetch_or(NODE_REL_MEMBER | NODE_IS_FEATURE, Ordering::Relaxed);
            rel_node_member_idx[mi] = nidx as u32;
        }
    }

    // Parallel way resolution: validate vertices, classify areas, encode coordinates,
    // mark used nodes and feature-node links.
    const WAY_TASK_CHUNK: usize = 16_384;
    let mut way_tasks_input: Vec<(&[RawWay], &[i64])> = Vec::new();
    for w in &raw.workers {
        for chunk in w.ways.chunks(WAY_TASK_CHUNK) {
            way_tasks_input.push((chunk, &w.way_refs));
        }
    }

    let process_way_task = |task_idx: u32, ways_slice: &[RawWay], way_refs: &[i64]| -> WayTaskOut {
        let mut out = WayTaskOut {
            ways: Vec::with_capacity(ways_slice.len()),
            coords: Vec::with_capacity(ways_slice.len() * 24),
            fnodes: Vec::new(),
            omitted: 0,
        };
        let mut points: Vec<(i32, i32, u32, bool)> = Vec::new();
        for w in ways_slice {
            let mut refs = &way_refs[w.ref_start..w.ref_start + w.ref_len as usize];
            if refs.len() < 4 && refs.first() == refs.last() {
                out.omitted += 1;
                continue;
            }
            let mut flags = 8u16;
            let wtags = &tags[w.tag_start..w.tag_start + w.tag_len as usize];
            if refs.len() >= 4
                && refs.first() == refs.last()
                && area_way(wtags, &strings, &str_meta)
            {
                flags |= 2;
                refs = &refs[..refs.len() - 1];
            }
            points.clear();
            let (mut x1, mut y1, mut x2, mut y2) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
            let mut complete = true;
            for &nid in refs {
                let Some(nidx) = node_index.find(nid) else {
                    complete = false;
                    break;
                };
                let node = nodes[nidx];
                x1 = x1.min(node.x);
                y1 = y1.min(node.y);
                x2 = x2.max(node.x);
                y2 = y2.max(node.y);
                let is_feature = (node_flags[nidx].load(Ordering::Relaxed) & NODE_IS_FEATURE) != 0;
                points.push((node.x, node.y, nidx as u32, is_feature));
            }
            if !complete || points.len() < 2 {
                out.omitted += 1;
                continue;
            }
            let fnode_start = out.fnodes.len();
            for &(_, _, nidx, is_feature) in &points {
                if is_feature {
                    out.fnodes.push(nidx);
                }
            }
            let fnode_len = (out.fnodes.len() - fnode_start) as u32;
            if fnode_len > 0 {
                flags |= 32;
                for &nidx in &out.fnodes[fnode_start..] {
                    node_flags[nidx as usize].fetch_or(NODE_WAY_LINK, Ordering::Relaxed);
                }
            }
            let coord_start = out.coords.len();
            varint(&mut out.coords, points.len() as u64);
            let (mut px, mut py) = (x1, y1);
            for &(x, y, nidx, _) in &points {
                node_flags[nidx as usize].fetch_or(NODE_USED_BY_WAY, Ordering::Relaxed);
                signed(&mut out.coords, i64::from(x) - i64::from(px));
                signed(&mut out.coords, i64::from(y) - i64::from(py));
                (px, py) = (x, y);
            }
            let mut prev = 0i64;
            for &nid in refs {
                signed(&mut out.coords, nid - prev);
                prev = nid;
            }
            let coord_len = (out.coords.len() - coord_start) as u32;
            out.ways.push(ResolvedWay {
                id: w.id,
                flags,
                bounds: [x1, y1, x2, y2],
                tag_start: w.tag_start,
                tag_len: w.tag_len,
                task_idx,
                coord_start,
                coord_len,
                fnode_start,
                fnode_len,
            });
        }
        out
    };

    let way_task_outs: Vec<WayTaskOut> = if workers <= 1 || way_tasks_input.len() <= 1 {
        way_tasks_input
            .iter()
            .enumerate()
            .map(|(ti, &(ws, wr))| process_way_task(ti as u32, ws, wr))
            .collect()
    } else {
        let next_task = std::sync::atomic::AtomicUsize::new(0);
        let mut results: Vec<Option<WayTaskOut>> =
            (0..way_tasks_input.len()).map(|_| None).collect();
        let results_mutex = std::sync::Mutex::new(&mut results);
        std::thread::scope(|scope| {
            for _ in 0..workers.min(way_tasks_input.len()) {
                let next_task = &next_task;
                let way_tasks_input = &way_tasks_input;
                let results_mutex = &results_mutex;
                scope.spawn(move || {
                    loop {
                        let ti = next_task.fetch_add(1, Ordering::Relaxed);
                        if ti >= way_tasks_input.len() {
                            break;
                        }
                        let (ws, wr) = way_tasks_input[ti];
                        let out = process_way_task(ti as u32, ws, wr);
                        results_mutex.lock().unwrap()[ti] = Some(out);
                    }
                });
            }
        });
        results.into_iter().map(|o| o.unwrap()).collect()
    };

    // Free raw way inputs immediately.
    for w in &mut raw.workers {
        w.ways = Vec::new();
        w.way_refs = Vec::new();
    }
    drop(node_index);

    // Build compact BitRank (1.5 bits/node, 21x smaller than Vec<u32>) and emit node features.
    let node_rank = BitRank::from_node_flags(&node_flags, NODE_IS_FEATURE, NODE_USED_BY_WAY);
    let num_node_features = node_rank.total_features();
    let node_chunk_size = nodes.len().div_ceil(workers).max(16_384);
    let mut node_chunk_counts = Vec::new();
    let mut node_feat_offsets = Vec::new();
    let mut running_nfeat = 0usize;
    let mut chunk_start = 0usize;
    while chunk_start < nodes.len() {
        let chunk_end = (chunk_start + node_chunk_size).min(nodes.len());
        let end_feat = if chunk_end == nodes.len() {
            num_node_features
        } else {
            // Find exact cumulative count at `chunk_end` via node_flags slice.
            let mut c = running_nfeat;
            for f in &node_flags[chunk_start..chunk_end] {
                let v = f.load(Ordering::Relaxed);
                if (v & NODE_IS_FEATURE) != 0 || (v & NODE_USED_BY_WAY) == 0 {
                    c += 1;
                }
            }
            c
        };
        node_feat_offsets.push(running_nfeat);
        node_chunk_counts.push(end_feat - running_nfeat);
        running_nfeat = end_feat;
        chunk_start = chunk_end;
    }

    let mut features = vec![
        FeatureRec {
            obj_id: 0,
            bounds: [0; 4],
            tag_start: 0,
            tag_len: 0,
        };
        num_node_features
    ];
    let mut flags = vec![0u16; num_node_features];

    let emit_node_chunk = |base_nidx: usize,
                           nodes_slice: &[RawNode],
                           nflags_slice: &[AtomicU8],
                           feat_out: &mut [FeatureRec],
                           flag_out: &mut [u16]| {
        let mut tcursor = tagged_by_idx.partition_point(|&(idx, _, _)| (idx as usize) < base_nidx);
        let mut out_pos = 0usize;
        for (off, (n, nf)) in nodes_slice.iter().zip(nflags_slice).enumerate() {
            let v = nf.load(Ordering::Relaxed);
            let is_feature = (v & NODE_IS_FEATURE) != 0;
            let used_by_way = (v & NODE_USED_BY_WAY) != 0;
            if !is_feature && used_by_way {
                continue;
            }
            let nidx = (base_nidx + off) as u32;
            let (tag_start, tag_len) = if (v & NODE_TAGGED) != 0 {
                while tcursor < tagged_by_idx.len() && tagged_by_idx[tcursor].0 < nidx {
                    tcursor += 1;
                }
                let (_, ts, tl) = tagged_by_idx[tcursor];
                tcursor += 1;
                (ts, tl)
            } else if (v & NODE_SHARED_XY) != 0 {
                if (v & NODE_WAY_LINK) == 0 && (v & NODE_REL_MEMBER) == 0 {
                    (both_tag_start, 2)
                } else {
                    (dup_tag_start, 1)
                }
            } else if (v & NODE_REL_MEMBER) != 0 {
                (0, 0)
            } else {
                (orphan_tag_start, 1)
            };
            let mut f = 0u16;
            if (v & NODE_SHARED_XY) != 0 {
                f |= 256;
            }
            if (v & NODE_WAY_LINK) != 0 {
                f |= 32;
            }
            feat_out[out_pos] = FeatureRec {
                obj_id: n.id * 4,
                bounds: [n.x, n.y, n.x, n.y],
                tag_start,
                tag_len,
            };
            flag_out[out_pos] = f;
            out_pos += 1;
        }
    };

    if workers <= 1 || nodes.len() <= node_chunk_size {
        emit_node_chunk(0, &nodes, &node_flags, &mut features, &mut flags);
    } else {
        std::thread::scope(|scope| {
            let mut feat_rest = features.as_mut_slice();
            let mut flag_rest = flags.as_mut_slice();
            for (ci, ((ns, nfs), &cnt)) in nodes
                .chunks(node_chunk_size)
                .zip(node_flags.chunks(node_chunk_size))
                .zip(&node_chunk_counts)
                .enumerate()
            {
                let (fo, ft) = feat_rest.split_at_mut(cnt);
                feat_rest = ft;
                let (flo, flt) = flag_rest.split_at_mut(cnt);
                flag_rest = flt;
                let base_nidx = ci * node_chunk_size;
                let emit = &emit_node_chunk;
                scope.spawn(move || emit(base_nidx, ns, nfs, fo, flo));
            }
        });
    }

    // Free raw nodes & flags immediately!
    drop(nodes);
    drop(node_flags);
    drop(tagged_by_idx);

    // Remap relation node members from `node_idx` to `feat_idx` via BitRank.
    for slot in &mut rel_node_member_idx {
        if *slot != u32::MAX {
            *slot = node_rank.get(*slot as usize);
        }
    }

    // Collect resolved ways, check/sort by ID, flatten coords & remap fnodes.
    let mut omitted_ways = 0u64;
    let mut total_ways = 0usize;
    for task in &way_task_outs {
        omitted_ways += task.omitted;
        total_ways += task.ways.len();
    }
    let mut resolved_ways = Vec::with_capacity(total_ways);
    let mut ways_sorted = true;
    let mut prev_wid = i64::MIN;
    for task in &way_task_outs {
        for &w in &task.ways {
            if w.id <= prev_wid {
                ways_sorted = false;
            }
            prev_wid = w.id;
            resolved_ways.push(w);
        }
    }
    if !ways_sorted {
        parallel_sort_by_key(&mut resolved_ways, |w| w.id, workers);
        for pair in resolved_ways.windows(2) {
            ensure!(
                pair[0].id != pair[1].id,
                "duplicate OSM way ID {}",
                pair[0].id
            );
        }
    }

    let way_base = features.len();
    let mut way_coord_spans = Vec::with_capacity(resolved_ways.len());
    let mut way_fnode_offsets: Vec<u32> = Vec::with_capacity(resolved_ways.len() + 1);
    let mut total_coords = 0usize;
    let mut total_fnodes = 0usize;
    way_fnode_offsets.push(0);
    for w in &resolved_ways {
        way_coord_spans.push((total_coords as u64, w.coord_len));
        total_coords += w.coord_len as usize;
        total_fnodes += w.fnode_len as usize;
        way_fnode_offsets
            .push(u32::try_from(total_fnodes).map_err(|_| {
                anyhow::anyhow!("way feature-node table exceeds 4 billion entries")
            })?);
    }

    features.resize(
        way_base + resolved_ways.len(),
        FeatureRec {
            obj_id: 0,
            bounds: [0; 4],
            tag_start: 0,
            tag_len: 0,
        },
    );
    flags.resize(way_base + resolved_ways.len(), 0);
    let mut way_coords = vec![0u8; total_coords];
    let mut way_fnodes = vec![0u32; total_fnodes];

    let way_chunk_size = resolved_ways.len().div_ceil(workers).max(16_384);
    if workers <= 1 || resolved_ways.len() <= way_chunk_size {
        for (i, w) in resolved_ways.iter().enumerate() {
            features[way_base + i] = FeatureRec {
                obj_id: w.id * 4 + 1,
                bounds: w.bounds,
                tag_start: w.tag_start,
                tag_len: w.tag_len,
            };
            flags[way_base + i] = w.flags;
            let src_task = &way_task_outs[w.task_idx as usize];
            let (cs, clen) = way_coord_spans[i];
            let cs = cs as usize;
            way_coords[cs..cs + clen as usize].copy_from_slice(
                &src_task.coords[w.coord_start..w.coord_start + w.coord_len as usize],
            );
            let fs = way_fnode_offsets[i] as usize;
            for (dst, &nidx) in way_fnodes[fs..way_fnode_offsets[i + 1] as usize]
                .iter_mut()
                .zip(&src_task.fnodes[w.fnode_start..w.fnode_start + w.fnode_len as usize])
            {
                *dst = node_rank.get(nidx as usize);
            }
        }
    } else {
        std::thread::scope(|scope| {
            let mut feat_rest = &mut features[way_base..];
            let mut flag_rest = &mut flags[way_base..];
            let mut coord_rest = way_coords.as_mut_slice();
            let mut fnode_rest = way_fnodes.as_mut_slice();
            for (ci, wslice) in resolved_ways.chunks(way_chunk_size).enumerate() {
                let start_i = ci * way_chunk_size;
                let end_i = start_i + wslice.len();
                let c_base = way_coord_spans[start_i].0 as usize;
                let c_end = if end_i == resolved_ways.len() {
                    total_coords
                } else {
                    way_coord_spans[end_i].0 as usize
                };
                let c_len = c_end - c_base;
                let f_len = (way_fnode_offsets[end_i] - way_fnode_offsets[start_i]) as usize;
                let f_base = way_fnode_offsets[start_i] as usize;
                let (fo, ft) = feat_rest.split_at_mut(wslice.len());
                feat_rest = ft;
                let (flo, flt) = flag_rest.split_at_mut(wslice.len());
                flag_rest = flt;
                let (co, ct) = coord_rest.split_at_mut(c_len);
                coord_rest = ct;
                let (fno, fnt) = fnode_rest.split_at_mut(f_len);
                fnode_rest = fnt;
                let way_task_outs = &way_task_outs;
                let node_rank = &node_rank;
                let way_coord_spans = &way_coord_spans;
                let way_fnode_offsets = &way_fnode_offsets;
                scope.spawn(move || {
                    for (off, w) in wslice.iter().enumerate() {
                        let gi = start_i + off;
                        fo[off] = FeatureRec {
                            obj_id: w.id * 4 + 1,
                            bounds: w.bounds,
                            tag_start: w.tag_start,
                            tag_len: w.tag_len,
                        };
                        flo[off] = w.flags;
                        let src_task = &way_task_outs[w.task_idx as usize];
                        let cs = way_coord_spans[gi].0 as usize - c_base;
                        let ce = cs + way_coord_spans[gi].1 as usize;
                        co[cs..ce].copy_from_slice(
                            &src_task.coords[w.coord_start..w.coord_start + w.coord_len as usize],
                        );
                        let fs = way_fnode_offsets[gi] as usize - f_base;
                        let fe = way_fnode_offsets[gi + 1] as usize - f_base;
                        for (dst, &nidx) in fno[fs..fe].iter_mut().zip(
                            &src_task.fnodes[w.fnode_start..w.fnode_start + w.fnode_len as usize],
                        ) {
                            *dst = node_rank.get(nidx as usize);
                        }
                    }
                });
            }
        });
    }
    drop(way_task_outs);
    drop(resolved_ways);
    drop(node_rank);

    // Resolve relations, transitive bounds, omissions, missing_members, areas, and parent links.
    let rel_base = features.len();
    let way_features = &features[way_base..rel_base];
    let mut rel_bounds: Vec<Option<[i32; 4]>> = vec![None; raw_rels.len()];
    let mut nested_edges: Vec<(usize, usize)> = Vec::new();
    // Pre-resolved member target: for kind 0 -> node feat_idx, kind 1 -> way feat_idx,
    // kind 2 -> raw_rel index (converted to feat_idx after omission).
    let mut member_target: Vec<u32> = vec![u32::MAX; raw_members.len()];

    let merge_bounds = |dst: &mut Option<[i32; 4]>, b: [i32; 4]| -> bool {
        match dst {
            None => {
                *dst = Some(b);
                true
            }
            Some(cur) => {
                let changed = b[0] < cur[0] || b[1] < cur[1] || b[2] > cur[2] || b[3] > cur[3];
                if changed {
                    cur[0] = cur[0].min(b[0]);
                    cur[1] = cur[1].min(b[1]);
                    cur[2] = cur[2].max(b[2]);
                    cur[3] = cur[3].max(b[3]);
                }
                changed
            }
        }
    };

    for (ri, r) in raw_rels.iter().enumerate() {
        let end = r.mem_start + r.mem_len as usize;
        for mi in r.mem_start..end {
            let m = raw_members[mi];
            match m.kind {
                0 => {
                    let fidx = rel_node_member_idx[mi];
                    if fidx != u32::MAX {
                        member_target[mi] = fidx;
                        merge_bounds(&mut rel_bounds[ri], features[fidx as usize].bounds);
                    }
                }
                1 => {
                    let target_obj_id = m.child_id * 4 + 1;
                    if let Ok(off) = way_features.binary_search_by_key(&target_obj_id, |f| f.obj_id)
                    {
                        let fidx = (way_base + off) as u32;
                        member_target[mi] = fidx;
                        merge_bounds(&mut rel_bounds[ri], way_features[off].bounds);
                    }
                }
                _ => {
                    if let Ok(child_ri) = raw_rels.binary_search_by_key(&m.child_id, |rr| rr.id) {
                        member_target[mi] = child_ri as u32;
                        nested_edges.push((ri, child_ri));
                    }
                }
            }
        }
    }
    nested_edges.sort_unstable();
    nested_edges.dedup();

    // Fixed-point propagation of transitive relation bounds (cycles converge).
    loop {
        let mut changed = false;
        for &(parent_ri, child_ri) in &nested_edges {
            if let Some(cb) = rel_bounds[child_ri] {
                changed |= merge_bounds(&mut rel_bounds[parent_ri], cb);
            }
        }
        if !changed {
            break;
        }
    }

    // Map surviving raw relations to final feature indices.
    let mut raw_rel_to_feat: Vec<u32> = vec![u32::MAX; raw_rels.len()];
    let mut omitted_relations = 0u64;
    let mut surviving_rels: Vec<usize> = Vec::with_capacity(raw_rels.len());
    for (ri, bounds) in rel_bounds.iter().enumerate() {
        if bounds.is_some() {
            raw_rel_to_feat[ri] = (rel_base + surviving_rels.len()) as u32;
            surviving_rels.push(ri);
        } else {
            omitted_relations += 1;
        }
    }

    // Count omitted members across ALL raw relations
    // and assemble surviving relations' members, missing_members tags, area flag, and parent links.
    let mut omitted_members = 0u64;
    for (ri, r) in raw_rels.iter().enumerate() {
        if rel_bounds[ri].is_none() {
            let end = r.mem_start + r.mem_len as usize;
            for mi in r.mem_start..end {
                let m = raw_members[mi];
                let alive = match m.kind {
                    0 | 1 => member_target[mi] != u32::MAX,
                    _ => {
                        let t = member_target[mi];
                        t != u32::MAX && raw_rel_to_feat[t as usize] != u32::MAX
                    }
                };
                if !alive {
                    omitted_members += 1;
                }
            }
        }
    }

    let mut rel_members: Vec<(u32, u32)> = Vec::new();
    let mut rel_member_offsets: Vec<usize> = Vec::with_capacity(surviving_rels.len() + 1);
    rel_member_offsets.push(0);
    let mut parent_pairs: Vec<(u32, u32)> = Vec::new();

    for &ri in &surviving_rels {
        let r = raw_rels[ri];
        let parent_fidx = raw_rel_to_feat[ri];
        let end = r.mem_start + r.mem_len as usize;
        let mut missing = 0u64;
        let mut has_outer = false;
        for mi in r.mem_start..end {
            let m = raw_members[mi];
            let child_fidx = match m.kind {
                0 | 1 => member_target[mi],
                _ => {
                    let t = member_target[mi];
                    if t == u32::MAX {
                        u32::MAX
                    } else {
                        raw_rel_to_feat[t as usize]
                    }
                }
            };
            if child_fidx == u32::MAX {
                omitted_members += 1;
                missing += 1;
            } else {
                if m.role == s_outer {
                    has_outer = true;
                }
                rel_members.push((child_fidx, m.role));
                parent_pairs.push((child_fidx, parent_fidx));
            }
        }
        rel_member_offsets.push(rel_members.len());

        let mut tag_start = r.tag_start;
        let mut tag_len = r.tag_len;
        if missing > 0 {
            let val_str = missing.to_string();
            let (val_id, is_new) = strings.intern_ex(&val_str);
            if is_new {
                str_meta.push(dictionary.str_meta(&val_str));
            }
            let slice = &mut tags[tag_start..tag_start + tag_len as usize];
            if let Some(slot) = slice.iter_mut().find(|(k, _)| *k == s_missing) {
                slot.1 = val_id;
            } else {
                let new_start = tags.len();
                tags.reserve(tag_len as usize + 1);
                for i in 0..tag_len as usize {
                    let pair = tags[tag_start + i];
                    tags.push(pair);
                }
                tags.push((s_missing, val_id));
                tag_start = new_start;
                tag_len += 1;
            }
        }

        let mut rflags = 16u16;
        if has_outer {
            let rtags = &tags[tag_start..tag_start + tag_len as usize];
            let area = rtags
                .iter()
                .find(|&&(k, _)| strings.get(k) == "type")
                .map_or_else(
                    || has_area_tag(rtags, &strings, &str_meta),
                    |&(_, v)| matches!(strings.get(v), "multipolygon" | "boundary"),
                );
            if area {
                rflags |= 2;
            }
        }

        features.push(FeatureRec {
            obj_id: r.id * 4 + 2,
            bounds: rel_bounds[ri].unwrap(),
            tag_start,
            tag_len,
        });
        flags.push(rflags);
    }

    // Build deduplicated parent relation backlinks CSR and set flag bit 4.
    parent_pairs.sort_unstable();
    parent_pairs.dedup();
    ensure!(
        parent_pairs.len() <= u32::MAX as usize,
        "parent relation table exceeds 4 billion entries"
    );
    let mut parent_offsets = vec![0u32; features.len() + 1];
    for &(child_fidx, _) in &parent_pairs {
        flags[child_fidx as usize] |= 4;
        parent_offsets[child_fidx as usize + 1] += 1;
    }
    for i in 1..parent_offsets.len() {
        parent_offsets[i] += parent_offsets[i - 1];
    }
    let parent_links: Vec<u32> = parent_pairs.into_iter().map(|(_, p)| p).collect();

    strings.shrink_lookup();

    let report = BuildReport {
        features: features.len() as u64,
        omitted_ways,
        omitted_relations,
        omitted_members,
        tiles: 0,
    };
    let dataset = BuiltDataset {
        strings,
        str_meta,
        name_str_id,
        tags: PodStore::Mem(tags),
        features: PodStore::Mem(features),
        flags,
        way_base,
        rel_base,
        way_coords: BytesStore::Mem(way_coords),
        way_coord_spans: PodStore::Mem(way_coord_spans),
        way_fnodes,
        way_fnode_offsets,
        rel_members,
        rel_member_offsets,
        parent_links,
        parent_offsets,
    };
    Ok((dataset, report))
}

/// Disk-backed sorted `RawNode` table indexed by a sparse 64 KiB page directory
/// (only 8 bytes per 4,096 nodes = 0.002 bytes/node in RAM).
struct DiskNodeStore {
    blob: PagedBlob,
    len: usize,
    page_first_ids: Vec<i64>,
}

impl DiskNodeStore {
    fn build(
        mut chunks: Vec<SpillNodeChunk>,
        raw_file: Option<File>,
        budget: &MemoryBudget,
    ) -> Result<Self> {
        chunks.sort_unstable_by_key(|c| c.seq);
        let total_nodes: usize = chunks.iter().map(|c| c.count).sum();
        ensure!(
            total_nodes <= u32::MAX as usize,
            "extract exceeds 4 billion nodes"
        );
        if total_nodes == 0 {
            let empty = BlobWriter::new(budget.create_file("nodes_sorted.spill")?)
                .into_paged_blob(budget.cache_bytes())?;
            return Ok(Self {
                blob: empty,
                len: 0,
                page_first_ids: Vec::new(),
            });
        }
        let raw_file = raw_file.expect("spilled node chunks must have backing file");

        let mut globally_sorted = true;
        let mut prev_max = i64::MIN;
        for c in &chunks {
            if !c.sorted || c.min_id <= prev_max {
                globally_sorted = false;
                break;
            }
            prev_max = c.max_id;
        }

        let mut writer = BlobWriter::new(budget.create_file("nodes_sorted.spill")?);
        let mut page_first_ids = Vec::with_capacity(total_nodes.div_ceil(NODES_PER_PAGE));

        if globally_sorted {
            let mut raw = Vec::new();
            let mut written = 0usize;
            for c in &chunks {
                let byte_len = c.count * RawNode::SIZE;
                raw.resize(byte_len, 0);
                read_file_exact_at(&raw_file, c.offset, &mut raw)?;
                for i in 0..c.count {
                    if written.is_multiple_of(NODES_PER_PAGE) {
                        let id = i64::from_le_bytes(
                            raw[i * RawNode::SIZE..i * RawNode::SIZE + 8]
                                .try_into()
                                .unwrap(),
                        );
                        page_first_ids.push(id);
                    }
                    written += 1;
                }
                writer.append(&raw)?;
            }
        } else {
            // Bounded-memory external k-way merge sort for out-of-order PBFs.
            let run_cap = (budget.sort_buffer_bytes() / RawNode::SIZE).max(4096);
            let mut sort_buf: Vec<RawNode> = Vec::with_capacity(run_cap.min(total_nodes));
            let mut raw = Vec::new();
            let mut run_names: Vec<String> = Vec::new();
            let mut run_files: Vec<File> = Vec::new();

            let flush_run = |buf: &mut Vec<RawNode>,
                             run_names: &mut Vec<String>,
                             run_files: &mut Vec<File>|
             -> Result<()> {
                if buf.is_empty() {
                    return Ok(());
                }
                buf.sort_unstable_by_key(|n| n.id);
                let name = format!("node_run_{}.spill", run_files.len());
                let mut w = BlobWriter::new(budget.create_file(&name)?);
                let mut enc = vec![0u8; buf.len() * RawNode::SIZE];
                for (i, n) in buf.iter().enumerate() {
                    n.encode(&mut enc[i * RawNode::SIZE..(i + 1) * RawNode::SIZE]);
                }
                w.append(&enc)?;
                let (file, _) = w.into_file()?;
                run_names.push(name);
                run_files.push(file);
                buf.clear();
                Ok(())
            };

            for c in &chunks {
                let byte_len = c.count * RawNode::SIZE;
                raw.resize(byte_len, 0);
                read_file_exact_at(&raw_file, c.offset, &mut raw)?;
                for rec in raw.as_chunks::<{ RawNode::SIZE }>().0 {
                    sort_buf.push(RawNode::decode(rec));
                    if sort_buf.len() >= run_cap {
                        flush_run(&mut sort_buf, &mut run_names, &mut run_files)?;
                    }
                }
            }
            flush_run(&mut sort_buf, &mut run_names, &mut run_files)?;
            drop(sort_buf);
            drop(raw);

            let mut readers: Vec<std::io::BufReader<File>> = run_files
                .into_iter()
                .map(sequential_reader)
                .collect::<Result<_>>()?;
            let mut heads: Vec<Option<RawNode>> = vec![None; readers.len()];
            let mut heap: BinaryHeap<Reverse<(i64, usize)>> =
                BinaryHeap::with_capacity(readers.len());
            let mut rec_buf = [0u8; RawNode::SIZE];

            for (ri, rdr) in readers.iter_mut().enumerate() {
                if rdr.read_exact(&mut rec_buf).is_ok() {
                    let node = RawNode::decode(&rec_buf);
                    heads[ri] = Some(node);
                    heap.push(Reverse((node.id, ri)));
                }
            }

            let mut out_batch = Vec::with_capacity(65_536);
            let mut prev_id = None;
            let mut written = 0usize;
            while let Some(Reverse((_, ri))) = heap.pop() {
                let node = heads[ri].take().unwrap();
                if let Some(prev) = prev_id {
                    ensure!(node.id != prev, "duplicate OSM node ID {}", node.id);
                }
                prev_id = Some(node.id);
                if written.is_multiple_of(NODES_PER_PAGE) {
                    page_first_ids.push(node.id);
                }
                written += 1;
                let mut enc = [0u8; RawNode::SIZE];
                node.encode(&mut enc);
                out_batch.extend_from_slice(&enc);
                if out_batch.len() >= 65_536 {
                    writer.append(&out_batch)?;
                    out_batch.clear();
                }
                if readers[ri].read_exact(&mut rec_buf).is_ok() {
                    let next = RawNode::decode(&rec_buf);
                    heads[ri] = Some(next);
                    heap.push(Reverse((next.id, ri)));
                }
            }
            if !out_batch.is_empty() {
                writer.append(&out_batch)?;
            }
            drop(readers);
            for name in run_names {
                budget.remove_file(&name);
            }
        }

        drop(raw_file);
        budget.remove_file("nodes_raw.spill");

        let blob = writer.into_paged_blob(budget.cache_bytes())?;
        Ok(Self {
            blob,
            len: total_nodes,
            page_first_ids,
        })
    }

    #[inline]
    fn find(&self, id: i64) -> Result<Option<(usize, RawNode)>> {
        if self.len == 0 || id < self.page_first_ids[0] {
            return Ok(None);
        }
        let page_idx = self.page_first_ids.partition_point(|&first| first <= id) - 1;
        let page = self.blob.read_page(page_idx as u64)?;
        let count = page.len() / RawNode::SIZE;
        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let off = mid * RawNode::SIZE;
            let mid_id = i64::from_le_bytes(page[off..off + 8].try_into().unwrap());
            if mid_id < id {
                lo = mid + 1;
            } else if mid_id > id {
                hi = mid;
            } else {
                return Ok(Some((
                    page_idx * NODES_PER_PAGE + mid,
                    RawNode::decode(&page[off..off + RawNode::SIZE]),
                )));
            }
        }
        Ok(None)
    }

    /// Stream all nodes sequentially in 64 KiB pages.
    fn for_each_page(&self, mut f: impl FnMut(usize, &[u8]) -> Result<()>) -> Result<()> {
        let num_pages = self.len.div_ceil(NODES_PER_PAGE);
        for page_idx in 0..num_pages {
            let page = self.blob.read_page(page_idx as u64)?;
            f(page_idx * NODES_PER_PAGE, &page)?;
        }
        Ok(())
    }
}

fn mark_shared_xy_disk(
    nodes: &DiskNodeStore,
    node_flags: &[AtomicU8],
    workers: usize,
    budget: &MemoryBudget,
) -> Result<()> {
    if nodes.len < 2 {
        return Ok(());
    }
    let max_count_bytes = (budget.sort_buffer_bytes() / 2).clamp(1024 * 1024, 16 * 1024 * 1024);
    let max_slots = (max_count_bytes * 4).next_power_of_two();
    let slots = (nodes.len.saturating_mul(4))
        .next_power_of_two()
        .clamp(1024, max_slots);
    let mask = slots - 1;
    let counts: Vec<AtomicU64> = (0..slots / 32).map(|_| AtomicU64::new(0)).collect();

    // Pass 1: streaming 2-bit saturating counting filter.
    nodes.for_each_page(|_, page| {
        for raw in page.as_chunks::<{ RawNode::SIZE }>().0 {
            let x = i32::from_le_bytes(raw[8..12].try_into().unwrap());
            let y = i32::from_le_bytes(raw[12..16].try_into().unwrap());
            let s = (hash_xy(x, y) as usize) & mask;
            let word = &counts[s >> 5];
            let shift = (s & 31) * 2;
            let v = word.load(Ordering::Relaxed);
            let c = (v >> shift) & 3;
            if c < 2 {
                word.store(
                    (v & !(3u64 << shift)) | ((c + 1) << shift),
                    Ordering::Relaxed,
                );
            }
        }
        Ok(())
    })?;

    let process_bucket = |bucket: &mut Vec<(i32, i32, u32)>| {
        if bucket.len() < 2 {
            return;
        }
        bucket.sort_unstable_by_key(|&(x, y, _)| (x, y));
        let mut i = 0;
        while i + 1 < bucket.len() {
            let (x, y, _) = bucket[i];
            let mut j = i + 1;
            while j < bucket.len() && bucket[j].0 == x && bucket[j].1 == y {
                j += 1;
            }
            if j - i > 1 {
                for &(_, _, idx) in &bucket[i..j] {
                    node_flags[idx as usize]
                        .fetch_or(NODE_SHARED_XY | NODE_IS_FEATURE, Ordering::Relaxed);
                }
            }
            i = j;
        }
    };

    if nodes.len <= 500_000 {
        // Small extract: keep all 256 buckets in RAM.
        const BUCKETS: usize = 256;
        let mut buckets: Vec<Vec<(i32, i32, u32)>> = (0..BUCKETS).map(|_| Vec::new()).collect();
        nodes.for_each_page(|base_idx, page| {
            for (off, raw) in page.as_chunks::<{ RawNode::SIZE }>().0.iter().enumerate() {
                let x = i32::from_le_bytes(raw[8..12].try_into().unwrap());
                let y = i32::from_le_bytes(raw[12..16].try_into().unwrap());
                let h = hash_xy(x, y);
                let s = (h as usize) & mask;
                let shift = (s & 31) * 2;
                if ((counts[s >> 5].load(Ordering::Relaxed) >> shift) & 3) >= 2 {
                    buckets[(h >> 56) as usize].push((x, y, (base_idx + off) as u32));
                }
            }
            Ok(())
        })?;
        drop(counts);

        if workers <= 1 {
            for bucket in &mut buckets {
                process_bucket(bucket);
            }
        } else {
            let per_worker = BUCKETS.div_ceil(workers);
            std::thread::scope(|scope| {
                for slice in buckets.chunks_mut(per_worker) {
                    scope.spawn(move || {
                        for bucket in slice {
                            process_bucket(bucket);
                        }
                    });
                }
            });
        }
        return Ok(());
    }

    // Large extract: partition colliding candidates into 16 temporary bucket files
    // in a single pass (~256 KiB RAM), then sort and remove one bucket at a time.
    const DISK_BUCKETS: usize = 16;
    let mut bucket_writers: Vec<std::io::BufWriter<File>> = Vec::with_capacity(DISK_BUCKETS);
    let mut bucket_counts = [0usize; DISK_BUCKETS];
    for b in 0..DISK_BUCKETS {
        let f = budget.create_file(&format!("xy_b{b}.spill"))?;
        bucket_writers.push(std::io::BufWriter::with_capacity(16 * 1024, f));
    }
    let mut enc = [0u8; 12];
    nodes.for_each_page(|base_idx, page| {
        use std::io::Write;
        for (off, raw) in page.as_chunks::<{ RawNode::SIZE }>().0.iter().enumerate() {
            let x = i32::from_le_bytes(raw[8..12].try_into().unwrap());
            let y = i32::from_le_bytes(raw[12..16].try_into().unwrap());
            let h = hash_xy(x, y);
            let s = (h as usize) & mask;
            let shift = (s & 31) * 2;
            if ((counts[s >> 5].load(Ordering::Relaxed) >> shift) & 3) >= 2 {
                let b = (h >> 60) as usize;
                enc[0..4].copy_from_slice(&x.to_le_bytes());
                enc[4..8].copy_from_slice(&y.to_le_bytes());
                enc[8..12].copy_from_slice(&((base_idx + off) as u32).to_le_bytes());
                bucket_writers[b].write_all(&enc)?;
                bucket_counts[b] += 1;
            }
        }
        Ok(())
    })?;
    drop(counts);

    let mut bucket_files: Vec<File> = Vec::with_capacity(DISK_BUCKETS);
    for bw in bucket_writers {
        bucket_files.push(bw.into_inner().map_err(|e| e.into_error())?);
    }

    let mut bucket: Vec<(i32, i32, u32)> = Vec::new();
    let mut buf = Vec::new();
    for (b, file) in bucket_files.into_iter().enumerate() {
        let count = bucket_counts[b];
        if count >= 2 {
            let mut rdr = sequential_reader(file)?;
            buf.resize(count * 12, 0);
            rdr.read_exact(&mut buf)?;
            drop(rdr);
            bucket.clear();
            bucket.reserve(count);
            for rec in buf.as_chunks::<12>().0 {
                bucket.push((
                    i32::from_le_bytes(rec[0..4].try_into().unwrap()),
                    i32::from_le_bytes(rec[4..8].try_into().unwrap()),
                    u32::from_le_bytes(rec[8..12].try_into().unwrap()),
                ));
            }
            process_bucket(&mut bucket);
        }
        budget.remove_file(&format!("xy_b{b}.spill"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct SpilledResolvedWay {
    id: i64,
    flags: u16,
    bounds: [i32; 4],
    tag_start: usize,
    tag_len: u32,
    coord_offset: u64,
    coord_len: u32,
    fnode_start: usize,
    fnode_len: u32,
}

impl PodCodec for SpilledResolvedWay {
    const SIZE: usize = 62;

    #[inline]
    fn encode(&self, dst: &mut [u8]) {
        dst[0..8].copy_from_slice(&self.id.to_le_bytes());
        dst[8..12].copy_from_slice(&self.bounds[0].to_le_bytes());
        dst[12..16].copy_from_slice(&self.bounds[1].to_le_bytes());
        dst[16..20].copy_from_slice(&self.bounds[2].to_le_bytes());
        dst[20..24].copy_from_slice(&self.bounds[3].to_le_bytes());
        dst[24..32].copy_from_slice(&(self.tag_start as u64).to_le_bytes());
        dst[32..40].copy_from_slice(&self.coord_offset.to_le_bytes());
        dst[40..48].copy_from_slice(&(self.fnode_start as u64).to_le_bytes());
        dst[48..52].copy_from_slice(&self.tag_len.to_le_bytes());
        dst[52..56].copy_from_slice(&self.coord_len.to_le_bytes());
        dst[56..60].copy_from_slice(&self.fnode_len.to_le_bytes());
        dst[60..62].copy_from_slice(&self.flags.to_le_bytes());
    }

    #[inline]
    fn decode(src: &[u8]) -> Self {
        Self {
            id: i64::from_le_bytes(src[0..8].try_into().unwrap()),
            bounds: [
                i32::from_le_bytes(src[8..12].try_into().unwrap()),
                i32::from_le_bytes(src[12..16].try_into().unwrap()),
                i32::from_le_bytes(src[16..20].try_into().unwrap()),
                i32::from_le_bytes(src[20..24].try_into().unwrap()),
            ],
            tag_start: u64::from_le_bytes(src[24..32].try_into().unwrap()) as usize,
            coord_offset: u64::from_le_bytes(src[32..40].try_into().unwrap()),
            fnode_start: u64::from_le_bytes(src[40..48].try_into().unwrap()) as usize,
            tag_len: u32::from_le_bytes(src[48..52].try_into().unwrap()),
            coord_len: u32::from_le_bytes(src[52..56].try_into().unwrap()),
            fnode_len: u32::from_le_bytes(src[56..60].try_into().unwrap()),
            flags: u16::from_le_bytes(src[60..62].try_into().unwrap()),
        }
    }
}

/// Bounded-memory disk-backed resolution pipeline used transparently when the
/// input exceeds the detected or configured memory budget.
fn resolve_spilled(
    mut raw: RawStage,
    dictionary: &Dictionary,
    budget: &MemoryBudget,
) -> Result<(BuiltDataset, BuildReport)> {
    let workers = budget.max_workers(worker_count().min(32));

    // Flush any remaining in-memory chunks from all workers into the raw spill files.
    for w in &mut raw.workers {
        w.maybe_spill(budget, &raw.spill_sink, true)?;
    }

    let mut strings = raw.strings;
    let s_yes = strings.intern("yes");
    let s_outer = strings.intern("outer");
    let s_dup = strings.intern("geodesk:duplicate");
    let s_orphan = strings.intern("geodesk:orphan");
    let s_missing = strings.intern("geodesk:missing_members");
    let name_str_id = strings.find("name");
    let mut str_meta = dictionary.build_meta(&strings);

    // 1. Stream remapped tags from `tags_raw.spill` into `tags.spill`.
    let raw_tags_file = raw.spill_sink.take_tags_file()?.map(|(f, _)| f);
    let has_remaps = !raw.remaps.is_empty();
    let mut tags_writer = BlobWriter::new(budget.create_file("tags.spill")?);
    let mut tag_bases = Vec::with_capacity(raw.workers.len());
    let mut total_tags = 0usize;
    {
        let mut raw_buf = Vec::new();
        for (wi, w) in raw.workers.iter_mut().enumerate() {
            tag_bases.push(total_tags);
            let remap = has_remaps.then(|| raw.remaps[wi].as_slice());
            if let Some(ref f) = raw_tags_file {
                for chunk in std::mem::take(&mut w.spilled_tag_chunks) {
                    let byte_len = chunk.count * 8;
                    raw_buf.resize(byte_len, 0);
                    read_file_exact_at(f, chunk.offset, &mut raw_buf)?;
                    if let Some(remap) = remap {
                        for pair_bytes in raw_buf.as_chunks_mut::<8>().0 {
                            let (k, v) = <(u32, u32)>::decode(pair_bytes);
                            (remap[k as usize], remap[v as usize]).encode(pair_bytes);
                        }
                    }
                    tags_writer.append(&raw_buf)?;
                    total_tags += chunk.count;
                }
            }
            if let Some(remap) = remap {
                for m in &mut w.rel_members {
                    m.role = remap[m.role as usize];
                }
            }
            let base = tag_bases[wi];
            if base != 0 {
                for n in &mut w.tagged_nodes {
                    n.tag_start += base;
                }
                for r in &mut w.relations {
                    if r.tag_len > 0 {
                        r.tag_start += base;
                    }
                }
            }
        }
    }
    drop(raw_tags_file);
    budget.remove_file("tags_raw.spill");
    raw.remaps = Vec::new();

    // Append synthetic annotation tag slices.
    let mut append_tag_pair = |k: u32, v: u32| -> Result<usize> {
        let idx = total_tags;
        let mut enc = [0u8; 8];
        (k, v).encode(&mut enc);
        tags_writer.append(&enc)?;
        total_tags += 1;
        Ok(idx)
    };
    let orphan_tag_start = append_tag_pair(s_orphan, s_yes)?;
    let dup_tag_start = append_tag_pair(s_dup, s_yes)?;
    let both_tag_start = append_tag_pair(s_dup, s_yes)?;
    append_tag_pair(s_orphan, s_yes)?;

    // Flush tags.spill and clone the File handle for read-only closed-way/relation checks.
    let (tags_file_for_read, tags_byte_len) = tags_writer.into_file()?;
    let tags_read_clone = tags_file_for_read.try_clone()?;
    let tags_reader = PodStore::<(u32, u32)>::from_disk(
        PagedBlob::new(tags_read_clone, tags_byte_len, budget.cache_bytes()),
        total_tags,
    );
    // Re-wrap the original file positioned at EOF in case relations append missing_members tags.
    let mut tags_append_file = tags_file_for_read;
    std::io::Seek::seek(&mut tags_append_file, std::io::SeekFrom::End(0))?;
    let mut tags_appender = BlobWriter::new(tags_append_file);
    let mut appended_tag_bytes = 0u64;

    // 2. Build sorted disk-backed `DiskNodeStore` and free `nodes_raw.spill`.
    let mut all_node_chunks = Vec::new();
    for w in &mut raw.workers {
        all_node_chunks.append(&mut w.spilled_node_chunks);
    }
    let raw_nodes_file = raw.spill_sink.take_nodes_file()?.map(|(f, _)| f);
    let node_store = DiskNodeStore::build(all_node_chunks, raw_nodes_file, budget)?;
    let node_flags: Vec<AtomicU8> = (0..node_store.len).map(|_| AtomicU8::new(0)).collect();

    // Mark tagged nodes.
    let mut raw_tagged = Vec::new();
    for w in &mut raw.workers {
        raw_tagged.append(&mut w.tagged_nodes);
    }
    let mut tagged_by_idx: Vec<(u32, usize, u32)> = Vec::with_capacity(raw_tagged.len());
    for t in raw_tagged {
        if let Some((idx, _)) = node_store.find(t.id)? {
            node_flags[idx].fetch_or(NODE_TAGGED | NODE_IS_FEATURE, Ordering::Relaxed);
            tagged_by_idx.push((idx as u32, t.tag_start, t.tag_len));
        }
    }
    tagged_by_idx.sort_unstable_by_key(|&(idx, _, _)| idx);

    // Mark nodes sharing identical (x, y) coordinates.
    mark_shared_xy_disk(&node_store, &node_flags, workers, budget)?;

    // Flatten raw relations and pre-resolve node members.
    let mut raw_rels: Vec<RawRelation> = Vec::new();
    let mut raw_members: Vec<RawMember> = Vec::new();
    for w in &mut raw.workers {
        let mem_base = raw_members.len();
        raw_members.append(&mut w.rel_members);
        for mut r in std::mem::take(&mut w.relations) {
            r.mem_start += mem_base;
            raw_rels.push(r);
        }
    }
    raw_rels.sort_unstable_by_key(|r| r.id);
    for pair in raw_rels.windows(2) {
        ensure!(
            pair[0].id != pair[1].id,
            "duplicate OSM relation ID {}",
            pair[0].id
        );
    }
    let mut rel_node_member_idx: Vec<u32> = vec![u32::MAX; raw_members.len()];
    for (mi, m) in raw_members.iter().enumerate() {
        if m.kind != 0 {
            continue;
        }
        if let Some((nidx, _)) = node_store.find(m.child_id)? {
            node_flags[nidx].fetch_or(NODE_REL_MEMBER | NODE_IS_FEATURE, Ordering::Relaxed);
            rel_node_member_idx[mi] = nidx as u32;
        }
    }

    // 3. Stream raw way chunks in bounded waves, resolving in parallel and writing
    //    encoded coordinates straight to `way_coords.spill` and resolved way metadata
    //    straight to `ways_resolved.spill` (never buffering all ways in RAM!).
    let (raw_ways_file, raw_refs_file) = raw.spill_sink.take_ways_files()?;
    let mut way_chunks_with_base: Vec<(usize, SpillWayChunk)> = Vec::new();
    for (wi, w) in raw.workers.iter_mut().enumerate() {
        let base = tag_bases[wi];
        for c in std::mem::take(&mut w.spilled_way_chunks) {
            way_chunks_with_base.push((base, c));
        }
    }
    way_chunks_with_base.sort_unstable_by_key(|&(_, c)| c.min_id);

    let mut way_coords_writer = BlobWriter::new(budget.create_file("way_coords.spill")?);
    let mut resolved_ways_writer = BlobWriter::new(budget.create_file("ways_resolved.spill")?);
    let mut all_way_fnodes: Vec<u32> = Vec::new();
    let mut num_resolved_ways = 0usize;
    let mut omitted_ways = 0u64;
    let mut ways_sorted = true;
    let mut prev_wid = i64::MIN;

    if let (Some(wf), Some(rf)) = (raw_ways_file.as_ref(), raw_refs_file.as_ref()) {
        let wave_size = workers.max(1);
        for wave in way_chunks_with_base.chunks(wave_size) {
            // Load one wave of raw way chunks from disk.
            let mut loaded: Vec<(usize, Vec<RawWay>, Vec<i64>)> = Vec::with_capacity(wave.len());
            let mut wbuf = Vec::new();
            let mut rbuf = Vec::new();
            for &(tbase, chunk) in wave {
                wbuf.resize(chunk.ways_count * RawWay::SIZE, 0);
                read_file_exact_at(wf, chunk.ways_offset, &mut wbuf)?;
                let ways: Vec<RawWay> = wbuf
                    .as_chunks::<{ RawWay::SIZE }>()
                    .0
                    .iter()
                    .map(|row| RawWay::decode(row))
                    .collect();
                rbuf.resize(chunk.refs_count * 8, 0);
                read_file_exact_at(rf, chunk.refs_offset, &mut rbuf)?;
                let refs: Vec<i64> = rbuf
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .copied()
                    .map(i64::from_le_bytes)
                    .collect();
                loaded.push((tbase, ways, refs));
            }
            drop(wbuf);
            drop(rbuf);

            // Resolve the wave in parallel against `DiskNodeStore` and `tags_reader`.
            let resolve_chunk = |tbase: usize,
                                 ways_slice: &[RawWay],
                                 way_refs: &[i64]|
             -> Result<WayTaskOut> {
                let mut out = WayTaskOut {
                    ways: Vec::with_capacity(ways_slice.len()),
                    coords: Vec::with_capacity(ways_slice.len() * 24),
                    fnodes: Vec::new(),
                    omitted: 0,
                };
                let mut points: Vec<(i32, i32, u32, bool)> = Vec::new();
                let mut tag_scratch: Vec<(u32, u32)> = Vec::new();
                for w in ways_slice {
                    let mut refs = &way_refs[w.ref_start..w.ref_start + w.ref_len as usize];
                    if refs.len() < 4 && refs.first() == refs.last() {
                        out.omitted += 1;
                        continue;
                    }
                    let tag_start = if w.tag_len > 0 {
                        w.tag_start + tbase
                    } else {
                        0
                    };
                    let mut flags = 8u16;
                    if refs.len() >= 4 && refs.first() == refs.last() {
                        let wtags = tags_reader.slice_or_read(
                            tag_start,
                            w.tag_len as usize,
                            &mut tag_scratch,
                        )?;
                        if area_way(wtags, &strings, &str_meta) {
                            flags |= 2;
                            refs = &refs[..refs.len() - 1];
                        }
                    }
                    points.clear();
                    let (mut x1, mut y1, mut x2, mut y2) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
                    let mut complete = true;
                    for &nid in refs {
                        let Some((nidx, node)) = node_store.find(nid)? else {
                            complete = false;
                            break;
                        };
                        x1 = x1.min(node.x);
                        y1 = y1.min(node.y);
                        x2 = x2.max(node.x);
                        y2 = y2.max(node.y);
                        let is_feature =
                            (node_flags[nidx].load(Ordering::Relaxed) & NODE_IS_FEATURE) != 0;
                        points.push((node.x, node.y, nidx as u32, is_feature));
                    }
                    if !complete || points.len() < 2 {
                        out.omitted += 1;
                        continue;
                    }
                    let fnode_start = out.fnodes.len();
                    for &(_, _, nidx, is_feature) in &points {
                        if is_feature {
                            out.fnodes.push(nidx);
                        }
                    }
                    let fnode_len = (out.fnodes.len() - fnode_start) as u32;
                    if fnode_len > 0 {
                        flags |= 32;
                        for &nidx in &out.fnodes[fnode_start..] {
                            node_flags[nidx as usize].fetch_or(NODE_WAY_LINK, Ordering::Relaxed);
                        }
                    }
                    let coord_start = out.coords.len();
                    varint(&mut out.coords, points.len() as u64);
                    let (mut px, mut py) = (x1, y1);
                    for &(x, y, nidx, _) in &points {
                        node_flags[nidx as usize].fetch_or(NODE_USED_BY_WAY, Ordering::Relaxed);
                        signed(&mut out.coords, i64::from(x) - i64::from(px));
                        signed(&mut out.coords, i64::from(y) - i64::from(py));
                        (px, py) = (x, y);
                    }
                    let mut prev = 0i64;
                    for &nid in refs {
                        signed(&mut out.coords, nid - prev);
                        prev = nid;
                    }
                    let coord_len = (out.coords.len() - coord_start) as u32;
                    out.ways.push(ResolvedWay {
                        id: w.id,
                        flags,
                        bounds: [x1, y1, x2, y2],
                        tag_start,
                        tag_len: w.tag_len,
                        task_idx: 0,
                        coord_start,
                        coord_len,
                        fnode_start,
                        fnode_len,
                    });
                }
                Ok(out)
            };

            let outs: Vec<Result<WayTaskOut>> = if workers <= 1 || loaded.len() <= 1 {
                loaded
                    .iter()
                    .map(|(tb, ws, wr)| resolve_chunk(*tb, ws, wr))
                    .collect()
            } else {
                std::thread::scope(|scope| {
                    let handles: Vec<_> = loaded
                        .iter()
                        .map(|(tb, ws, wr)| {
                            let resolve_chunk = &resolve_chunk;
                            scope.spawn(move || resolve_chunk(*tb, ws, wr))
                        })
                        .collect();
                    handles
                        .into_iter()
                        .map(|h| h.join().expect("spilled way worker panicked"))
                        .collect()
                })
            };

            let mut way_enc_buf = Vec::new();
            for out in outs {
                let out = out?;
                omitted_ways += out.omitted;
                let coord_base = way_coords_writer.append(&out.coords)?;
                let fnode_base = all_way_fnodes.len();
                all_way_fnodes.extend_from_slice(&out.fnodes);
                way_enc_buf.clear();
                way_enc_buf.resize(out.ways.len() * SpilledResolvedWay::SIZE, 0);
                for (i, w) in out.ways.into_iter().enumerate() {
                    ensure!(w.id != prev_wid, "duplicate OSM way ID {}", w.id);
                    if w.id < prev_wid {
                        ways_sorted = false;
                    }
                    prev_wid = w.id;
                    let rec = SpilledResolvedWay {
                        id: w.id,
                        flags: w.flags,
                        bounds: w.bounds,
                        tag_start: w.tag_start,
                        tag_len: w.tag_len,
                        coord_offset: coord_base + w.coord_start as u64,
                        coord_len: w.coord_len,
                        fnode_start: fnode_base + w.fnode_start,
                        fnode_len: w.fnode_len,
                    };
                    rec.encode(
                        &mut way_enc_buf
                            [i * SpilledResolvedWay::SIZE..(i + 1) * SpilledResolvedWay::SIZE],
                    );
                    num_resolved_ways += 1;
                }
                if !way_enc_buf.is_empty() {
                    resolved_ways_writer.append(&way_enc_buf)?;
                }
            }
        }
    }
    let (resolved_ways_file, _) = resolved_ways_writer.into_file()?;
    drop(raw_ways_file);
    drop(raw_refs_file);
    budget.remove_file("ways_raw.spill");
    budget.remove_file("way_refs_raw.spill");

    // 4. Build compact BitRank, stream node features to `features.spill`, and free node_store.
    let node_rank = BitRank::from_node_flags(&node_flags, NODE_IS_FEATURE, NODE_USED_BY_WAY);
    let num_node_features = node_rank.total_features();
    let mut features_writer = BlobWriter::new(budget.create_file("features.spill")?);
    let mut flags: Vec<u16> = Vec::with_capacity(num_node_features + num_resolved_ways);
    let mut total_features = 0usize;
    {
        let mut tcursor = 0usize;
        let mut enc_buf = Vec::with_capacity(65_536);
        node_store.for_each_page(|base_nidx, page| {
            enc_buf.clear();
            for (off, raw_node) in page.as_chunks::<{ RawNode::SIZE }>().0.iter().enumerate() {
                let nidx = base_nidx + off;
                let v = node_flags[nidx].load(Ordering::Relaxed);
                let is_feature = (v & NODE_IS_FEATURE) != 0;
                let used_by_way = (v & NODE_USED_BY_WAY) != 0;
                if !is_feature && used_by_way {
                    continue;
                }
                let n = RawNode::decode(raw_node);
                let (tag_start, tag_len) = if (v & NODE_TAGGED) != 0 {
                    while tcursor < tagged_by_idx.len()
                        && (tagged_by_idx[tcursor].0 as usize) < nidx
                    {
                        tcursor += 1;
                    }
                    let (_, ts, tl) = tagged_by_idx[tcursor];
                    tcursor += 1;
                    (ts, tl)
                } else if (v & NODE_SHARED_XY) != 0 {
                    if (v & NODE_WAY_LINK) == 0 && (v & NODE_REL_MEMBER) == 0 {
                        (both_tag_start, 2)
                    } else {
                        (dup_tag_start, 1)
                    }
                } else if (v & NODE_REL_MEMBER) != 0 {
                    (0, 0)
                } else {
                    (orphan_tag_start, 1)
                };
                let mut f = 0u16;
                if (v & NODE_SHARED_XY) != 0 {
                    f |= 256;
                }
                if (v & NODE_WAY_LINK) != 0 {
                    f |= 32;
                }
                let rec = FeatureRec {
                    obj_id: n.id * 4,
                    bounds: [n.x, n.y, n.x, n.y],
                    tag_start,
                    tag_len,
                };
                let mut rec_bytes = [0u8; FeatureRec::SIZE];
                rec.encode(&mut rec_bytes);
                enc_buf.extend_from_slice(&rec_bytes);
                flags.push(f);
                total_features += 1;
            }
            if !enc_buf.is_empty() {
                features_writer.append(&enc_buf)?;
            }
            Ok(())
        })?;
    }
    drop(node_store);
    budget.remove_file("nodes_sorted.spill");
    drop(node_flags);
    drop(tagged_by_idx);

    for slot in &mut rel_node_member_idx {
        if *slot != u32::MAX {
            *slot = node_rank.get(*slot as usize);
        }
    }

    // 5. Stream sorted resolved ways (or external k-way merge if out-of-order) into
    //    `features.spill` + `way_coord_spans.spill`, remapping fnodes via BitRank.
    let way_base = total_features;
    let mut way_coord_spans_writer = BlobWriter::new(budget.create_file("way_coord_spans.spill")?);
    let mut way_fnode_offsets: Vec<u32> = Vec::with_capacity(num_resolved_ways + 1);
    let mut way_fnodes = Vec::with_capacity(all_way_fnodes.len());
    way_fnode_offsets.push(0);

    // Sparse index over sorted way obj_ids (1 entry per 1,024 ways) for relation member resolution.
    const WAYS_PER_SPARSE_BLOCK: usize = 1024;
    let mut way_block_first_ids: Vec<i64> =
        Vec::with_capacity(num_resolved_ways.div_ceil(WAYS_PER_SPARSE_BLOCK));
    {
        let mut feat_enc_buf = Vec::with_capacity(65_536);
        let mut span_enc_buf = Vec::with_capacity(65_536);
        let mut emitted_ways = 0usize;
        let mut emit_way = |w: SpilledResolvedWay| -> Result<()> {
            let obj_id = w.id * 4 + 1;
            if emitted_ways.is_multiple_of(WAYS_PER_SPARSE_BLOCK) {
                way_block_first_ids.push(obj_id);
            }
            emitted_ways += 1;

            let mut span_bytes = [0u8; 12];
            (w.coord_offset, w.coord_len).encode(&mut span_bytes);
            span_enc_buf.extend_from_slice(&span_bytes);
            if span_enc_buf.len() >= 65_536 {
                way_coord_spans_writer.append(&span_enc_buf)?;
                span_enc_buf.clear();
            }

            for &nidx in &all_way_fnodes[w.fnode_start..w.fnode_start + w.fnode_len as usize] {
                way_fnodes.push(node_rank.get(nidx as usize));
            }
            way_fnode_offsets.push(u32::try_from(way_fnodes.len()).map_err(|_| {
                anyhow::anyhow!("way feature-node table exceeds 4 billion entries")
            })?);

            let rec = FeatureRec {
                obj_id,
                bounds: w.bounds,
                tag_start: w.tag_start,
                tag_len: w.tag_len,
            };
            let mut rec_bytes = [0u8; FeatureRec::SIZE];
            rec.encode(&mut rec_bytes);
            feat_enc_buf.extend_from_slice(&rec_bytes);
            if feat_enc_buf.len() >= 65_536 {
                features_writer.append(&feat_enc_buf)?;
                feat_enc_buf.clear();
            }
            flags.push(w.flags);
            total_features += 1;
            Ok(())
        };

        if num_resolved_ways == 0 {
            drop(resolved_ways_file);
        } else if ways_sorted {
            let mut rdr = sequential_reader(resolved_ways_file)?;
            let mut rec_buf = [0u8; SpilledResolvedWay::SIZE];
            for _ in 0..num_resolved_ways {
                rdr.read_exact(&mut rec_buf)?;
                emit_way(SpilledResolvedWay::decode(&rec_buf))?;
            }
            drop(rdr);
        } else {
            // Bounded-memory external k-way merge sort for out-of-order ways.
            let run_cap = (budget.sort_buffer_bytes() / SpilledResolvedWay::SIZE).max(4096);
            let mut sort_buf: Vec<SpilledResolvedWay> =
                Vec::with_capacity(run_cap.min(num_resolved_ways));
            let mut run_names: Vec<String> = Vec::new();
            let mut run_files: Vec<File> = Vec::new();

            let flush_run = |buf: &mut Vec<SpilledResolvedWay>,
                             run_names: &mut Vec<String>,
                             run_files: &mut Vec<File>|
             -> Result<()> {
                if buf.is_empty() {
                    return Ok(());
                }
                buf.sort_unstable_by_key(|w| w.id);
                let name = format!("way_run_{}.spill", run_files.len());
                let mut w = BlobWriter::new(budget.create_file(&name)?);
                let mut enc = vec![0u8; buf.len() * SpilledResolvedWay::SIZE];
                for (i, item) in buf.iter().enumerate() {
                    item.encode(
                        &mut enc[i * SpilledResolvedWay::SIZE..(i + 1) * SpilledResolvedWay::SIZE],
                    );
                }
                w.append(&enc)?;
                let (file, _) = w.into_file()?;
                run_names.push(name);
                run_files.push(file);
                buf.clear();
                Ok(())
            };

            let mut rdr = sequential_reader(resolved_ways_file)?;
            let mut rec_buf = [0u8; SpilledResolvedWay::SIZE];
            for _ in 0..num_resolved_ways {
                rdr.read_exact(&mut rec_buf)?;
                sort_buf.push(SpilledResolvedWay::decode(&rec_buf));
                if sort_buf.len() >= run_cap {
                    flush_run(&mut sort_buf, &mut run_names, &mut run_files)?;
                }
            }
            flush_run(&mut sort_buf, &mut run_names, &mut run_files)?;
            drop(rdr);
            drop(sort_buf);

            let mut readers: Vec<std::io::BufReader<File>> = run_files
                .into_iter()
                .map(sequential_reader)
                .collect::<Result<_>>()?;
            let mut heads: Vec<Option<SpilledResolvedWay>> = vec![None; readers.len()];
            let mut heap: BinaryHeap<Reverse<(i64, usize)>> =
                BinaryHeap::with_capacity(readers.len());
            for (ri, r) in readers.iter_mut().enumerate() {
                if r.read_exact(&mut rec_buf).is_ok() {
                    let w = SpilledResolvedWay::decode(&rec_buf);
                    heads[ri] = Some(w);
                    heap.push(Reverse((w.id, ri)));
                }
            }
            let mut prev_id = None;
            while let Some(Reverse((_, ri))) = heap.pop() {
                let w = heads[ri].take().unwrap();
                if let Some(prev) = prev_id {
                    ensure!(w.id != prev, "duplicate OSM way ID {}", w.id);
                }
                prev_id = Some(w.id);
                emit_way(w)?;
                if readers[ri].read_exact(&mut rec_buf).is_ok() {
                    let next = SpilledResolvedWay::decode(&rec_buf);
                    heads[ri] = Some(next);
                    heap.push(Reverse((next.id, ri)));
                }
            }
            drop(readers);
            for name in run_names {
                budget.remove_file(&name);
            }
        }

        if !feat_enc_buf.is_empty() {
            features_writer.append(&feat_enc_buf)?;
        }
        if !span_enc_buf.is_empty() {
            way_coord_spans_writer.append(&span_enc_buf)?;
        }
    }
    let rel_base = total_features;
    budget.remove_file("ways_resolved.spill");
    drop(all_way_fnodes);
    drop(node_rank);

    // Flush features.spill and create a read-only clone for relation member bounds lookup.
    let (feat_file_for_read, feat_byte_len) = features_writer.into_file()?;
    let feat_read_clone = feat_file_for_read.try_clone()?;
    let feat_reader = PodStore::<FeatureRec>::from_disk(
        PagedBlob::new(feat_read_clone, feat_byte_len, budget.cache_bytes()),
        rel_base,
    );
    let mut feat_append_file = feat_file_for_read;
    std::io::Seek::seek(&mut feat_append_file, std::io::SeekFrom::End(0))?;
    let mut feat_appender = BlobWriter::new(feat_append_file);
    let mut appended_feat_bytes = 0u64;

    // 6. Resolve relations against disk-backed node & way FeatureRecs.
    let num_ways = rel_base - way_base;
    let find_way = |target_obj_id: i64| -> Result<Option<(usize, [i32; 4])>> {
        if num_ways == 0 || target_obj_id < way_block_first_ids[0] {
            return Ok(None);
        }
        let block = way_block_first_ids.partition_point(|&first| first <= target_obj_id) - 1;
        let lo_init = block * WAYS_PER_SPARSE_BLOCK;
        let hi_init = (lo_init + WAYS_PER_SPARSE_BLOCK).min(num_ways);
        let mut lo = lo_init;
        let mut hi = hi_init;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let rec = feat_reader.get(way_base + mid)?;
            if rec.obj_id < target_obj_id {
                lo = mid + 1;
            } else if rec.obj_id > target_obj_id {
                hi = mid;
            } else {
                return Ok(Some((way_base + mid, rec.bounds)));
            }
        }
        Ok(None)
    };

    let mut rel_bounds: Vec<Option<[i32; 4]>> = vec![None; raw_rels.len()];
    let mut nested_edges: Vec<(usize, usize)> = Vec::new();
    let mut member_target: Vec<u32> = vec![u32::MAX; raw_members.len()];

    let merge_bounds = |dst: &mut Option<[i32; 4]>, b: [i32; 4]| -> bool {
        match dst {
            None => {
                *dst = Some(b);
                true
            }
            Some(cur) => {
                let changed = b[0] < cur[0] || b[1] < cur[1] || b[2] > cur[2] || b[3] > cur[3];
                if changed {
                    cur[0] = cur[0].min(b[0]);
                    cur[1] = cur[1].min(b[1]);
                    cur[2] = cur[2].max(b[2]);
                    cur[3] = cur[3].max(b[3]);
                }
                changed
            }
        }
    };

    for (ri, r) in raw_rels.iter().enumerate() {
        let end = r.mem_start + r.mem_len as usize;
        for mi in r.mem_start..end {
            let m = raw_members[mi];
            match m.kind {
                0 => {
                    let fidx = rel_node_member_idx[mi];
                    if fidx != u32::MAX {
                        member_target[mi] = fidx;
                        merge_bounds(&mut rel_bounds[ri], feat_reader.get(fidx as usize)?.bounds);
                    }
                }
                1 => {
                    if let Some((fidx, bounds)) = find_way(m.child_id * 4 + 1)? {
                        member_target[mi] = fidx as u32;
                        merge_bounds(&mut rel_bounds[ri], bounds);
                    }
                }
                _ => {
                    if let Ok(child_ri) = raw_rels.binary_search_by_key(&m.child_id, |rr| rr.id) {
                        member_target[mi] = child_ri as u32;
                        nested_edges.push((ri, child_ri));
                    }
                }
            }
        }
    }
    nested_edges.sort_unstable();
    nested_edges.dedup();
    loop {
        let mut changed = false;
        for &(parent_ri, child_ri) in &nested_edges {
            if let Some(cb) = rel_bounds[child_ri] {
                changed |= merge_bounds(&mut rel_bounds[parent_ri], cb);
            }
        }
        if !changed {
            break;
        }
    }

    let mut raw_rel_to_feat: Vec<u32> = vec![u32::MAX; raw_rels.len()];
    let mut omitted_relations = 0u64;
    let mut surviving_rels: Vec<usize> = Vec::with_capacity(raw_rels.len());
    for (ri, bounds) in rel_bounds.iter().enumerate() {
        if bounds.is_some() {
            raw_rel_to_feat[ri] = (rel_base + surviving_rels.len()) as u32;
            surviving_rels.push(ri);
        } else {
            omitted_relations += 1;
        }
    }

    let mut omitted_members = 0u64;
    for (ri, r) in raw_rels.iter().enumerate() {
        if rel_bounds[ri].is_none() {
            let end = r.mem_start + r.mem_len as usize;
            for mi in r.mem_start..end {
                let m = raw_members[mi];
                let alive = match m.kind {
                    0 | 1 => member_target[mi] != u32::MAX,
                    _ => {
                        let t = member_target[mi];
                        t != u32::MAX && raw_rel_to_feat[t as usize] != u32::MAX
                    }
                };
                if !alive {
                    omitted_members += 1;
                }
            }
        }
    }

    let mut rel_members: Vec<(u32, u32)> = Vec::new();
    let mut rel_member_offsets: Vec<usize> = Vec::with_capacity(surviving_rels.len() + 1);
    rel_member_offsets.push(0);
    let mut parent_pairs: Vec<(u32, u32)> = Vec::new();
    let mut tag_scratch: Vec<(u32, u32)> = Vec::new();

    for &ri in &surviving_rels {
        let r = raw_rels[ri];
        let parent_fidx = raw_rel_to_feat[ri];
        let end = r.mem_start + r.mem_len as usize;
        let mut missing = 0u64;
        let mut has_outer = false;
        for mi in r.mem_start..end {
            let m = raw_members[mi];
            let child_fidx = match m.kind {
                0 | 1 => member_target[mi],
                _ => {
                    let t = member_target[mi];
                    if t == u32::MAX {
                        u32::MAX
                    } else {
                        raw_rel_to_feat[t as usize]
                    }
                }
            };
            if child_fidx == u32::MAX {
                omitted_members += 1;
                missing += 1;
            } else {
                if m.role == s_outer {
                    has_outer = true;
                }
                rel_members.push((child_fidx, m.role));
                parent_pairs.push((child_fidx, parent_fidx));
            }
        }
        rel_member_offsets.push(rel_members.len());

        let mut tag_start = r.tag_start;
        let mut tag_len = r.tag_len;
        let mut rtags: Vec<(u32, u32)> = tags_reader
            .slice_or_read(tag_start, tag_len as usize, &mut tag_scratch)?
            .to_vec();
        if missing > 0 {
            let val_str = missing.to_string();
            let (val_id, is_new) = strings.intern_ex(&val_str);
            if is_new {
                str_meta.push(dictionary.str_meta(&val_str));
            }
            if let Some(slot) = rtags.iter_mut().find(|(k, _)| *k == s_missing) {
                slot.1 = val_id;
            } else {
                rtags.push((s_missing, val_id));
            }
            // Append updated relation tags to tags.spill (immutable original slice untouched).
            let new_start = total_tags;
            let mut enc = vec![0u8; rtags.len() * 8];
            for (i, pair) in rtags.iter().enumerate() {
                pair.encode(&mut enc[i * 8..(i + 1) * 8]);
            }
            tags_appender.append(&enc)?;
            appended_tag_bytes += enc.len() as u64;
            total_tags += rtags.len();
            tag_start = new_start;
            tag_len = rtags.len() as u32;
        }

        let mut rflags = 16u16;
        if has_outer {
            let area = rtags
                .iter()
                .find(|&&(k, _)| strings.get(k) == "type")
                .map_or_else(
                    || has_area_tag(&rtags, &strings, &str_meta),
                    |&(_, v)| matches!(strings.get(v), "multipolygon" | "boundary"),
                );
            if area {
                rflags |= 2;
            }
        }

        let rec = FeatureRec {
            obj_id: r.id * 4 + 2,
            bounds: rel_bounds[ri].unwrap(),
            tag_start,
            tag_len,
        };
        let mut rec_bytes = [0u8; FeatureRec::SIZE];
        rec.encode(&mut rec_bytes);
        feat_appender.append(&rec_bytes)?;
        appended_feat_bytes += FeatureRec::SIZE as u64;
        flags.push(rflags);
        total_features += 1;
    }

    // Build deduplicated parent relation backlinks CSR and set flag bit 4.
    parent_pairs.sort_unstable();
    parent_pairs.dedup();
    ensure!(
        parent_pairs.len() <= u32::MAX as usize,
        "parent relation table exceeds 4 billion entries"
    );
    let mut parent_offsets = vec![0u32; total_features + 1];
    for &(child_fidx, _) in &parent_pairs {
        flags[child_fidx as usize] |= 4;
        parent_offsets[child_fidx as usize + 1] += 1;
    }
    for i in 1..parent_offsets.len() {
        parent_offsets[i] += parent_offsets[i - 1];
    }
    let parent_links: Vec<u32> = parent_pairs.into_iter().map(|(_, p)| p).collect();

    strings.shrink_lookup();

    // Finalize all disk-backed stores (`tags.spill`, `features.spill`, `way_coords.spill`, `way_coord_spans.spill`).
    drop(tags_reader);
    drop(feat_reader);
    let (final_tags_file, _) = tags_appender.into_file()?;
    let final_tags = PodStore::from_disk(
        PagedBlob::new(
            final_tags_file,
            tags_byte_len + appended_tag_bytes,
            budget.cache_bytes(),
        ),
        total_tags,
    );
    let (final_feat_file, _) = feat_appender.into_file()?;
    let final_features = PodStore::from_disk(
        PagedBlob::new(
            final_feat_file,
            feat_byte_len + appended_feat_bytes,
            budget.cache_bytes(),
        ),
        total_features,
    );
    let final_way_coords =
        BytesStore::Disk(way_coords_writer.into_paged_blob(budget.cache_bytes())?);
    let final_way_coord_spans = PodStore::from_disk(
        way_coord_spans_writer.into_paged_blob(budget.cache_bytes())?,
        num_resolved_ways,
    );

    let report = BuildReport {
        features: total_features as u64,
        omitted_ways,
        omitted_relations,
        omitted_members,
        tiles: 0,
    };
    let dataset = BuiltDataset {
        strings,
        str_meta,
        name_str_id,
        tags: final_tags,
        features: final_features,
        flags,
        way_base,
        rel_base,
        way_coords: final_way_coords,
        way_coord_spans: final_way_coord_spans,
        way_fnodes,
        way_fnode_offsets,
        rel_members,
        rel_member_offsets,
        parent_links,
        parent_offsets,
    };
    Ok((dataset, report))
}
