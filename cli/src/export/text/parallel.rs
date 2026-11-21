use super::EncodeResult;
use rayon::{ThreadPoolBuilder, prelude::*};
use regol::{Feature, source::Source};
use std::io::{self, Write};

pub(super) const CHUNK_SIZE: usize = 256;
pub(super) const BATCH_SIZE: usize = 8 * CHUNK_SIZE;
pub(super) const MIN_PARALLEL_SIZE: usize = 2 * CHUNK_SIZE;
const CHUNK_BYTE_LIMIT: usize = 512 * 1024;

pub(super) fn write<'a, S: Source + Sync + 'a, W: Write>(
    features: impl IntoIterator<Item = regol::Result<Feature<'a, S>>>,
    output: &mut W,
    threads: usize,
    stream: impl Fn(&mut W, Feature<'a, S>, bool) -> EncodeResult<()>,
    encode: impl Fn(&mut ChunkBytes, Feature<'a, S>, bool) -> EncodeResult<()> + Sync,
) -> EncodeResult<()> {
    let mut features = features.into_iter();
    let mut batch = Vec::new();
    let pool = std::cell::OnceCell::new();
    let mut first = true;
    loop {
        batch.clear();
        let mut query_error = None;
        for feature in features.by_ref().take(BATCH_SIZE) {
            match feature {
                Ok(feature) => batch.push(feature),
                Err(error) => {
                    query_error = Some(error);
                    break;
                }
            }
        }
        // Build the pool on the first substantial batch. Resource-constrained
        // hosts can still finish an export on the calling thread.
        let active_pool = if batch.len() >= MIN_PARALLEL_SIZE {
            pool.get_or_init(|| {
                ThreadPoolBuilder::new()
                    .num_threads(threads.min(batch.len().div_ceil(CHUNK_SIZE)))
                    .build()
                    .ok()
            })
            .as_ref()
        } else {
            None
        };
        if let Some(pool) = active_pool {
            let chunks: Vec<_> = pool.install(|| {
                batch
                    .par_chunks(CHUNK_SIZE)
                    .enumerate()
                    .map(|(index, chunk)| encode_chunk(chunk, first && index == 0, &encode))
                    .collect()
            });
            for (chunk, (bytes, encoded)) in batch.chunks(CHUNK_SIZE).zip(chunks) {
                if !bytes.buffer.is_empty() {
                    output.write_all(&bytes.buffer)?;
                    first = false;
                }
                // Stream the rest of any chunk that reached its byte limit.
                for &feature in &chunk[encoded?..] {
                    stream(output, feature, first)?;
                    first = false;
                }
            }
        } else {
            for &feature in &batch {
                stream(output, feature, first)?;
                first = false;
            }
        }
        if let Some(error) = query_error {
            return Err(error.into());
        }
        if batch.len() < BATCH_SIZE {
            return Ok(());
        }
    }
}

fn encode_chunk<'a, S: Source>(
    features: &[Feature<'a, S>],
    first: bool,
    encode: &impl Fn(&mut ChunkBytes, Feature<'a, S>, bool) -> EncodeResult<()>,
) -> (ChunkBytes, EncodeResult<usize>) {
    let mut bytes = ChunkBytes {
        buffer: Vec::with_capacity(64 * 1024),
        overflowed: false,
    };
    for (index, &feature) in features.iter().enumerate() {
        let start = bytes.buffer.len();
        if let Err(error) = encode(&mut bytes, feature, first && index == 0) {
            if bytes.overflowed {
                bytes.buffer.truncate(start);
                return (bytes, Ok(index));
            }
            return (bytes, Err(error));
        }
    }
    (bytes, Ok(features.len()))
}

// At most eight chunks are pending at a time, so parallel buffering uses up
// to 4 MiB of encoded bytes regardless of the size of individual features.
pub(super) struct ChunkBytes {
    buffer: Vec<u8>,
    overflowed: bool,
}

impl Write for ChunkBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let len = self.buffer.len();
        let capacity = self.buffer.capacity();
        if bytes.len() > capacity - len {
            if bytes.len() > CHUNK_BYTE_LIMIT - len {
                self.overflowed = true;
                return Err(io::ErrorKind::WriteZero.into());
            }
            let capacity = (capacity * 2).max(len + bytes.len()).min(CHUNK_BYTE_LIMIT);
            self.buffer.reserve_exact(capacity - len);
        }
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_bytes_stops_at_the_limit_after_uneven_growth() {
        let mut output = ChunkBytes {
            buffer: Vec::with_capacity(64 * 1024),
            overflowed: false,
        };
        let bytes = vec![b'x'; CHUNK_BYTE_LIMIT];
        for count in [180_000, 150_000, 100_000, CHUNK_BYTE_LIMIT - 430_000] {
            output.write_all(&bytes[..count]).unwrap();
        }
        assert_eq!(output.buffer, bytes);
        assert_eq!(output.buffer.capacity(), CHUNK_BYTE_LIMIT);
        assert_eq!(
            output.write_all(b"x").unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );
        assert!(output.overflowed);
        assert_eq!(output.buffer, bytes);
    }
}
