//! Transparent memory-budget detection and disk-backed spill storage for the
//! pure-Rust GOL builder.
//!
//! When available memory is sufficient, the builder runs entirely in RAM with
//! zero disk I/O overhead. When the input exceeds the memory budget (detected
//! automatically from Linux `/proc/meminfo` and cgroups, or configured via
//! `BuildOptions::memory_limit_bytes`), large staging and resolved buffers spill
//! transparently into temporary files beside the output GOL and are removed
//! automatically on completion or error.
use super::strings::FxHashMap;
use anyhow::{Context, Result, ensure};
use std::{
    fs::{File, OpenOptions},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    marker::PhantomData,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};
use tempfile::TempDir;

pub(super) const PAGE_SHIFT: usize = 16;
pub(super) const PAGE_SIZE: usize = 1 << PAGE_SHIFT; // 64 KiB
const CACHE_SHARDS: usize = 16;
const WRITER_BUF_BYTES: usize = 256 * 1024;

/// Detect available memory in bytes (Linux `/proc/meminfo` + cgroup v2/v1).
/// Returns `None` on platforms where automatic detection is unavailable.
pub(super) fn detect_available_memory() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let mut best: Option<u64> = None;
        let mut consider = |bytes: u64| {
            best = Some(best.map_or(bytes, |cur| cur.min(bytes)));
        };

        if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
            for line in meminfo.lines() {
                if let Some(rest) = line.strip_prefix("MemAvailable:") {
                    if let Some(kib) = rest
                        .split_whitespace()
                        .next()
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        consider(kib.saturating_mul(1024));
                    }
                    break;
                }
            }
        }

        // cgroup v2 memory limit (memory.max / memory.high minus memory.current).
        let read_u64 = |path: &str| -> Option<u64> {
            std::fs::read_to_string(path)
                .ok()?
                .trim()
                .parse::<u64>()
                .ok()
        };
        let cg2_current = read_u64("/sys/fs/cgroup/memory.current").unwrap_or(0);
        for path in ["/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory.high"] {
            if let Some(limit) = read_u64(path) {
                consider(limit.saturating_sub(cg2_current));
            }
        }

        // cgroup v1 fallback.
        if let Some(limit) = read_u64("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
            if limit < (1u64 << 60) {
                let usage = read_u64("/sys/fs/cgroup/memory/memory.usage_in_bytes").unwrap_or(0);
                consider(limit.saturating_sub(usage));
            }
        }

        best
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Coordinates transparent spilling and owns the lazy temporary spill directory.
pub(super) struct MemoryBudget {
    parent: PathBuf,
    limit_bytes: Option<u64>,
    spilling: AtomicBool,
    live_bytes: AtomicU64,
    temp_dir: Mutex<Option<TempDir>>,
}

impl MemoryBudget {
    pub(super) fn new(parent: &Path, explicit_limit: Option<u64>, pbf_bytes: u64) -> Self {
        let limit_bytes = match explicit_limit {
            Some(limit) => Some(limit),
            None => detect_available_memory().map(|avail| (avail / 5).saturating_mul(3)),
        };
        // Compressed OSM PBFs expand ~6-8x in peak in-memory staging RAM.
        // Start spilling upfront when the estimated peak exceeds the budget.
        let initial_spill =
            limit_bytes.is_some_and(|budget| budget == 0 || pbf_bytes.saturating_mul(8) > budget);
        Self {
            parent: parent.to_path_buf(),
            limit_bytes,
            spilling: AtomicBool::new(initial_spill),
            live_bytes: AtomicU64::new(0),
            temp_dir: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn unlimited() -> Self {
        Self {
            parent: std::env::temp_dir(),
            limit_bytes: None,
            spilling: AtomicBool::new(false),
            live_bytes: AtomicU64::new(0),
            temp_dir: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn force_spill_in(parent: &Path, budget_bytes: u64) -> Self {
        Self {
            parent: parent.to_path_buf(),
            limit_bytes: Some(budget_bytes),
            spilling: AtomicBool::new(true),
            live_bytes: AtomicU64::new(0),
            temp_dir: Mutex::new(None),
        }
    }

    #[inline]
    pub(super) fn is_spilling(&self) -> bool {
        self.spilling.load(Ordering::Relaxed)
    }

    /// Account for newly retained in-memory staging bytes; returns `true` once
    /// spilling is active (either previously or triggered by this allocation).
    #[inline]
    pub(super) fn account(&self, added_bytes: u64) -> bool {
        if self.spilling.load(Ordering::Relaxed) {
            return true;
        }
        let Some(budget) = self.limit_bytes else {
            return false;
        };
        let total = self
            .live_bytes
            .fetch_add(added_bytes, Ordering::Relaxed)
            .saturating_add(added_bytes);
        if total > budget / 2 {
            self.spilling.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// Create a fresh read/write temporary spill file inside the lazy temp dir.
    pub(super) fn create_file(&self, name: &str) -> Result<File> {
        let mut guard = self.temp_dir.lock().unwrap();
        if guard.is_none() {
            let dir = tempfile::Builder::new()
                .prefix(".regol-spill-")
                .tempdir_in(&self.parent)
                .with_context(|| format!("create spill directory in {}", self.parent.display()))?;
            *guard = Some(dir);
        }
        let path = guard.as_ref().unwrap().path().join(name);
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("create spill file {}", path.display()))
    }

    /// Remove a named spill file early once it is no longer needed, reclaiming disk space.
    pub(super) fn remove_file(&self, name: &str) {
        let guard = self.temp_dir.lock().unwrap();
        if let Some(dir) = guard.as_ref() {
            let _ = std::fs::remove_file(dir.path().join(name));
        }
    }

    pub(super) fn cache_bytes(&self) -> usize {
        let budget = self.limit_bytes.unwrap_or(128 * 1024 * 1024);
        (budget / 16).clamp(1024 * 1024, 16 * 1024 * 1024) as usize
    }

    pub(super) fn sort_buffer_bytes(&self) -> usize {
        let budget = self.limit_bytes.unwrap_or(128 * 1024 * 1024);
        (budget / 4).clamp(1024 * 1024, 128 * 1024 * 1024) as usize
    }

    /// Cap worker parallelism under tight memory budgets so per-worker scratch
    /// buffers and allocator arenas fit comfortably inside the budget.
    pub(super) fn max_workers(&self, default_workers: usize) -> usize {
        if !self.is_spilling() {
            return default_workers;
        }
        match self.limit_bytes {
            Some(budget) => {
                let allowed = (budget / (64 * 1024 * 1024)).clamp(1, 8) as usize;
                default_workers.min(allowed).max(1)
            }
            None => default_workers,
        }
    }

    /// Cap in-flight encoded tiles in `store::write_parallel`.
    pub(super) fn tile_wave_limit(&self, target_tile_bytes: u64, default_wave: usize) -> usize {
        if !self.is_spilling() {
            return default_wave;
        }
        match self.limit_bytes {
            Some(budget) => {
                let allowed = ((budget / 8) / target_tile_bytes.max(1)).clamp(2, 8) as usize;
                default_wave.min(allowed).max(2)
            }
            None => default_wave,
        }
    }
}

/// Cross-platform positioned read (100% safe Rust).
fn read_exact_at(file: &File, mut offset: u64, mut dst: &mut [u8]) -> io::Result<()> {
    while !dst.is_empty() {
        #[cfg(unix)]
        let n = {
            use std::os::unix::fs::FileExt;
            file.read_at(dst, offset)?
        };
        #[cfg(windows)]
        let n = {
            use std::os::windows::fs::FileExt;
            file.seek_read(dst, offset)?
        };
        #[cfg(not(any(unix, windows)))]
        let n = {
            let mut f = file;
            f.seek(SeekFrom::Start(offset))?;
            f.read(dst)?
        };
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "unexpected EOF in spill file",
            ));
        }
        offset += n as u64;
        dst = &mut dst[n..];
    }
    Ok(())
}

/// Sequential buffered writer that converts cleanly into a read-only `PagedBlob`.
pub(super) struct BlobWriter {
    writer: BufWriter<File>,
    written: u64,
}

impl BlobWriter {
    pub(super) fn new(file: File) -> Self {
        Self {
            writer: BufWriter::with_capacity(WRITER_BUF_BYTES, file),
            written: 0,
        }
    }

    pub(super) fn append(&mut self, bytes: &[u8]) -> Result<u64> {
        let start = self.written;
        self.writer.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(start)
    }

    pub(super) fn into_file(mut self) -> Result<(File, u64)> {
        self.writer.flush()?;
        let file = self
            .writer
            .into_inner()
            .map_err(|e| anyhow::anyhow!("flush spill writer: {}", e.error()))?;
        Ok((file, self.written))
    }

    pub(super) fn into_paged_blob(self, cache_bytes: usize) -> Result<PagedBlob> {
        let (file, len) = self.into_file()?;
        Ok(PagedBlob::new(file, len, cache_bytes))
    }
}

struct ShardCache {
    capacity: usize,
    entries: Vec<(u64, Arc<[u8]>)>,
    index: FxHashMap<u64, usize>,
    hand: usize,
}

impl ShardCache {
    fn new(capacity: usize) -> Self {
        let cap = capacity.max(2);
        Self {
            capacity: cap,
            entries: Vec::with_capacity(cap),
            index: FxHashMap::with_capacity_and_hasher(cap * 2, Default::default()),
            hand: 0,
        }
    }

    fn get(&self, page: u64) -> Option<Arc<[u8]>> {
        self.index
            .get(&page)
            .map(|&slot| Arc::clone(&self.entries[slot].1))
    }

    fn insert(&mut self, page: u64, data: Arc<[u8]>) -> Arc<[u8]> {
        if let Some(&slot) = self.index.get(&page) {
            return Arc::clone(&self.entries[slot].1);
        }
        if self.entries.len() < self.capacity {
            let slot = self.entries.len();
            self.entries.push((page, Arc::clone(&data)));
            self.index.insert(page, slot);
        } else {
            let slot = self.hand;
            self.hand = (self.hand + 1) % self.capacity;
            let (old_page, _) = &self.entries[slot];
            self.index.remove(old_page);
            self.entries[slot] = (page, Arc::clone(&data));
            self.index.insert(page, slot);
        }
        data
    }
}

/// Thread-safe, bounded-memory paged reader over a spill file.
pub(super) struct PagedBlob {
    file: File,
    len: u64,
    shards: Vec<Mutex<ShardCache>>,
}

impl PagedBlob {
    pub(super) fn new(file: File, len: u64, cache_bytes: usize) -> Self {
        let total_pages = (cache_bytes / PAGE_SIZE).max(CACHE_SHARDS * 2);
        let per_shard = (total_pages / CACHE_SHARDS).max(2);
        let shards = (0..CACHE_SHARDS)
            .map(|_| Mutex::new(ShardCache::new(per_shard)))
            .collect();
        Self { file, len, shards }
    }

    #[inline]
    pub(super) fn len(&self) -> u64 {
        self.len
    }

    /// Return a cached `Arc<[u8]>` handle to the 64 KiB page `page_idx`.
    pub(super) fn read_page(&self, page_idx: u64) -> Result<Arc<[u8]>> {
        let shard = &self.shards[(page_idx as usize) & (CACHE_SHARDS - 1)];
        if let Some(hit) = shard.lock().unwrap().get(page_idx) {
            return Ok(hit);
        }
        let offset = page_idx << PAGE_SHIFT;
        ensure!(offset < self.len, "spill page offset out of bounds");
        let page_len = ((self.len - offset).min(PAGE_SIZE as u64)) as usize;
        let mut buf = vec![0u8; page_len];
        read_exact_at(&self.file, offset, &mut buf)?;
        let arc: Arc<[u8]> = Arc::from(buf.into_boxed_slice());
        Ok(shard.lock().unwrap().insert(page_idx, arc))
    }

    /// Append `len` bytes starting at `offset` into `dst`.
    pub(super) fn append_range(
        &self,
        mut offset: u64,
        mut len: usize,
        dst: &mut Vec<u8>,
    ) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        ensure!(
            offset.saturating_add(len as u64) <= self.len,
            "spill read out of bounds"
        );
        dst.reserve(len);
        while len > 0 {
            let page_idx = offset >> PAGE_SHIFT;
            let in_page = (offset as usize) & (PAGE_SIZE - 1);
            let page = self.read_page(page_idx)?;
            let take = len.min(page.len() - in_page);
            dst.extend_from_slice(&page[in_page..in_page + take]);
            offset += take as u64;
            len -= take;
        }
        Ok(())
    }

    /// Fill `dst` with exact bytes starting at `offset`.
    pub(super) fn read_exact(&self, mut offset: u64, mut dst: &mut [u8]) -> Result<()> {
        if dst.is_empty() {
            return Ok(());
        }
        ensure!(
            offset.saturating_add(dst.len() as u64) <= self.len,
            "spill read out of bounds"
        );
        while !dst.is_empty() {
            let page_idx = offset >> PAGE_SHIFT;
            let in_page = (offset as usize) & (PAGE_SIZE - 1);
            let page = self.read_page(page_idx)?;
            let take = dst.len().min(page.len() - in_page);
            dst[..take].copy_from_slice(&page[in_page..in_page + take]);
            offset += take as u64;
            dst = &mut dst[take..];
        }
        Ok(())
    }
}

/// Byte blob stored either in RAM (`Vec<u8>`) or spilled on disk (`PagedBlob`).
pub(super) enum BytesStore {
    Mem(Vec<u8>),
    Disk(PagedBlob),
}

impl BytesStore {
    #[inline]
    pub(super) fn append_range(&self, start: u64, len: usize, dst: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Mem(buf) => {
                let s = start as usize;
                dst.extend_from_slice(&buf[s..s + len]);
                Ok(())
            }
            Self::Disk(blob) => blob.append_range(start, len, dst),
        }
    }
}

/// Fixed-width little-endian POD serialization for safe disk-backed arrays.
pub(super) trait PodCodec: Copy + Send + Sync {
    const SIZE: usize;
    fn encode(&self, dst: &mut [u8]);
    fn decode(src: &[u8]) -> Self;
}

impl PodCodec for (u32, u32) {
    const SIZE: usize = 8;

    #[inline]
    fn encode(&self, dst: &mut [u8]) {
        dst[0..4].copy_from_slice(&self.0.to_le_bytes());
        dst[4..8].copy_from_slice(&self.1.to_le_bytes());
    }

    #[inline]
    fn decode(src: &[u8]) -> Self {
        (
            u32::from_le_bytes(src[0..4].try_into().unwrap()),
            u32::from_le_bytes(src[4..8].try_into().unwrap()),
        )
    }
}

impl PodCodec for (u64, u32) {
    const SIZE: usize = 12;

    #[inline]
    fn encode(&self, dst: &mut [u8]) {
        dst[0..8].copy_from_slice(&self.0.to_le_bytes());
        dst[8..12].copy_from_slice(&self.1.to_le_bytes());
    }

    #[inline]
    fn decode(src: &[u8]) -> Self {
        (
            u64::from_le_bytes(src[0..8].try_into().unwrap()),
            u32::from_le_bytes(src[8..12].try_into().unwrap()),
        )
    }
}

/// Fixed-record array stored either in RAM (`Vec<T>`) or spilled on disk (`PagedBlob`).
pub(super) enum PodStore<T: PodCodec> {
    Mem(Vec<T>),
    Disk {
        blob: PagedBlob,
        len: usize,
        _marker: PhantomData<T>,
    },
}

impl<T: PodCodec> PodStore<T> {
    pub(super) fn from_disk(blob: PagedBlob, len: usize) -> Self {
        debug_assert_eq!(blob.len(), (len as u64) * (T::SIZE as u64));
        Self::Disk {
            blob,
            len,
            _marker: PhantomData,
        }
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Mem(v) => v.len(),
            Self::Disk { len, .. } => *len,
        }
    }

    #[inline]
    pub(super) fn as_mem(&self) -> Option<&[T]> {
        match self {
            Self::Mem(v) => Some(v.as_slice()),
            Self::Disk { .. } => None,
        }
    }

    #[inline]
    pub(super) fn get(&self, idx: usize) -> Result<T> {
        match self {
            Self::Mem(v) => Ok(v[idx]),
            Self::Disk { blob, .. } => {
                let mut raw = [0u8; 64];
                let slot = &mut raw[..T::SIZE];
                blob.read_exact((idx as u64) * (T::SIZE as u64), slot)?;
                Ok(T::decode(slot))
            }
        }
    }

    /// Borrow a slice directly if in memory, or decode into `scratch` if spilled.
    #[inline]
    pub(super) fn slice_or_read<'a>(
        &'a self,
        start: usize,
        len: usize,
        scratch: &'a mut Vec<T>,
    ) -> Result<&'a [T]> {
        match self {
            Self::Mem(v) => Ok(&v[start..start + len]),
            Self::Disk { blob, .. } => {
                scratch.clear();
                if len == 0 {
                    return Ok(&[]);
                }
                let byte_len = len * T::SIZE;
                let mut raw = vec![0u8; byte_len];
                blob.read_exact((start as u64) * (T::SIZE as u64), &mut raw)?;
                scratch.reserve(len);
                for chunk in raw.chunks_exact(T::SIZE) {
                    scratch.push(T::decode(chunk));
                }
                Ok(scratch.as_slice())
            }
        }
    }

    /// Read `count` consecutive items starting at `start` into `dst`.
    pub(super) fn read_chunk_into(
        &self,
        start: usize,
        count: usize,
        dst: &mut Vec<T>,
        raw_buf: &mut Vec<u8>,
    ) -> Result<()> {
        dst.clear();
        if count == 0 {
            return Ok(());
        }
        match self {
            Self::Mem(v) => {
                dst.extend_from_slice(&v[start..start + count]);
                Ok(())
            }
            Self::Disk { blob, .. } => {
                let byte_len = count * T::SIZE;
                raw_buf.resize(byte_len, 0);
                blob.read_exact((start as u64) * (T::SIZE as u64), raw_buf)?;
                dst.reserve(count);
                for chunk in raw_buf.chunks_exact(T::SIZE) {
                    dst.push(T::decode(chunk));
                }
                Ok(())
            }
        }
    }
}

/// Compact O(1) rank index mapping `node_idx -> feat_idx` (or `u32::MAX` if not
/// a node feature) in 1.5 bits/node (21.3x smaller than `Vec<u32>`).
pub(super) struct BitRank {
    words: Vec<u64>,
    prefix: Vec<u32>,
    total: usize,
}

impl BitRank {
    pub(super) fn from_node_flags(
        node_flags: &[AtomicU8],
        is_feature_bit: u8,
        used_by_way_bit: u8,
    ) -> Self {
        let num_words = node_flags.len().div_ceil(64);
        let mut words = vec![0u64; num_words];
        let mut prefix = Vec::with_capacity(num_words);
        let mut running = 0u32;
        for (wi, chunk) in node_flags.chunks(64).enumerate() {
            prefix.push(running);
            let mut bits = 0u64;
            for (bi, f) in chunk.iter().enumerate() {
                let v = f.load(Ordering::Relaxed);
                if (v & is_feature_bit) != 0 || (v & used_by_way_bit) == 0 {
                    bits |= 1u64 << bi;
                }
            }
            words[wi] = bits;
            running += bits.count_ones();
        }
        Self {
            words,
            prefix,
            total: running as usize,
        }
    }

    #[inline]
    pub(super) fn total_features(&self) -> usize {
        self.total
    }

    #[inline]
    pub(super) fn get(&self, nidx: usize) -> u32 {
        let w = nidx >> 6;
        let bit = nidx & 63;
        let word = self.words[w];
        if (word >> bit) & 1 == 0 {
            return u32::MAX;
        }
        let mask = (1u64 << bit) - 1;
        self.prefix[w] + (word & mask).count_ones()
    }
}

/// Metadata for a spilled raw node chunk written during PBF decoding.
#[derive(Clone, Copy)]
pub(super) struct SpillNodeChunk {
    pub seq: usize,
    pub sorted: bool,
    pub min_id: i64,
    pub max_id: i64,
    pub offset: u64,
    pub count: usize,
}

/// Metadata for a spilled raw way chunk written during PBF decoding.
#[derive(Clone, Copy)]
pub(super) struct SpillWayChunk {
    pub min_id: i64,
    pub ways_offset: u64,
    pub ways_count: usize,
    pub refs_offset: u64,
    pub refs_count: usize,
}

/// Metadata for a spilled raw tag chunk belonging to a single PBF worker.
#[derive(Clone, Copy)]
pub(super) struct SpillTagChunk {
    pub offset: u64,
    pub count: usize,
}

/// Shared lazy spill files populated by PBF decoder workers whenever memory
/// pressure triggers spilling.
pub(super) struct RawSpillSink {
    nodes: Mutex<Option<BlobWriter>>,
    ways: Mutex<Option<BlobWriter>>,
    way_refs: Mutex<Option<BlobWriter>>,
    tags: Mutex<Option<BlobWriter>>,
}

impl RawSpillSink {
    pub(super) fn new() -> Self {
        Self {
            nodes: Mutex::new(None),
            ways: Mutex::new(None),
            way_refs: Mutex::new(None),
            tags: Mutex::new(None),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn spill_nodes(
        &self,
        budget: &MemoryBudget,
        seq: usize,
        sorted: bool,
        min_id: i64,
        max_id: i64,
        bytes: &[u8],
        count: usize,
    ) -> Result<SpillNodeChunk> {
        let mut guard = self.nodes.lock().unwrap();
        if guard.is_none() {
            *guard = Some(BlobWriter::new(budget.create_file("nodes_raw.spill")?));
        }
        let offset = guard.as_mut().unwrap().append(bytes)?;
        Ok(SpillNodeChunk {
            seq,
            sorted,
            min_id,
            max_id,
            offset,
            count,
        })
    }

    pub(super) fn spill_ways(
        &self,
        budget: &MemoryBudget,
        min_id: i64,
        ways_bytes: &[u8],
        ways_count: usize,
        refs_bytes: &[u8],
        refs_count: usize,
    ) -> Result<SpillWayChunk> {
        let ways_offset = {
            let mut guard = self.ways.lock().unwrap();
            if guard.is_none() {
                *guard = Some(BlobWriter::new(budget.create_file("ways_raw.spill")?));
            }
            guard.as_mut().unwrap().append(ways_bytes)?
        };
        let refs_offset = {
            let mut guard = self.way_refs.lock().unwrap();
            if guard.is_none() {
                *guard = Some(BlobWriter::new(budget.create_file("way_refs_raw.spill")?));
            }
            guard.as_mut().unwrap().append(refs_bytes)?
        };
        Ok(SpillWayChunk {
            min_id,
            ways_offset,
            ways_count,
            refs_offset,
            refs_count,
        })
    }

    pub(super) fn spill_tags(
        &self,
        budget: &MemoryBudget,
        bytes: &[u8],
        count: usize,
    ) -> Result<SpillTagChunk> {
        let mut guard = self.tags.lock().unwrap();
        if guard.is_none() {
            *guard = Some(BlobWriter::new(budget.create_file("tags_raw.spill")?));
        }
        let offset = guard.as_mut().unwrap().append(bytes)?;
        Ok(SpillTagChunk { offset, count })
    }

    pub(super) fn take_nodes_file(&self) -> Result<Option<(File, u64)>> {
        self.nodes
            .lock()
            .unwrap()
            .take()
            .map(BlobWriter::into_file)
            .transpose()
    }

    pub(super) fn take_ways_files(&self) -> Result<(Option<File>, Option<File>)> {
        let ways = self
            .ways
            .lock()
            .unwrap()
            .take()
            .map(BlobWriter::into_file)
            .transpose()?
            .map(|(f, _)| f);
        let refs = self
            .way_refs
            .lock()
            .unwrap()
            .take()
            .map(BlobWriter::into_file)
            .transpose()?
            .map(|(f, _)| f);
        Ok((ways, refs))
    }

    pub(super) fn take_tags_file(&self) -> Result<Option<(File, u64)>> {
        self.tags
            .lock()
            .unwrap()
            .take()
            .map(BlobWriter::into_file)
            .transpose()
    }
}

/// Read exact bytes from a raw spill `File` at `offset`.
pub(super) fn read_file_exact_at(file: &File, offset: u64, dst: &mut [u8]) -> Result<()> {
    read_exact_at(file, offset, dst).context("read raw spill file")
}

/// Helper for streaming a `File` sequentially from offset 0.
pub(super) fn sequential_reader(mut file: File) -> Result<std::io::BufReader<File>> {
    file.seek(SeekFrom::Start(0))?;
    Ok(std::io::BufReader::with_capacity(WRITER_BUF_BYTES, file))
}

#[allow(dead_code)]
pub(super) fn read_exact_or_eof(reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paged_blob_round_trips_across_page_boundaries_and_evictions() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let budget = MemoryBudget::force_spill_in(dir.path(), 4 * 1024 * 1024);
        let file = budget.create_file("test.blob")?;
        let mut writer = BlobWriter::new(file);
        // Write > 4 MiB so the small cache has to evict pages.
        let mut expected = Vec::with_capacity(512 * 1024);
        for i in 0..(512 * 1024 / 4) {
            expected.extend_from_slice(&(i as u32).to_le_bytes());
        }
        writer.append(&expected)?;
        let blob = writer.into_paged_blob(2 * 1024 * 1024)?;
        assert_eq!(blob.len(), expected.len() as u64);

        // Read ranges straddling 64 KiB page boundaries.
        for &(off, len) in &[
            (0usize, 100usize),
            (65_530, 32),
            (65_536, 65_536),
            (200_000, 140_000),
            (expected.len() - 17, 17),
        ] {
            let mut got = Vec::new();
            blob.append_range(off as u64, len, &mut got)?;
            assert_eq!(got, &expected[off..off + len], "off={off} len={len}");
            let mut exact = vec![0u8; len];
            blob.read_exact(off as u64, &mut exact)?;
            assert_eq!(exact, &expected[off..off + len]);
        }
        Ok(())
    }

    #[test]
    fn bit_rank_maps_exact_feature_indices() {
        let flags: Vec<AtomicU8> = (0..300)
            .map(|i| {
                // Features are indices divisible by 3 or 5.
                let v = if i % 3 == 0 || i % 5 == 0 { 16 } else { 8 };
                AtomicU8::new(v)
            })
            .collect();
        let rank = BitRank::from_node_flags(&flags, 16, 8);
        let mut expected_idx = 0u32;
        for i in 0..300 {
            if i % 3 == 0 || i % 5 == 0 {
                assert_eq!(rank.get(i), expected_idx, "i={i}");
                expected_idx += 1;
            } else {
                assert_eq!(rank.get(i), u32::MAX, "i={i}");
            }
        }
        assert_eq!(rank.total_features(), expected_idx as usize);
    }
}
