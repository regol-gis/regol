use super::{Features, Work};
use crate::{
    Bounds, Library,
    source::{MappedFile, Source, WorkerMap},
};
use std::{
    collections::VecDeque,
    fs::File,
    os::{fd::AsRawFd, unix::fs::FileExt},
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, SyncSender},
    },
};

const GOL_PAGE: usize = 4096;
const ROOT_PAGES: usize = 6;
const ROOT_SPAN: u64 = 64 * 1024;
const TILE_SPANS: u64 = 3 * ROOT_SPAN;
const WINDOW: usize = 64;
const WORKERS: usize = 16;
const MINIMUM_TILES: usize = 4;
const STEP_BLOCK: u64 = 128;
const DIRECTORY_LOOKAHEAD_STEPS: usize = 256 * WINDOW;

struct Scan {
    library: Weak<WorkerMap>,
    bounds: Option<Bounds>,
    cancelled: AtomicBool,
    remaining_steps: AtomicU64,
}

struct Job {
    scan: Weak<Scan>,
    tile: u64,
    bounds: Bounds,
    types: u8,
    required: u32,
    alternative: u32,
    _permit: Permit,
}

static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

struct Permit;

impl Permit {
    #[allow(deprecated)] // Keep fetch_update for the minimum supported Rust version.
    fn acquire() -> Option<Self> {
        IN_FLIGHT
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < WINDOW).then_some(count + 1)
            })
            .ok()?;
        Some(Self)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
    }
}

fn pool() -> Option<&'static SyncSender<Job>> {
    static POOL: OnceLock<Option<SyncSender<Job>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (send, receive) = mpsc::sync_channel::<Job>(WINDOW);
        let receive = Arc::new(Mutex::new(receive));
        let mut started = false;
        for _ in 0..WORKERS {
            let receive = Arc::clone(&receive);
            let worker = std::thread::Builder::new()
                .name("regol-mmap-ahead".into())
                .spawn(move || {
                    loop {
                        let job = {
                            let Ok(receive) = receive.lock() else { break };
                            receive.recv()
                        };
                        let Ok(job) = job else { break };
                        complete(job);
                    }
                });
            if worker.is_err() {
                break;
            }
            started = true;
        }
        started.then_some(send)
    })
    .as_ref()
}

fn complete(job: Job) {
    let Some(scan) = job.scan.upgrade() else {
        return;
    };
    let Some(mapping) = scan.library.upgrade() else {
        return;
    };
    mapping.with_library(|library| {
        if live(&scan, &mapping) && scan.remaining_steps.load(Ordering::Relaxed) != 0 {
            scan_tile(&job, &scan, &mapping, library);
        }
    });
}

fn scan_tile(job: &Job, scan: &Scan, mapping: &WorkerMap, library: &Library<MappedFile>) {
    let source = library.source();
    let live = || live(scan, mapping);
    if scan.bounds.is_none() {
        hint_roots(
            source.file(),
            source.len(),
            RootHints {
                start: job.tile,
                types: job.types,
                required: job.required,
                alternative: job.alternative,
            },
            &live,
        );
    }
    let mut cursor = Features::new(library, scan.bounds)
        .restrict(job.types, job.required)
        .require_any(job.alternative);
    cursor.read_ahead = None;
    cursor.work.clear();
    if !live() || cursor.load_tile(job.tile, job.bounds).is_err() {
        return;
    }
    let mut step_block = 0;
    let mut last_page = None;
    let Some(bytes) = source.as_bytes() else {
        return;
    };
    let _ = cursor.next_with_budget::<true>(
        |feature, _, _| {
            let position = feature.checked_tag_table()?;
            let page = position / GOL_PAGE as u64;
            if last_page != Some(page) {
                if let Some(byte) = usize::try_from(position)
                    .ok()
                    .and_then(|offset| bytes.get(offset))
                {
                    std::hint::black_box(*byte);
                }
                last_page = Some(page);
            }
            Ok(false)
        },
        || {
            if step_block == 0 {
                if !live() {
                    return Ok(false);
                }
                #[allow(deprecated)] // Keep fetch_update for the minimum supported Rust version.
                let Ok(remaining) = scan.remaining_steps.fetch_update(
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                    |remaining| (remaining != 0).then(|| remaining.saturating_sub(STEP_BLOCK)),
                ) else {
                    return Ok(false);
                };
                step_block = remaining.min(STEP_BLOCK);
            }
            step_block -= 1;
            Ok(true)
        },
    );
}

struct Pages<'a> {
    file: &'a File,
    length: u64,
    start: u64,
    pages: Vec<(u64, Box<[u8; GOL_PAGE]>)>,
}

impl Pages<'_> {
    fn read<const N: usize>(
        &mut self,
        offset: u64,
        end: u64,
        live: &impl Fn() -> bool,
    ) -> Option<[u8; N]> {
        let finish = offset.checked_add(N as u64)?;
        if offset < self.start || finish > end || end > self.length {
            return None;
        }
        let mut bytes = [0; N];
        let mut copied = 0;
        while copied < N {
            let position = offset + copied as u64;
            let page = position & !(GOL_PAGE as u64 - 1);
            let slot = if let Some(slot) = self.pages.iter().position(|(start, _)| *start == page) {
                slot
            } else {
                if self.pages.len() == ROOT_PAGES || !live() {
                    return None;
                }
                let mut data = Box::new([0; GOL_PAGE]);
                let size = (self.length - page).min(GOL_PAGE as u64) as usize;
                self.file.read_exact_at(&mut data[..size], page).ok()?;
                self.pages.push((page, data));
                self.pages.len() - 1
            };
            let within = (position - page) as usize;
            let take = (N - copied).min(GOL_PAGE - within);
            bytes[copied..copied + take]
                .copy_from_slice(&self.pages[slot].1[within..within + take]);
            copied += take;
        }
        Some(bytes)
    }
}

struct RootHints {
    start: u64,
    types: u8,
    required: u32,
    alternative: u32,
}

fn hint_roots(file: &File, length: u64, roots: RootHints, live: &impl Fn() -> bool) {
    let mut pages = Pages {
        file,
        length,
        start: roots.start,
        pages: Vec::with_capacity(ROOT_PAGES),
    };
    let Some(header) = pages.read::<24>(roots.start, length, live) else {
        return;
    };
    let payload = u32::from_le_bytes(header[..4].try_into().expect("tile length")) & 0x3fff_ffff;
    let Some(end) = roots
        .start
        .checked_add(4 + u64::from(payload))
        .filter(|&end| end <= length && payload >= 24)
    else {
        return;
    };
    let mut heads = Vec::new();
    let mut selected = Vec::new();
    for (index, slot) in [8, 12, 16, 20].into_iter().enumerate() {
        let delta = i32::from_le_bytes(header[slot..slot + 4].try_into().expect("root pointer"));
        if delta == 0 || !live() {
            continue;
        }
        let Some(mut root) = (roots.start + slot as u64).checked_add_signed(i64::from(delta))
        else {
            continue;
        };
        for _ in 0..32 {
            let Some(record) = pages.read::<8>(root, end, live) else {
                break;
            };
            let link = i32::from_le_bytes(record[..4].try_into().expect("root link"));
            let keys = u32::from_le_bytes(record[4..].try_into().expect("root categories"));
            if let Some(target) = root
                .checked_add_signed(i64::from(link & !3))
                .filter(|&target| {
                    target >= roots.start
                        && target.checked_add(20).is_some_and(|finish| finish <= end)
                })
            {
                heads.push(target);
                if roots.types & (1 << index) != 0
                    && keys & roots.required == roots.required
                    && (roots.alternative == 0 || keys & roots.alternative != 0)
                {
                    selected.push(target);
                }
            }
            if link & 1 != 0 {
                break;
            }
            root += 8;
        }
    }
    hint_spans(file, live, end, &mut heads, &mut selected);
}

fn live(scan: &Scan, mapping: &WorkerMap) -> bool {
    mapping.active() && !scan.cancelled.load(Ordering::Relaxed)
}

fn hint_spans(
    file: &File,
    live: &impl Fn() -> bool,
    end: u64,
    heads: &mut Vec<u64>,
    selected: &mut Vec<u64>,
) {
    // The offset of the next root is a conservative bound when subtrees
    // share physical order. Demand traversal never relies on this guess.
    heads.sort_unstable();
    heads.dedup();
    selected.sort_unstable();
    selected.dedup();
    let mut remaining = TILE_SPANS;
    for &start in selected.iter() {
        if remaining == 0 || !live() {
            break;
        }
        let next = heads.partition_point(|&head| head <= start);
        let limit = heads.get(next).copied().unwrap_or(end);
        let length = (limit - start).min(ROOT_SPAN).min(remaining);
        let Ok(offset) = libc::off_t::try_from(start) else {
            break;
        };
        let Ok(length_arg) = libc::off_t::try_from(length) else {
            break;
        };
        // SAFETY: the read-only descriptor is live and the nonempty
        // advice range fits within the validated tile and file.
        #[allow(unsafe_code)]
        unsafe {
            libc::posix_fadvise(
                file.as_raw_fd(),
                offset,
                length_arg,
                libc::POSIX_FADV_WILLNEED,
            );
        }
        remaining -= length;
    }
}

pub(super) struct Ahead {
    scan: Arc<Scan>,
    scheduled: VecDeque<u64>,
    until_refill: usize,
}

impl Ahead {
    pub(super) fn advance(&mut self) {
        self.until_refill = self.until_refill.saturating_sub(1);
    }

    pub(super) fn due(&self) -> bool {
        self.until_refill == 0
    }
}

impl Drop for Ahead {
    fn drop(&mut self) {
        self.scan.cancelled.store(true, Ordering::Relaxed);
    }
}

impl<S: Source> Features<'_, S> {
    pub(crate) fn enable_mapped_ahead(mut self) -> Self {
        if self.prune {
            // Avoid measuring tiny windows. Exact tile intersection is
            // still tested by the normal directory walker before admission.
            let max_width = self
                .bounds
                .min
                .x
                .abs_diff(self.bounds.max.x)
                .max(self.bounds.min.y.abs_diff(self.bounds.max.y));
            if max_width >= (u32::MAX >> self.library.zooms.last().copied().unwrap_or_default()) {
                self.read_ahead = self
                    .library
                    .source
                    .mapped_file()
                    .map(crate::source::MappedScan::bounded);
            }
        }
        if let Some(scan) = self.read_ahead.as_mut() {
            scan.enable_helpers();
        }
        self
    }

    #[inline]
    pub(super) fn maybe_start_mapped_ahead(&mut self) {
        if self.steps != 0
            && self
                .read_ahead
                .as_mut()
                .is_some_and(|scan| scan.admit_helpers())
        {
            self.start_mapped_ahead();
        }
    }

    #[cold]
    fn start_mapped_ahead(&mut self) {
        // A configured resource ceiling can intentionally stop demand after a
        // short prefix. Keep speculative work off those readers.
        let limits = self.library.limits;
        let defaults = crate::Limits::default();
        if limits.max_traversal_steps < defaults.max_traversal_steps
            || limits.max_tree_depth < defaults.max_tree_depth
            || limits.max_tags < defaults.max_tags
        {
            return;
        }
        let tiles = self.upcoming_tiles();
        if tiles.len() < MINIMUM_TILES {
            return;
        }
        let Some(library) = self
            .library
            .source
            .mapped_file()
            .and_then(MappedFile::worker_library)
        else {
            return;
        };
        self.mapped_ahead = Some(Box::new(Ahead {
            scan: Arc::new(Scan {
                library,
                bounds: self.prune.then_some(self.bounds),
                cancelled: AtomicBool::new(false),
                remaining_steps: AtomicU64::new(
                    limits.max_traversal_steps.saturating_sub(self.steps),
                ),
            }),
            scheduled: VecDeque::with_capacity(WINDOW * 2),
            until_refill: 0,
        }));
        self.submit_mapped_ahead(tiles);
    }

    pub(super) fn refill_mapped_ahead(&mut self) {
        let tiles = self.upcoming_tiles();
        self.submit_mapped_ahead(tiles);
    }

    fn upcoming_tiles(&self) -> Vec<(u64, Bounds)> {
        let mut directory = Self::new(self.library, self.prune.then_some(self.bounds));
        directory.read_ahead = None;
        directory.work = self
            .work
            .iter()
            .copied()
            .filter(|work| matches!(work, Work::Tile(_) | Work::Children { .. }))
            .collect();
        let mut tiles = Vec::with_capacity(WINDOW);
        for _ in 0..DIRECTORY_LOOKAHEAD_STEPS {
            if tiles.len() == WINDOW {
                break;
            }
            match directory.work.pop() {
                Some(Work::Tile(tile)) => match directory.index_tile(tile) {
                    Ok(Some(payload)) => tiles.push(payload),
                    Ok(None) => {}
                    // Speculation never consumes an error ahead of demand.
                    Err(_) => break,
                },
                Some(Work::Children {
                    parent,
                    mask,
                    entries,
                    extent,
                    ordinal,
                }) => {
                    directory.push_child(parent, mask, entries, extent, ordinal);
                }
                _ => break,
            }
        }
        tiles
    }

    fn submit_mapped_ahead(&mut self, tiles: Vec<(u64, Bounds)>) {
        let Some(ahead) = self.mapped_ahead.as_mut() else {
            return;
        };
        ahead.until_refill = if tiles.len() < MINIMUM_TILES {
            usize::MAX
        } else {
            tiles.len() / 2
        };
        if ahead.until_refill == usize::MAX {
            return;
        }
        let Some(sender) = pool() else {
            ahead.until_refill = usize::MAX;
            return;
        };
        for (tile, bounds) in tiles {
            if ahead.scan.remaining_steps.load(Ordering::Relaxed) == 0 {
                break;
            }
            if ahead.scheduled.contains(&tile) {
                continue;
            }
            let Some(permit) = Permit::acquire() else {
                break;
            };
            let job = Job {
                scan: Arc::downgrade(&ahead.scan),
                tile,
                bounds,
                types: self.types,
                required: self.required_keys,
                alternative: self.alternative_keys,
                _permit: permit,
            };
            if sender.try_send(job).is_err() {
                break;
            }
            if ahead.scheduled.len() == WINDOW * 2 {
                ahead.scheduled.pop_front();
            }
            ahead.scheduled.push_back(tile);
        }
    }
}

#[cfg(feature = "parallel")]
impl<S: Source> Features<'_, S> {
    pub(crate) fn partition_fault_sample(&self) -> Option<crate::source::FaultSample> {
        if self.prune || self.library.source.mapped_file().is_none() {
            return None;
        }
        crate::source::FaultSample::now()
    }

    /// Split partitions may start partway through a tile. Only hint whole,
    /// deferred payloads whose header the worker is about to traverse.
    pub(crate) fn hint_pending_tile(&self, live: &impl Fn() -> bool) {
        let [Work::Payload { start, .. }] = self.work.as_slice() else {
            return;
        };
        if let Some(source) = self.library.source.mapped_file() {
            hint_roots(
                source.file(),
                source.len(),
                RootHints {
                    start: *start,
                    types: self.types,
                    required: self.required_keys,
                    alternative: self.alternative_keys,
                },
                live,
            );
        }
    }
}

#[cfg(test)]
mod tests;
