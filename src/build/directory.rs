//! Quadtree tile directory construction, feature placement, border duplication,
//! and cross-tile export indexing.
use super::{
    TileZooms,
    spill::MemoryBudget,
    stage::{BuiltDataset, FeatureRec},
    strings::{FxHashMap, FxHashSet},
    worker_count,
};
use anyhow::{Result, ensure};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const MAX_ZOOM: u32 = 12;
pub(super) const TARGET_TILE_BYTES: u64 = 16 * 1024 * 1024;

#[inline]
fn tile_bounds(key: u32) -> [i32; 4] {
    let zoom = key.ilog2() / 2;
    let mut col = 0u32;
    let mut row = 0u32;
    for level in 0..zoom {
        let shift = (zoom - 1 - level) * 2;
        let quad = (key >> shift) & 3;
        col = (col << 1) | (quad & 1);
        row = (row << 1) | ((quad >> 1) & 1);
    }
    let size = 1i64 << (32 - zoom);
    let left = i64::from(i32::MIN) + i64::from(col) * size;
    let top = i64::from(i32::MAX) - i64::from(row) * size;
    [
        left as i32,
        (top - size + 1) as i32,
        (left + size - 1) as i32,
        top as i32,
    ]
}

#[inline]
fn intersects(a: [i32; 4], b: [i32; 4]) -> bool {
    a[0] <= b[2] && a[2] >= b[0] && a[1] <= b[3] && a[3] >= b[1]
}

fn collect_covering_leaves(
    key: u32,
    bbox: [i32; 4],
    zooms: TileZooms,
    tips: &FxHashMap<u32, u32>,
    has_children: &FxHashSet<u32>,
    limit: usize,
    out: &mut Vec<(u32, [i32; 4])>,
) -> bool {
    if !has_children.contains(&key) {
        if out.len() >= limit {
            return false;
        }
        out.push((tips[&key], tile_bounds(key)));
        return true;
    }
    for child_key in zooms.children(key) {
        let child_bounds = tile_bounds(child_key);
        if !intersects(bbox, child_bounds) {
            continue;
        }
        if !tips.contains_key(&child_key) {
            // An uncovered child quadrant exists; keep the feature in its ancestor tile.
            return false;
        }
        if !collect_covering_leaves(child_key, bbox, zooms, tips, has_children, limit, out) {
            return false;
        }
    }
    true
}

// A leading 1 followed by the quadrant at each zoom. A feature is stored
// once in a tile enclosing its entire bounding box unless border-duplicated.
#[inline]
fn cell(bbox: [i32; 4]) -> u32 {
    let col = |x: i32| x as u32 ^ (1 << 31);
    let row = |y: i32| !col(y);
    let (west, south, east, north) = (col(bbox[0]), row(bbox[1]), col(bbox[2]), row(bbox[3]));
    let depth = ((west ^ east) | (north ^ south))
        .leading_zeros()
        .min(MAX_ZOOM);
    let mut key = 1u32;
    for bit in (32 - depth..32).rev() {
        key = (key << 2) | (((north >> bit) & 1) << 1) | ((west >> bit) & 1);
    }
    key
}

pub(super) struct Directory {
    pub words: Vec<u32>,
    pub tips: Vec<u32>,
    pub zooms: u32,
}

pub(super) struct Placement {
    /// Canonical home tile TIP for each feature (`0..num_features`).
    pub home_tip: Vec<u32>,
    /// Export index (`tex`) in the feature's home tile, or `u32::MAX` if not exported.
    pub tex: Vec<u32>,
    /// Flat per-tile placements `(feat_idx, dup_flags)`, sorted by `obj_id` within each tile.
    tile_placements: Vec<(u32, u16)>,
    tile_offsets: Vec<usize>,
}

impl Placement {
    #[inline]
    pub(super) fn tile_slice(&self, tile_idx: usize) -> &[(u32, u16)] {
        &self.tile_placements[self.tile_offsets[tile_idx]..self.tile_offsets[tile_idx + 1]]
    }
}

impl TileZooms {
    // Directory slots go left to right in each row. The in-memory key stores
    // Morton quadrants; the sixteen grandchildren occupy four 2x2 quadrants.
    fn children(self, parent: u32) -> impl Iterator<Item = u32> {
        let slots: &[u32] = match self {
            Self::Consecutive => &[0, 1, 2, 3],
            Self::Even => &[0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15],
        };
        let first = parent * slots.len() as u32;
        slots.iter().map(move |offset| first + offset)
    }
}

impl Directory {
    #[cfg(test)]
    pub(super) fn with_target(
        dataset: &BuiltDataset,
        target: u64,
        zooms: TileZooms,
    ) -> Result<(Self, Placement)> {
        let budget = MemoryBudget::unlimited();
        Self::build(dataset, target, 1, zooms, &budget)
    }

    pub(super) fn build(
        dataset: &BuiltDataset,
        target: u64,
        max_duplicate_tiles: usize,
        zooms: TileZooms,
        budget: &MemoryBudget,
    ) -> Result<(Self, Placement)> {
        let workers = budget.max_workers(worker_count().min(32));
        let n = dataset.features.len();

        // Step 1: accumulate per-cell feature weights in parallel.
        let chunk_size = n.div_ceil(workers).max(16_384);
        let leaf_weights: FxHashMap<u32, u64> = if let Some(mem_features) =
            dataset.features.as_mem()
        {
            if workers <= 1 || n <= chunk_size {
                let mut local = FxHashMap::default();
                for (fi, f) in mem_features.iter().enumerate() {
                    let key = cell(f.bounds);
                    let bytes = 64 + u64::from(f.tag_len) * 16 + dataset.body_weight_bytes(fi)?;
                    *local.entry(key).or_insert(0) += bytes;
                }
                local
            } else {
                let partials: Vec<FxHashMap<u32, u64>> = std::thread::scope(|scope| {
                    let mut handles = Vec::new();
                    for (ci, chunk) in mem_features.chunks(chunk_size).enumerate() {
                        let base = ci * chunk_size;
                        handles.push(scope.spawn(move || {
                            let mut local = FxHashMap::default();
                            for (off, f) in chunk.iter().enumerate() {
                                let fi = base + off;
                                let key = cell(f.bounds);
                                let bytes = 64
                                    + u64::from(f.tag_len) * 16
                                    + dataset.body_weight_bytes(fi).expect("in-memory spans");
                                *local.entry(key).or_insert(0) += bytes;
                            }
                            local
                        }));
                    }
                    handles.into_iter().map(|h| h.join().unwrap()).collect()
                });
                let mut merged = FxHashMap::default();
                for map in partials {
                    for (k, v) in map {
                        *merged.entry(k).or_insert(0) += v;
                    }
                }
                merged
            }
        } else {
            let mut local = FxHashMap::default();
            let mut fbuf: Vec<FeatureRec> = Vec::with_capacity(65_536);
            let mut raw = Vec::new();
            let mut start = 0usize;
            while start < n {
                let count = (n - start).min(65_536);
                dataset
                    .features
                    .read_chunk_into(start, count, &mut fbuf, &mut raw)?;
                for (off, f) in fbuf.iter().enumerate() {
                    let fi = start + off;
                    let key = cell(f.bounds);
                    let bytes = 64 + u64::from(f.tag_len) * 16 + dataset.body_weight_bytes(fi)?;
                    *local.entry(key).or_insert(0) += bytes;
                }
                start += count;
            }
            local
        };

        let mut weights = FxHashMap::<u32, u64>::default();
        for (mut key, bytes) in leaf_weights {
            while key != 0 {
                *weights.entry(key).or_insert(0) += bytes;
                key >>= 2;
            }
        }

        // Step 2: construct the GOL quadtree directory.
        let mut dir = Self {
            words: vec![0, 0, 0],
            tips: Vec::new(),
            zooms: match zooms {
                TileZooms::Consecutive => 0b11,
                TileZooms::Even => 0b101,
            },
        };
        let mut tips = FxHashMap::default();
        let mut has_children = FxHashSet::default();
        dir.branch(1, 1, &weights, target, &mut tips, &mut has_children, zooms);
        dir.words[0] = u32::try_from((dir.words.len() - 1) * 4)?;
        ensure!(
            dir.words[0] < 1 << 30,
            "tile index exceeds GOL segment limit"
        );

        let num_tiles = dir.tips.len();
        let mut tip_to_tile = vec![0u32; dir.words.len()];
        for (tile_idx, &tip) in dir.tips.iter().enumerate() {
            tip_to_tile[tip as usize] = tile_idx as u32;
        }

        // Step 3: assign home_tip and per-tile placements.
        let mut home_tip = vec![0u32; n];
        let mut tile_offsets = vec![0usize; num_tiles + 1];
        let mut tile_placements: Vec<(u32, u16)>;

        if max_duplicate_tiles <= 1 {
            let counts: Vec<usize> = if let Some(mem_features) = dataset.features.as_mem() {
                if workers <= 1 || n <= chunk_size {
                    let mut counts = vec![0usize; num_tiles];
                    for (fi, f) in mem_features.iter().enumerate() {
                        let mut key = cell(f.bounds);
                        while !tips.contains_key(&key) {
                            key >>= 2;
                        }
                        let tip = tips[&key];
                        home_tip[fi] = tip;
                        counts[tip_to_tile[tip as usize] as usize] += 1;
                    }
                    counts
                } else {
                    let partials: Vec<Vec<usize>> = std::thread::scope(|scope| {
                        let mut handles = Vec::new();
                        let tips = &tips;
                        let tip_to_tile = &tip_to_tile;
                        for (fchunk, hchunk) in mem_features
                            .chunks(chunk_size)
                            .zip(home_tip.chunks_mut(chunk_size))
                        {
                            handles.push(scope.spawn(move || {
                                let mut counts = vec![0usize; num_tiles];
                                for (f, h) in fchunk.iter().zip(hchunk) {
                                    let mut key = cell(f.bounds);
                                    while !tips.contains_key(&key) {
                                        key >>= 2;
                                    }
                                    let tip = tips[&key];
                                    *h = tip;
                                    counts[tip_to_tile[tip as usize] as usize] += 1;
                                }
                                counts
                            }));
                        }
                        handles.into_iter().map(|h| h.join().unwrap()).collect()
                    });
                    let mut counts = vec![0usize; num_tiles];
                    for c in partials {
                        for (dst, src) in counts.iter_mut().zip(c) {
                            *dst += src;
                        }
                    }
                    counts
                }
            } else {
                let mut counts = vec![0usize; num_tiles];
                let mut fbuf: Vec<FeatureRec> = Vec::with_capacity(65_536);
                let mut raw = Vec::new();
                let mut start = 0usize;
                while start < n {
                    let count = (n - start).min(65_536);
                    dataset
                        .features
                        .read_chunk_into(start, count, &mut fbuf, &mut raw)?;
                    for (off, f) in fbuf.iter().enumerate() {
                        let mut key = cell(f.bounds);
                        while !tips.contains_key(&key) {
                            key >>= 2;
                        }
                        let tip = tips[&key];
                        home_tip[start + off] = tip;
                        counts[tip_to_tile[tip as usize] as usize] += 1;
                    }
                    start += count;
                }
                counts
            };

            for t in 0..num_tiles {
                tile_offsets[t + 1] = tile_offsets[t] + counts[t];
            }
            tile_placements = vec![(0u32, 0u16); n];
            let mut cursor = tile_offsets[..num_tiles].to_vec();
            for (fi, &tip) in home_tip.iter().enumerate() {
                let t = tip_to_tile[tip as usize] as usize;
                let pos = cursor[t];
                tile_placements[pos] = (fi as u32, 0);
                cursor[t] = pos + 1;
            }
        } else {
            type DupChunkOut = (Vec<usize>, Vec<(u32, u32, u16)>);
            let assign_chunk =
                |base: usize, fchunk: &[FeatureRec], hchunk: &mut [u32]| -> DupChunkOut {
                    let mut counts = vec![0usize; num_tiles];
                    let mut local = Vec::with_capacity(fchunk.len());
                    let mut leaves = Vec::with_capacity(max_duplicate_tiles);
                    for (off, (f, h)) in fchunk.iter().zip(hchunk).enumerate() {
                        let fi = (base + off) as u32;
                        let bbox = f.bounds;
                        let mut key = cell(bbox);
                        while !tips.contains_key(&key) {
                            key >>= 2;
                        }
                        leaves.clear();
                        let duplicated = has_children.contains(&key)
                            && collect_covering_leaves(
                                key,
                                bbox,
                                zooms,
                                &tips,
                                &has_children,
                                max_duplicate_tiles,
                                &mut leaves,
                            )
                            && leaves.len() > 1;
                        if duplicated {
                            let mut canonical = None;
                            for &(leaf_tip, tb) in &leaves {
                                let dup = (if bbox[0] < tb[0] { 64u16 } else { 0 })
                                    | (if bbox[3] > tb[3] { 128u16 } else { 0 });
                                let t = tip_to_tile[leaf_tip as usize];
                                counts[t as usize] += 1;
                                local.push((t, fi, dup));
                                if dup == 0 {
                                    canonical = Some(leaf_tip);
                                }
                            }
                            *h = canonical.unwrap_or(leaves[0].0);
                        } else {
                            let home = tips[&key];
                            *h = home;
                            let t = tip_to_tile[home as usize];
                            counts[t as usize] += 1;
                            local.push((t, fi, 0));
                        }
                    }
                    (counts, local)
                };

            if let Some(mem_features) = dataset.features.as_mem() {
                let parts: Vec<DupChunkOut> = if workers <= 1 || n <= chunk_size {
                    vec![assign_chunk(0, mem_features, &mut home_tip)]
                } else {
                    std::thread::scope(|scope| {
                        let mut handles = Vec::new();
                        let assign_chunk = &assign_chunk;
                        for (ci, (fchunk, hchunk)) in mem_features
                            .chunks(chunk_size)
                            .zip(home_tip.chunks_mut(chunk_size))
                            .enumerate()
                        {
                            let base = ci * chunk_size;
                            handles.push(scope.spawn(move || assign_chunk(base, fchunk, hchunk)));
                        }
                        handles.into_iter().map(|h| h.join().unwrap()).collect()
                    })
                };

                for (counts, _) in &parts {
                    for t in 0..num_tiles {
                        tile_offsets[t + 1] += counts[t];
                    }
                }
                for t in 0..num_tiles {
                    tile_offsets[t + 1] += tile_offsets[t];
                }
                let total_placements = tile_offsets[num_tiles];
                tile_placements = vec![(0u32, 0u16); total_placements];
                let mut cursor = tile_offsets[..num_tiles].to_vec();
                for (_, local) in parts {
                    for (t, fi, dup) in local {
                        let pos = cursor[t as usize];
                        tile_placements[pos] = (fi, dup);
                        cursor[t as usize] = pos + 1;
                    }
                }
            } else {
                // Two-pass streaming assignment to avoid an extra 12 B/feature intermediate buffer.
                let mut fbuf: Vec<FeatureRec> = Vec::with_capacity(65_536);
                let mut raw = Vec::new();
                let mut leaves = Vec::with_capacity(max_duplicate_tiles);
                let mut start = 0usize;
                while start < n {
                    let count = (n - start).min(65_536);
                    dataset
                        .features
                        .read_chunk_into(start, count, &mut fbuf, &mut raw)?;
                    for (off, f) in fbuf.iter().enumerate() {
                        let bbox = f.bounds;
                        let mut key = cell(bbox);
                        while !tips.contains_key(&key) {
                            key >>= 2;
                        }
                        leaves.clear();
                        let duplicated = has_children.contains(&key)
                            && collect_covering_leaves(
                                key,
                                bbox,
                                zooms,
                                &tips,
                                &has_children,
                                max_duplicate_tiles,
                                &mut leaves,
                            )
                            && leaves.len() > 1;
                        if duplicated {
                            let mut canonical = None;
                            for &(leaf_tip, tb) in &leaves {
                                let dup = (if bbox[0] < tb[0] { 64u16 } else { 0 })
                                    | (if bbox[3] > tb[3] { 128u16 } else { 0 });
                                let t = tip_to_tile[leaf_tip as usize] as usize;
                                tile_offsets[t + 1] += 1;
                                if dup == 0 {
                                    canonical = Some(leaf_tip);
                                }
                            }
                            home_tip[start + off] = canonical.unwrap_or(leaves[0].0);
                        } else {
                            let home = tips[&key];
                            home_tip[start + off] = home;
                            tile_offsets[tip_to_tile[home as usize] as usize + 1] += 1;
                        }
                    }
                    start += count;
                }
                for t in 0..num_tiles {
                    tile_offsets[t + 1] += tile_offsets[t];
                }
                let total_placements = tile_offsets[num_tiles];
                tile_placements = vec![(0u32, 0u16); total_placements];
                let mut cursor = tile_offsets[..num_tiles].to_vec();
                start = 0;
                while start < n {
                    let count = (n - start).min(65_536);
                    dataset
                        .features
                        .read_chunk_into(start, count, &mut fbuf, &mut raw)?;
                    for (off, f) in fbuf.iter().enumerate() {
                        let fi = (start + off) as u32;
                        let bbox = f.bounds;
                        let mut key = cell(bbox);
                        while !tips.contains_key(&key) {
                            key >>= 2;
                        }
                        leaves.clear();
                        let duplicated = has_children.contains(&key)
                            && collect_covering_leaves(
                                key,
                                bbox,
                                zooms,
                                &tips,
                                &has_children,
                                max_duplicate_tiles,
                                &mut leaves,
                            )
                            && leaves.len() > 1;
                        if duplicated {
                            for &(leaf_tip, tb) in &leaves {
                                let dup = (if bbox[0] < tb[0] { 64u16 } else { 0 })
                                    | (if bbox[3] > tb[3] { 128u16 } else { 0 });
                                let t = tip_to_tile[leaf_tip as usize] as usize;
                                let pos = cursor[t];
                                tile_placements[pos] = (fi, dup);
                                cursor[t] = pos + 1;
                            }
                        } else {
                            let home = tips[&key];
                            let t = tip_to_tile[home as usize] as usize;
                            let pos = cursor[t];
                            tile_placements[pos] = (fi, 0);
                            cursor[t] = pos + 1;
                        }
                    }
                    start += count;
                }
            }
        }

        // Sort each tile's placement slice by `obj_id` (matching PRIMARY KEY(tip, id)) in parallel.
        if let Some(mem_features) = dataset.features.as_mem() {
            if workers <= 1 || num_tiles <= 1 {
                for t in 0..num_tiles {
                    let slice = &mut tile_placements[tile_offsets[t]..tile_offsets[t + 1]];
                    slice.sort_unstable_by_key(|&(fi, _)| mem_features[fi as usize].obj_id);
                }
            } else {
                let mut rest = tile_placements.as_mut_slice();
                let mut slices = Vec::with_capacity(num_tiles);
                for t in 0..num_tiles {
                    let len = tile_offsets[t + 1] - tile_offsets[t];
                    let (s, tail) = rest.split_at_mut(len);
                    rest = tail;
                    slices.push(s);
                }
                let per_worker = num_tiles.div_ceil(workers).max(1);
                std::thread::scope(|scope| {
                    for group in slices.chunks_mut(per_worker) {
                        scope.spawn(move || {
                            for slice in group {
                                slice.sort_unstable_by_key(|&(fi, _)| {
                                    mem_features[fi as usize].obj_id
                                });
                            }
                        });
                    }
                });
            }
        } else {
            let mut keyed: Vec<(i64, u32, u16)> = Vec::new();
            for t in 0..num_tiles {
                let slice = &mut tile_placements[tile_offsets[t]..tile_offsets[t + 1]];
                keyed.clear();
                keyed.reserve(slice.len());
                for &(fi, dup) in slice.iter() {
                    let obj_id = dataset.feature(fi as usize)?.obj_id;
                    keyed.push((obj_id, fi, dup));
                }
                keyed.sort_unstable_by_key(|&(id, _, _)| id);
                for (dst, &(_, fi, dup)) in slice.iter_mut().zip(&keyed) {
                    *dst = (fi, dup);
                }
            }
        }

        // Step 4: identify cross-tile exports in parallel across tiles.
        let is_exported: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(false)).collect();
        let mark_tile_exports = |t: usize| {
            let tip = dir.tips[t];
            let slice = &tile_placements[tile_offsets[t]..tile_offsets[t + 1]];
            for &(fi, _) in slice {
                let i = fi as usize;
                if i >= dataset.rel_base {
                    for &(child_fi, _) in dataset.rel_members(i) {
                        if home_tip[child_fi as usize] != tip {
                            is_exported[child_fi as usize].store(true, Ordering::Relaxed);
                        }
                    }
                }
                if dataset.flags[i] & 4 != 0 {
                    for &parent_fi in dataset.parent_links(i) {
                        if home_tip[parent_fi as usize] != tip {
                            is_exported[parent_fi as usize].store(true, Ordering::Relaxed);
                        }
                    }
                }
                if i >= dataset.way_base && i < dataset.rel_base && dataset.flags[i] & 32 != 0 {
                    for &node_fi in dataset.way_fnodes(i) {
                        if home_tip[node_fi as usize] != tip {
                            is_exported[node_fi as usize].store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
        };

        if num_tiles > 1 {
            if workers <= 1 {
                for t in 0..num_tiles {
                    mark_tile_exports(t);
                }
            } else {
                let next_t = std::sync::atomic::AtomicUsize::new(0);
                std::thread::scope(|scope| {
                    for _ in 0..workers.min(num_tiles) {
                        let next_t = &next_t;
                        let mark = &mark_tile_exports;
                        scope.spawn(move || {
                            loop {
                                let t = next_t.fetch_add(1, Ordering::Relaxed);
                                if t >= num_tiles {
                                    break;
                                }
                                mark(t);
                            }
                        });
                    }
                });
            }
        }

        // Step 5: assign deterministic `tex` indices per home tile in `obj_id` order.
        let tex_atomic: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(u32::MAX)).collect();
        let assign_tile_tex = |t: usize| {
            let tip = dir.tips[t];
            let slice = &tile_placements[tile_offsets[t]..tile_offsets[t + 1]];
            let mut next_tex = 0u32;
            for &(fi, _) in slice {
                let i = fi as usize;
                if home_tip[i] == tip && is_exported[i].load(Ordering::Relaxed) {
                    tex_atomic[i].store(next_tex, Ordering::Relaxed);
                    next_tex += 1;
                }
            }
        };

        if num_tiles > 1 {
            if workers <= 1 {
                for t in 0..num_tiles {
                    assign_tile_tex(t);
                }
            } else {
                let next_t = std::sync::atomic::AtomicUsize::new(0);
                std::thread::scope(|scope| {
                    for _ in 0..workers.min(num_tiles) {
                        let next_t = &next_t;
                        let assign = &assign_tile_tex;
                        scope.spawn(move || {
                            loop {
                                let t = next_t.fetch_add(1, Ordering::Relaxed);
                                if t >= num_tiles {
                                    break;
                                }
                                assign(t);
                            }
                        });
                    }
                });
            }
        }
        let tex: Vec<u32> = tex_atomic.into_iter().map(AtomicU32::into_inner).collect();

        Ok((
            dir,
            Placement {
                home_tip,
                tex,
                tile_placements,
                tile_offsets,
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn branch(
        &mut self,
        key: u32,
        slot: usize,
        weights: &FxHashMap<u32, u64>,
        target: u64,
        tips: &mut FxHashMap<u32, u32>,
        has_children: &mut FxHashSet<u32>,
        zooms: TileZooms,
    ) {
        let mut mask = 0u32;
        if weights.get(&key).copied().unwrap_or(0) > target {
            for (slot, child_key) in zooms.children(key).enumerate() {
                if weights.contains_key(&child_key) {
                    mask |= 1 << slot;
                }
            }
        }
        let entry = if key == 1 || mask == 0 {
            slot
        } else {
            let entry = self.words.len();
            self.words[slot] = ((entry - slot) * 4) as u32 | 1;
            self.words.extend([0, 0]);
            entry
        };
        tips.insert(key, entry as u32);
        self.tips.push(entry as u32);
        let zoom = key.ilog2() / 2;
        self.zooms |= 1 << zoom;
        if mask != 0 {
            has_children.insert(key);
            self.words[entry + 1] = mask;
            let mut child = self.words.len();
            self.words.resize(child + mask.count_ones() as usize, 0);
            for (slot, child_key) in zooms.children(key).enumerate() {
                if mask & (1 << slot) != 0 {
                    self.branch(child_key, child, weights, target, tips, has_children, zooms);
                    child += 1;
                }
            }
        }
    }
}
