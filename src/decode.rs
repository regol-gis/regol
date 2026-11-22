use crate::{Error, Result, source::Source};
use std::borrow::Cow;

pub(crate) fn add(base: u64, delta: i64) -> Result<u64> {
    base.checked_add_signed(delta)
        .ok_or_else(|| Error::invalid(base, "offset overflow"))
}

#[inline]
fn varint_from(start: u64, mut byte: impl FnMut(usize) -> Result<u8>) -> Result<u64> {
    let mut n = 0;
    for index in 0..10 {
        let byte = byte(index)?;
        if index == 9 && byte > 1 {
            return Err(Error::invalid(start, "varint overflow"));
        }
        n |= u64::from(byte & 127) << (index * 7);
        if byte < 128 {
            return Ok(n);
        }
    }
    Err(Error::invalid(start, "unterminated varint"))
}

pub(crate) trait Decode: Source {
    fn check(&self, offset: u64, len: u64) -> Result<()> {
        if offset.checked_add(len).is_none_or(|end| end > self.len()) {
            return Err(Error::invalid(offset, "read outside file"));
        }
        Ok(())
    }
    fn array<const N: usize>(&self, offset: u64) -> Result<[u8; N]> {
        if let Some(bytes) = self.view(offset, N)? {
            let data = bytes
                .get(..N)
                .ok_or_else(|| Error::invalid(offset, "short source view"))?;
            let mut out = [0; N];
            out.copy_from_slice(data);
            Ok(out)
        } else {
            self.check(offset, N as u64)?;
            let mut out = [0; N];
            self.read_exact_at(offset, &mut out)?;
            Ok(out)
        }
    }
    fn u8(&self, p: u64) -> Result<u8> {
        Ok(self.array::<1>(p)?[0])
    }
    fn u16(&self, p: u64) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array(p)?))
    }
    fn u32(&self, p: u64) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array(p)?))
    }
    fn i32(&self, p: u64) -> Result<i32> {
        Ok(i32::from_le_bytes(self.array(p)?))
    }
    fn relative(&self, p: u64, mask: i32) -> Result<u64> {
        add(p, (self.i32(p)? & mask) as i64)
    }
    fn varint(&self, p: &mut u64) -> Result<u64> {
        varint_from(*p, |_| {
            let byte = self.u8(*p)?;
            *p += 1;
            Ok(byte)
        })
    }
    fn signed_varint(&self, p: &mut u64) -> Result<i64> {
        let n = self.varint(p)?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }
    fn text(&self, offset: u64, len: usize) -> Result<Cow<'_, str>> {
        self.check(offset, len as u64)?;
        if let Some(bytes) = self.view(offset, len)? {
            let bytes = bytes
                .get(..len)
                .ok_or_else(|| Error::invalid(offset, "short source view"))?;
            Ok(Cow::Borrowed(
                std::str::from_utf8(bytes).map_err(|_| Error::invalid(offset, "invalid UTF-8"))?,
            ))
        } else {
            let mut bytes = vec![0; len];
            self.read_exact_at(offset, &mut bytes)?;
            Ok(Cow::Owned(
                String::from_utf8(bytes).map_err(|_| Error::invalid(offset, "invalid UTF-8"))?,
            ))
        }
    }
    fn next_string(&self, p: &mut u64) -> Result<Cow<'_, str>> {
        let offset = *p;
        let len = self.varint(p)?;
        if len > 32767 {
            return Err(Error::invalid(offset, "string exceeds format limit"));
        }
        let value = self.text(*p, len as usize)?;
        *p += len;
        Ok(value)
    }
}
impl<S: Source> Decode for S {}

/// Checked segment for clustered reads. Stable source segments are borrowed;
/// sources that opt in can instead use a reusable bounded buffer. Crossing a
/// window boundary falls back to the original source.
pub(crate) struct ReadWindow<'a, S> {
    source: &'a S,
    base: u64,
    bytes: Cow<'a, [u8]>,
}
impl<'a, S: Source> ReadWindow<'a, S> {
    pub(crate) fn new(source: &'a S) -> Self {
        Self {
            source,
            base: 0,
            bytes: Cow::Borrowed(&[]),
        }
    }
    #[cfg(feature = "parallel")]
    pub(crate) fn clear(&mut self) {
        self.base = 0;
        match &mut self.bytes {
            Cow::Borrowed(bytes) => *bytes = &[],
            Cow::Owned(bytes) => bytes.clear(),
        }
    }
    /// Retain a borrowed segment across nearby records, refreshing only when
    /// the next record starts outside it. All actual reads still check ranges.
    #[inline]
    pub(crate) fn seek(&mut self, offset: u64) -> Result<()> {
        if offset
            .checked_sub(self.base)
            .is_some_and(|n| n < self.bytes.len() as u64)
        {
            return Ok(());
        }
        self.refresh(offset)
    }
    // Keep backend I/O and its error conversion outside the common window hit.
    // Otherwise their size can prevent seek's small range check from inlining.
    #[inline(never)]
    fn refresh(&mut self, offset: u64) -> Result<()> {
        if let Some((base, bytes)) = self.source.segment(offset)? {
            self.base = base;
            self.bytes = Cow::Borrowed(bytes);
            return Ok(());
        }
        self.fill_buffer(offset)
    }
    #[inline]
    fn fill_buffer(&mut self, offset: u64) -> Result<()> {
        let width = self.source.read_buffer_size().min(64 * 1024);
        // Detach the old contents before any fill can fail or panic. An empty
        // window cannot expose partial data and still permits checked fallback.
        let previous = std::mem::replace(&mut self.bytes, Cow::Borrowed(&[]));
        self.base = 0;
        if width == 0 {
            return Ok(());
        }
        if offset >= self.source.len() {
            return Err(Error::invalid(offset, "read outside file"));
        }
        let base = offset / width as u64 * width as u64;
        let len = (self.source.len() - base).min(width as u64) as usize;
        let mut buffer = match previous {
            Cow::Owned(buffer) => buffer,
            Cow::Borrowed(_) => Vec::new(),
        };
        if len > buffer.len() {
            buffer
                .try_reserve_exact(len - buffer.len())
                .map_err(std::io::Error::other)?;
        }
        buffer.resize(len, 0);
        self.source.read_exact_at(base, &mut buffer)?;
        self.base = base;
        self.bytes = Cow::Owned(buffer);
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn at(source: &'a S, offset: u64) -> Result<Self> {
        let mut window = Self::new(source);
        window.refresh(offset)?;
        Ok(window)
    }
    /// Borrow an exact record, retaining window, tile and overflow checks.
    #[inline]
    pub(crate) fn record<const N: usize, const EXACT: bool>(
        &self,
        offset: u64,
        limit: u64,
    ) -> Option<&[u8; N]> {
        if EXACT {
            let start = usize::try_from(offset.checked_sub(self.base)?).ok()?;
            let end = start.checked_add(N)?;
            if offset.checked_add(N as u64)? > limit {
                return None;
            }
            self.bytes.get(start..end)?.try_into().ok()
        } else {
            self.tail(offset, limit)?
                .split_first_chunk::<N>()
                .map(|(record, _)| record)
        }
    }
    /// The remaining segment, also bounded by the enclosing format record.
    #[inline]
    pub(crate) fn tail(&self, offset: u64, limit: u64) -> Option<&[u8]> {
        let start = usize::try_from(offset.checked_sub(self.base)?).ok()?;
        let end = usize::try_from(limit.checked_sub(self.base)?)
            .unwrap_or(usize::MAX)
            .min(self.bytes.len());
        self.bytes.get(start..end)
    }
    /// Decode directly from a checked window when the maximum encoded width
    /// fits. Short windows use the same decoder through checked source reads.
    #[inline]
    fn varint(&self, p: &mut u64) -> Result<u64> {
        if let Some(bytes) = self
            .tail(*p, self.source.len())
            .and_then(|bytes| bytes.get(..10))
        {
            varint_from(*p, |index| {
                *p += 1;
                Ok(bytes[index])
            })
        } else {
            self.fallback_varint(p)
        }
    }
    #[inline(never)]
    fn fallback_varint(&self, p: &mut u64) -> Result<u64> {
        Decode::varint(self, p)
    }
    #[inline]
    pub(crate) fn signed_varint(&self, p: &mut u64) -> Result<i64> {
        let n = self.varint(p)?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }
}
impl<S: Source> Source for ReadWindow<'_, S> {
    #[inline]
    fn len(&self) -> u64 {
        self.source.len()
    }
    #[inline]
    fn view(&self, offset: u64, len: usize) -> std::io::Result<Option<&[u8]>> {
        if let Some(start) = offset
            .checked_sub(self.base)
            .and_then(|n| usize::try_from(n).ok())
            && let Some(slice) = start
                .checked_add(len)
                .and_then(|end| self.bytes.get(start..end))
        {
            return Ok(Some(slice));
        }
        self.source.view(offset, len)
    }
    #[inline]
    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> std::io::Result<()> {
        if let Some(bytes) = self.view(offset, output.len())? {
            output.copy_from_slice(bytes);
            Ok(())
        } else {
            self.source.read_exact_at(offset, output)
        }
    }
}

#[cfg(test)]
mod record_chunk_tests;
#[cfg(test)]
mod tests;
