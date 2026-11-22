use super::{EncodeResult, Scratch};
use regol::{Feature, source::Source};
use std::{
    io::Write,
    sync::{
        Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

pub(super) const CHUNK_SIZE: usize = 256;
const NUM_CHUNKS: usize = 8;
pub(super) const BATCH_SIZE: usize = NUM_CHUNKS * CHUNK_SIZE;
pub(super) const MIN_PARALLEL_SIZE: usize = 2 * CHUNK_SIZE;
const CHUNK_BYTE_LIMIT: usize = 512 * 1024;

// At most eight chunks are pending at a time, so parallel buffering retains at
// most 4 MiB of encoded bytes across batches regardless of feature size.
struct Slot<F> {
    features: Vec<F>,
    buffer: Vec<u8>,
    result: EncodeResult<usize>,
    first: bool,
}

impl<F> Slot<F> {
    fn new() -> Self {
        Self {
            features: Vec::with_capacity(CHUNK_SIZE),
            buffer: Vec::with_capacity(64 * 1024),
            result: Ok(0),
            first: false,
        }
    }
}

struct SharedState {
    epoch: usize,
    active_chunks: usize,
    spawned_count: usize,
    done: bool,
    completed_workers: usize,
}

pub(super) fn write<'a, S: Source + Sync + 'a, W: Write>(
    features: impl IntoIterator<Item = regol::Result<Feature<'a, S>>>,
    output: &mut W,
    threads: usize,
    stream: impl Fn(&mut W, Feature<'a, S>, bool, &mut Vec<u8>, &mut Scratch) -> EncodeResult<()>,
    encode: impl Fn(&mut Vec<u8>, Feature<'a, S>, bool, &mut Scratch) -> EncodeResult<()> + Sync,
) -> EncodeResult<()> {
    let mut features = features.into_iter();
    let mut first = true;
    let mut serial_buf = Vec::with_capacity(64 * 1024);
    let mut scratch = Scratch::default();

    let slots: Vec<Mutex<Slot<Feature<'a, S>>>> =
        (0..NUM_CHUNKS).map(|_| Mutex::new(Slot::new())).collect();
    let next_chunk = AtomicUsize::new(0);
    let state = (
        Mutex::new(SharedState {
            epoch: 0,
            active_chunks: 0,
            spawned_count: 0,
            done: false,
            completed_workers: 0,
        }),
        Condvar::new(),
    );

    thread::scope(|scope| {
        let mut spawned = 0usize;
        let encode_ref = &encode;
        let slots_ref = &slots;
        let next_ref = &next_chunk;
        let state_ref = &state;

        let run_worker = |worker_scratch: &mut Scratch, active_chunks: usize| {
            loop {
                let idx = next_ref.fetch_add(1, Ordering::Relaxed);
                if idx >= active_chunks {
                    break;
                }
                let mut slot = slots_ref[idx].lock().unwrap();
                let is_first = slot.first;
                let Slot {
                    features,
                    buffer,
                    result,
                    ..
                } = &mut *slot;
                buffer.clear();
                *result = encode_chunk(
                    features,
                    buffer,
                    is_first,
                    CHUNK_BYTE_LIMIT,
                    encode_ref,
                    worker_scratch,
                );
            }
        };

        let result: EncodeResult<()> = (|| loop {
            let mut total = 0usize;
            let mut active_chunks = 0usize;
            let mut query_error = None;
            for slot_mutex in slots_ref {
                let mut slot = slot_mutex.lock().unwrap();
                slot.features.clear();
                slot.first = first && active_chunks == 0;
                for feature in features.by_ref().take(CHUNK_SIZE) {
                    match feature {
                        Ok(feature) => slot.features.push(feature),
                        Err(error) => {
                            query_error = Some(error);
                            break;
                        }
                    }
                }
                if slot.features.is_empty() {
                    break;
                }
                total += slot.features.len();
                active_chunks += 1;
                if query_error.is_some() || slot.features.len() < CHUNK_SIZE {
                    break;
                }
            }

            if total >= MIN_PARALLEL_SIZE && threads > 1 {
                let want_workers = threads.min(active_chunks).saturating_sub(1);
                while spawned < want_workers {
                    let spawned_epoch = state_ref.0.lock().unwrap().epoch;
                    if thread::Builder::new()
                        .spawn_scoped(scope, move || {
                            let mut worker_scratch = Scratch::default();
                            let mut seen_epoch = spawned_epoch;
                            loop {
                                let active = {
                                    let mut guard = state_ref.0.lock().unwrap();
                                    while guard.epoch == seen_epoch && !guard.done {
                                        guard = state_ref.1.wait(guard).unwrap();
                                    }
                                    if guard.done {
                                        return;
                                    }
                                    seen_epoch = guard.epoch;
                                    guard.active_chunks
                                };
                                run_worker(&mut worker_scratch, active);
                                let mut guard = state_ref.0.lock().unwrap();
                                guard.completed_workers += 1;
                                if guard.completed_workers == guard.spawned_count {
                                    state_ref.1.notify_all();
                                }
                            }
                        })
                        .is_ok()
                    {
                        spawned += 1;
                    } else {
                        break;
                    }
                }

                if spawned > 0 {
                    next_ref.store(0, Ordering::Relaxed);
                    {
                        let mut guard = state_ref.0.lock().unwrap();
                        guard.active_chunks = active_chunks;
                        guard.spawned_count = spawned;
                        guard.completed_workers = 0;
                        guard.epoch += 1;
                        state_ref.1.notify_all();
                    }
                    // Main thread participates as worker 0.
                    run_worker(&mut scratch, active_chunks);
                    {
                        let mut guard = state_ref.0.lock().unwrap();
                        while guard.completed_workers < spawned {
                            guard = state_ref.1.wait(guard).unwrap();
                        }
                    }
                    for slot_mutex in &slots_ref[..active_chunks] {
                        let mut slot = slot_mutex.lock().unwrap();
                        if !slot.buffer.is_empty() {
                            output.write_all(&slot.buffer)?;
                            first = false;
                        }
                        let encoded = std::mem::replace(&mut slot.result, Ok(0))?;
                        for &feature in &slot.features[encoded..] {
                            stream(output, feature, first, &mut serial_buf, &mut scratch)?;
                            first = false;
                        }
                    }
                    if !serial_buf.is_empty() {
                        output.write_all(&serial_buf)?;
                        serial_buf.clear();
                    }
                } else {
                    for slot_mutex in &slots_ref[..active_chunks] {
                        let slot = slot_mutex.lock().unwrap();
                        for &feature in &slot.features {
                            stream(output, feature, first, &mut serial_buf, &mut scratch)?;
                            first = false;
                        }
                    }
                    if !serial_buf.is_empty() {
                        output.write_all(&serial_buf)?;
                        serial_buf.clear();
                    }
                }
            } else {
                for slot_mutex in &slots_ref[..active_chunks] {
                    let slot = slot_mutex.lock().unwrap();
                    for &feature in &slot.features {
                        stream(output, feature, first, &mut serial_buf, &mut scratch)?;
                        first = false;
                    }
                }
                if !serial_buf.is_empty() {
                    output.write_all(&serial_buf)?;
                    serial_buf.clear();
                }
            }

            if let Some(error) = query_error {
                return Err(error.into());
            }
            if total < BATCH_SIZE {
                return Ok(());
            }
        })();

        if spawned > 0 {
            let mut guard = state_ref.0.lock().unwrap();
            guard.done = true;
            state_ref.1.notify_all();
        }
        result
    })
}

fn encode_chunk<T: Copy>(
    items: &[T],
    buffer: &mut Vec<u8>,
    first: bool,
    limit: usize,
    encode: &impl Fn(&mut Vec<u8>, T, bool, &mut Scratch) -> EncodeResult<()>,
    scratch: &mut Scratch,
) -> EncodeResult<usize> {
    for (index, &item) in items.iter().enumerate() {
        let start = buffer.len();
        if start >= limit {
            return Ok(index);
        }
        encode(buffer, item, first && index == 0, scratch)?;
        if buffer.len() > limit {
            buffer.truncate(start);
            if buffer.capacity() > limit {
                buffer.shrink_to(limit);
            }
            return Ok(index);
        }
    }
    Ok(items.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_chunk_truncates_at_limit_and_caps_retained_capacity() {
        let mut buffer = Vec::with_capacity(64);
        let mut scratch = Scratch::default();
        let items = [40usize, 50, 200, 30];
        let encoded = encode_chunk(
            &items,
            &mut buffer,
            true,
            100,
            &|out, size, _, _| {
                out.resize(out.len() + size, b'x');
                Ok(())
            },
            &mut scratch,
        )
        .unwrap();
        assert_eq!(encoded, 2);
        assert_eq!(buffer.len(), 90);
        assert!(buffer.capacity() <= 100);

        // An oversized first item yields zero buffered items so the caller streams it.
        buffer.clear();
        let encoded = encode_chunk(
            &[256usize, 10],
            &mut buffer,
            true,
            100,
            &|out, size, _, _| {
                out.resize(out.len() + size, b'y');
                Ok(())
            },
            &mut scratch,
        )
        .unwrap();
        assert_eq!(encoded, 0);
        assert!(buffer.is_empty());
        assert!(buffer.capacity() <= 100);
    }
}
