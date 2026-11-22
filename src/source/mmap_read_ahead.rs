use super::MappedFile;
use crate::Library;
use memmap2::Advice;
use std::{
    sync::{
        Arc, OnceLock, RwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

const INITIAL_SAMPLE_STEPS: u64 = 512;
const SAMPLE_INTERVAL: u64 = 16_384;
const MINIMUM_FAULTS: u64 = 32;
const HELPER_MINIMUM_FAULTS: u64 = 2;
const MAX_BLOCKS_PER_FAULT: u64 = 16;
// A 1 MiB demand read already covers sixteen 64 KiB root hints. Let the
// kernel keep servicing those reads when each fault also stalls for milliseconds.
const BULK_BLOCKS_PER_FAULT: u64 = 2048; // Linux accounts input in 512-byte blocks.
const BULK_WAIT_US_PER_FAULT: u64 = 4_000;

#[derive(Clone, Copy)]
struct Activity {
    major_faults: u64,
    input_blocks: u64,
    wall: Instant,
    cpu: Duration,
}

impl Activity {
    /// None means there is not yet enough evidence to start or decline helpers.
    fn speculate_since(self, before: Self) -> Option<bool> {
        let faults = self.major_faults.saturating_sub(before.major_faults);
        if faults < HELPER_MINIMUM_FAULTS {
            return None;
        }
        let blocks = self.input_blocks.saturating_sub(before.input_blocks);
        let wait = (self.wall - before.wall).saturating_sub(self.cpu.saturating_sub(before.cpu));
        Some(
            blocks < faults.saturating_mul(BULK_BLOCKS_PER_FAULT)
                || wait.as_micros() < u128::from(faults) * u128::from(BULK_WAIT_US_PER_FAULT),
        )
    }
}

#[allow(unsafe_code)]
fn activity() -> Option<Activity> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    // SAFETY: getrusage writes one rusage to a valid writable pointer.
    if unsafe { libc::getrusage(libc::RUSAGE_THREAD, usage.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: the successful call above initialized the structure.
    let usage = unsafe { usage.assume_init() };
    let cpu_time = |t: libc::timeval| {
        Duration::from_secs(t.tv_sec as u64) + Duration::from_micros(t.tv_usec as u64)
    };
    Some(Activity {
        major_faults: u64::try_from(usage.ru_majflt).ok()?,
        input_blocks: u64::try_from(usage.ru_inblock).ok()?,
        wall: Instant::now(),
        cpu: cpu_time(usage.ru_utime) + cpu_time(usage.ru_stime),
    })
}

/// Per-thread baseline for a query prefix or a worker's tile group.
#[cfg(feature = "parallel")]
pub(crate) struct FaultSample {
    thread: ThreadId,
    start: Activity,
}

#[cfg(feature = "parallel")]
impl FaultSample {
    pub(crate) fn now() -> Option<Self> {
        Some(Self {
            thread: thread::current().id(),
            start: activity()?,
        })
    }

    pub(crate) fn speculate(&self) -> Option<bool> {
        if self.thread != thread::current().id() {
            return None;
        }
        activity()?.speculate_since(self.start)
    }
}

// The caller's immutability guarantee ends when their MappedFile drops.
// Only this owner can keep a worker mapping alive between jobs. A worker
// holds the read guard throughout its accesses and cannot return a borrow.
#[derive(Default)]
pub(super) struct HelperOwner(OnceLock<Option<Arc<WorkerMap>>>);

impl HelperOwner {
    pub(super) fn access(
        &self,
        create: impl FnOnce() -> Option<Library<MappedFile>>,
    ) -> Option<Weak<WorkerMap>> {
        self.0
            .get_or_init(|| create().map(WorkerMap::new))
            .as_ref()
            .map(Arc::downgrade)
    }
}

impl Drop for HelperOwner {
    fn drop(&mut self) {
        if let Some(Some(worker)) = self.0.get() {
            worker.closing.store(true, Ordering::Release);
            worker
                .library
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
        }
    }
}

pub(crate) struct WorkerMap {
    closing: AtomicBool,
    library: RwLock<Option<Library<MappedFile>>>,
}

impl WorkerMap {
    fn new(library: Library<MappedFile>) -> Arc<Self> {
        Arc::new(Self {
            closing: AtomicBool::new(false),
            library: RwLock::new(Some(library)),
        })
    }

    pub(crate) fn active(&self) -> bool {
        !self.closing.load(Ordering::Acquire)
    }

    pub(crate) fn with_library(&self, run: impl FnOnce(&Library<MappedFile>)) {
        if !self.active() {
            return;
        }
        if let Ok(guard) = self.library.read()
            && self.active()
            && let Some(library) = guard.as_ref()
        {
            run(library);
        }
    }
}

pub(crate) struct MappedScan<'a> {
    source: &'a MappedFile,
    allow_sequential: bool,
    next_sample: u64,
    previous: Option<(ThreadId, Activity)>,
    sequential: bool,
    helpers_considered: bool,
    helpers_enabled: bool,
    helper_faults_observed: bool,
    advised: Option<(usize, usize)>,
}

impl<'a> MappedScan<'a> {
    pub(crate) fn new(source: &'a MappedFile) -> Self {
        Self::with_policy(source, true)
    }

    pub(crate) fn bounded(source: &'a MappedFile) -> Self {
        Self::with_policy(source, false)
    }

    fn with_policy(source: &'a MappedFile, allow_sequential: bool) -> Self {
        Self {
            source,
            allow_sequential,
            next_sample: 0,
            previous: None,
            sequential: false,
            helpers_considered: false,
            helpers_enabled: false,
            helper_faults_observed: false,
            advised: None,
        }
    }

    #[inline]
    pub(crate) fn sample(&mut self, steps: u64) {
        if steps >= self.next_sample {
            self.read_sample(steps);
        }
    }

    #[cold]
    fn read_sample(&mut self, steps: u64) {
        let Some(current) = activity() else {
            self.next_sample = u64::MAX;
            return;
        };
        let thread = thread::current().id();
        let previous = self.previous.replace((thread, current));
        let Some((_, before)) = previous.filter(|(owner, _)| *owner == thread) else {
            self.next_sample = steps.saturating_add(INITIAL_SAMPLE_STEPS);
            return;
        };
        let faults = current.major_faults.saturating_sub(before.major_faults);
        let blocks = current.input_blocks.saturating_sub(before.input_blocks);
        match current.speculate_since(before) {
            Some(false) => {
                self.helpers_enabled = false;
                self.next_sample = u64::MAX;
                return;
            }
            Some(true) => self.helper_faults_observed |= self.helpers_enabled,
            None => {}
        }
        // Linux reports block inputs in 512-byte units. A cold mapped scan
        // with little kernel readahead waits for many small reads. FUSE
        // can report major faults without charging the caller any block I/O.
        self.sequential = self.allow_sequential
            && faults >= MINIMUM_FAULTS
            && blocks <= faults.saturating_mul(MAX_BLOCKS_PER_FAULT);
        self.next_sample = if self.sequential {
            u64::MAX
        } else {
            steps.saturating_add(SAMPLE_INTERVAL)
        };
    }

    pub(crate) fn enable_helpers(&mut self) {
        self.helpers_enabled = true;
    }

    #[inline]
    pub(crate) fn admit_helpers(&mut self) -> bool {
        if !self.helpers_enabled || !self.helper_faults_observed || self.helpers_considered {
            return false;
        }
        self.helpers_considered = true;
        true
    }

    #[inline]
    pub(crate) fn enter_tile(&mut self, start: u64, end: u64) {
        if !self.sequential {
            return;
        }
        // Keep the advice local to the validated tile the cursor is visiting.
        let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
            return;
        };
        if start >= end || end > self.source.map.len() {
            return;
        }
        let length = end - start;
        if self
            .source
            .map
            .advise_range(Advice::Sequential, start, length)
            .is_ok()
        {
            self.advised = Some((start, length));
        }
    }

    #[inline]
    pub(crate) fn leave_tile(&mut self) {
        if let Some((start, length)) = self.advised.take() {
            let _ = self.source.map.advise_range(Advice::Normal, start, length);
        }
    }
}

impl Drop for MappedScan<'_> {
    fn drop(&mut self) {
        self.leave_tile();
    }
}

#[cfg(test)]
pub(crate) mod test_support;
