use super::*;
use std::cell::Cell;

struct Buffered {
    bytes: Vec<u8>,
    width: usize,
    calls: Cell<usize>,
    largest: Cell<usize>,
    failure: Cell<u8>,
}
impl Source for Buffered {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }
    fn read_buffer_size(&self) -> usize {
        self.width
    }
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<()> {
        self.calls.set(self.calls.get() + 1);
        self.largest.set(self.largest.get().max(output.len()));
        if self.failure.get() != 0 {
            output.fill(255);
            assert_ne!(self.failure.get(), 2, "injected source panic");
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        self.bytes.read_exact_at(offset, output)
    }
}
fn buffered(width: usize) -> Buffered {
    Buffered {
        bytes: (0..70_013).map(|n| (n % 251) as u8).collect(),
        width,
        calls: Cell::new(0),
        largest: Cell::new(0),
        failure: Cell::new(0),
    }
}

#[test]
fn owned_windows_cover_boundaries_and_cap_buffer_size() -> Result<()> {
    for width in [1, 3, 7, 31, 4096, usize::MAX] {
        let source = buffered(width);
        let mut window = ReadWindow::new(&source);
        let width = width.min(65536);
        for offset in [0, width - 1, width, source.bytes.len() - 1] {
            window.seek(offset as u64)?;
            assert!(window.bytes.len() <= width);
            for p in [offset.saturating_sub(1), offset, offset + 1] {
                match source.bytes.get(p..p + 8) {
                    Some(expected) => {
                        assert_eq!(window.array::<8>(p as u64)?.as_slice(), expected)
                    }
                    None => assert!(window.array::<8>(p as u64).is_err()),
                }
            }
        }
        assert!(source.largest.get() <= width.max(8));
        assert!(window.seek(u64::MAX).is_err());
        assert!(window.array::<8>(u64::MAX).is_err());
        assert!(window.array::<0>(source.len())?.is_empty());
    }
    Ok(())
}

#[test]
fn owned_windows_reuse_storage_and_discard_failed_fills() -> Result<()> {
    let source = buffered(32);
    let mut window = ReadWindow::new(&source);
    window.seek(5)?;
    let allocation = window.bytes.as_ptr();
    assert_eq!(source.calls.get(), 1);
    for p in 5..24 {
        window.seek(p)?;
        assert_eq!(
            window.array::<8>(p)?,
            source.bytes[p as usize..p as usize + 8]
        );
    }
    assert_eq!(source.calls.get(), 1, "in-window reads must not repeat I/O");
    window.seek(40)?;
    assert_eq!(
        window.bytes.as_ptr(),
        allocation,
        "reuse the allocation on refill"
    );
    assert_eq!(source.calls.get(), 2);
    for failure in [1, 2] {
        source.failure.set(failure);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| window.seek(80)));
        if failure == 1 {
            assert!(result.unwrap().is_err());
        } else {
            assert!(result.is_err());
        }
        assert!(
            window.bytes.is_empty(),
            "partial/stale bytes must not remain visible"
        );
        source.failure.set(0);
        window.seek(40)?;
        assert_eq!(window.array::<8>(40)?, source.bytes[40..48]);
    }
    Ok(())
}

struct Segments<'a>(&'a [u8]);
impl Source for Segments<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<()> {
        self.0.read_exact_at(offset, output)
    }
    fn segment(&self, offset: u64) -> std::io::Result<Option<(u64, &[u8])>> {
        let base = (offset / 16 * 16) as usize;
        let bytes = self
            .0
            .get(base..(base + 16).min(self.0.len()))
            .ok_or(std::io::ErrorKind::UnexpectedEof)?;
        Ok(Some((base as u64, bytes)))
    }
}

#[test]
fn windows_and_fallback_reads_check_every_boundary() -> Result<()> {
    let data: Vec<u8> = (0..80).collect();
    let source = Segments(&data);
    for anchor in [0, 15, 16, 31, 64, 79] {
        let window = ReadWindow::at(&source, anchor)?;
        for position in 0..90 {
            let actual = window.array::<8>(position);
            let expected = data.get(position as usize..position as usize + 8);
            match expected {
                Some(expected) => assert_eq!(actual?.as_slice(), expected),
                None => assert!(actual.is_err()),
            }
        }
        assert!(window.array::<8>(u64::MAX).is_err());
        for end in 0..90 {
            if let Some(tail) = window.tail(anchor, end) {
                assert!(anchor + tail.len() as u64 <= end);
                assert_eq!(tail, &data[anchor as usize..anchor as usize + tail.len()]);
            }
        }
    }
    Ok(())
}

#[test]
fn empty_windows_use_checked_reads_without_borrowable_segments() -> Result<()> {
    struct ReadOnly(Vec<u8>);
    impl Source for ReadOnly {
        fn len(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<()> {
            self.0.read_exact_at(offset, output)
        }
    }
    let source = ReadOnly((0..80).collect());
    let mut window = ReadWindow::new(&source);
    for seek in [None, Some(0), Some(31), Some(79), Some(80)] {
        if let Some(offset) = seek {
            window.seek(offset)?;
        }
        for offset in (0..90).chain([u64::MAX]) {
            let expected = usize::try_from(offset).ok().and_then(|start| {
                start
                    .checked_add(8)
                    .and_then(|end| source.0.get(start..end))
            });
            match expected {
                Some(bytes) => assert_eq!(window.array::<8>(offset)?.as_slice(), bytes),
                None => assert!(window.array::<8>(offset).is_err()),
            }
        }
        assert!(window.array::<0>(80)?.is_empty());
        assert!(window.array::<0>(81).is_err());
    }
    Ok(())
}

#[test]
fn window_varints_cover_all_widths_boundaries_and_truncation() -> Result<()> {
    let mut values = vec![0, u64::MAX];
    for bit in 0..64 {
        let power = 1u64 << bit;
        values.extend([power - 1, power, power + 1]);
    }
    for value in values {
        let mut encoded = Vec::new();
        let mut remaining = value;
        loop {
            let byte = (remaining & 127) as u8;
            remaining >>= 7;
            encoded.push(byte | if remaining == 0 { 0 } else { 128 });
            if remaining == 0 {
                break;
            }
        }
        for offset in 0..32 {
            let mut data = vec![0; offset];
            data.extend(&encoded);
            data.extend([0; 16]);
            let source = Segments(&data);
            let window = ReadWindow::at(&source, offset as u64)?;
            let mut p = offset as u64;
            assert_eq!(window.varint(&mut p)?, value);
            assert_eq!(p, (offset + encoded.len()) as u64);
            p = offset as u64;
            let signed = if value & 1 == 0 {
                i128::from(value / 2)
            } else {
                -i128::from(value / 2) - 1
            };
            assert_eq!(i128::from(window.signed_varint(&mut p)?), signed);
            for len in 0..encoded.len() {
                let source = Segments(&data[..offset + len]);
                let window = ReadWindow::at(&source, offset as u64)?;
                let mut p = offset as u64;
                assert!(window.varint(&mut p).is_err());
                assert_eq!(p, (offset + len) as u64);
            }
        }
    }
    for last in [2, 127, 128, 255] {
        for offset in 0..32 {
            let mut data = vec![0; offset];
            data.extend([255; 9]);
            data.push(last);
            data.extend([0; 16]);
            let source = Segments(&data);
            let window = ReadWindow::at(&source, offset as u64)?;
            let mut p = offset as u64;
            let error = window.varint(&mut p).unwrap_err();
            assert!(error.to_string().contains("varint overflow"));
            assert_eq!(p, (offset + 10) as u64);
        }
    }
    Ok(())
}

#[cfg(feature = "parallel")]
#[test]
fn clearing_reused_window_keeps_capacity_but_requires_a_fresh_read() -> Result<()> {
    let source = buffered(32);
    let mut window = ReadWindow::new(&source);
    window.seek(5)?;
    let allocation = window.bytes.as_ptr();

    window.clear();
    assert!(window.bytes.is_empty());
    window.seek(5)?;
    assert_eq!(window.bytes.as_ptr(), allocation);
    assert_eq!(source.calls.get(), 2);

    window.clear();
    source.failure.set(1);
    assert!(window.seek(5).is_err());
    assert!(window.bytes.is_empty());
    assert_eq!(source.calls.get(), 3);
    Ok(())
}
