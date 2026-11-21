use crate::{
    Error, Result,
    decode::{Decode, ReadWindow},
    source::Source,
};

pub(crate) struct Dictionary {
    text: String,
    // The trailing offset marks the end of the last spelling.
    offsets: Vec<u32>,
    codes: Vec<u16>,
    bucket_starts: [u16; 257],
}

// Narrow the search before comparing complete UTF-8 spellings. Using the ends
// and length also bounds the work for a very long query literal.
fn bucket(text: &[u8]) -> usize {
    let Some((&first, rest)) = text.split_first() else {
        return 0;
    };
    let last = rest.last().copied().unwrap_or(first);
    (text.len() as u8)
        .wrapping_mul(31)
        .wrapping_add(first.wrapping_mul(13))
        .wrapping_add(last) as usize
}

impl Dictionary {
    pub(super) fn read<S: Source>(source: &S, mut p: u64, count: u16) -> Result<Self> {
        let mut offsets = Vec::with_capacity(usize::from(count) + 1);
        let mut packed = Vec::new();
        let mut buckets = Vec::with_capacity(usize::from(count));
        let mut bucket_starts = [0u16; 257];
        let origin = p;
        let mut input = ReadWindow::new(source);
        while offsets.len() < usize::from(count) {
            input.seek(p)?;
            let mut remaining = usize::from(count) - offsets.len();
            // Validate the packed buffer once. Reject a continuation at the
            // start of a spelling so two invalid neighbours cannot combine.
            let mut append = |spelling: &[u8], start| {
                if matches!(spelling.first(), Some(0x80..=0xbf)) {
                    return Err(Error::invalid(start, "invalid UTF-8"));
                }
                offsets.push(packed.len() as u32);
                let b = bucket(spelling);
                buckets.push(b as u8);
                bucket_starts[b] += 1;
                packed.extend_from_slice(spelling);
                Ok(())
            };
            let mut tail = input.tail(p, source.len()).unwrap_or_default();
            let available = tail.len();
            // Reuse the suffix across adjacent short spellings. The checked
            // path handles a longer length field or a spelling across pages.
            while remaining != 0 {
                let Some((&n, rest)) = tail.split_first() else {
                    break;
                };
                if n >= 128 || rest.len() < usize::from(n) {
                    break;
                }
                let (spelling, next) = rest.split_at(usize::from(n));
                append(spelling, p + (available - rest.len()) as u64)?;
                tail = next;
                remaining -= 1;
            }
            p += (available - tail.len()) as u64;
            // Refresh the window if the short spellings consumed the segment.
            if tail.is_empty() && available != 0 {
                continue;
            }
            if remaining == 0 {
                continue;
            }
            let len = input.varint(&mut p)?;
            if len > 32767 {
                return Err(Error::invalid(p, "global string exceeds format limit"));
            }
            let spelling = input.text(p, len as usize)?;
            append(spelling.as_bytes(), p)?;
            p += len;
        }
        // At most u16::MAX entries of at most 32,767 bytes each.
        offsets.push(packed.len() as u32);
        if offsets.get(1) != Some(&0) {
            return Err(Error::invalid(origin, "missing empty global string"));
        }
        let text =
            String::from_utf8(packed).map_err(|_| Error::invalid(origin, "invalid UTF-8"))?;

        let mut end = 0;
        for size in &mut bucket_starts {
            end += *size;
            *size = end;
        }
        let mut codes = vec![0; usize::from(count)];
        // Fill backwards so codes within each bucket retain file order. This
        // keeps the first eligible key when multiple codes have the same text.
        for (code, b) in buckets.into_iter().enumerate().rev() {
            let start = &mut bucket_starts[usize::from(b)];
            *start -= 1;
            codes[usize::from(*start)] = code as u16;
        }
        Ok(Self {
            text,
            offsets,
            codes,
            bucket_starts,
        })
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.codes.len()
    }

    #[inline]
    pub(crate) fn get(&self, code: u16) -> Option<&str> {
        let &[start, end] = self.offsets.get(usize::from(code)..)?.first_chunk::<2>()?;
        Some(&self.text[start as usize..end as usize])
    }

    pub(crate) fn key_code(&self, key: &str) -> Option<u16> {
        self.exact_codes(key, 1..self.len().min(8191))
            .next()
            .map(|code| code as u16)
    }
    pub(crate) fn has_global_value(&self, text: &str) -> bool {
        // GOQL compiles a positive dictionary code to an integer comparison.
        // Code zero represents the empty string and is not promoted.
        self.exact_codes(text, 1..self.len()).next().is_some()
    }

    pub(crate) fn exact_codes<'a>(
        &'a self,
        text: &'a str,
        range: std::ops::Range<usize>,
    ) -> impl Iterator<Item = usize> + 'a {
        let b = bucket(text.as_bytes());
        let start = usize::from(self.bucket_starts[b]);
        let end = usize::from(self.bucket_starts[b + 1]);
        self.codes[start..end]
            .iter()
            .copied()
            .map(usize::from)
            .take_while(move |&code| code < range.end)
            .filter(move |&code| code >= range.start && self.get(code as u16) == Some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_keeps_code_order_across_duplicates_and_byte_collisions() -> Result<()> {
        let words = ["", "aéz", "aøz", "aéz", "", "aøz"];
        let mut bytes = (words.len() as u16).to_le_bytes().to_vec();
        for word in words {
            bytes.push(word.len() as u8);
            bytes.extend_from_slice(word.as_bytes());
        }
        let dictionary = Dictionary::read(&bytes, 2, bytes.u16(0)?)?;
        for (i, word) in words.into_iter().enumerate() {
            assert_eq!(dictionary.get(i as u16), Some(word));
            for end in i..=words.len() {
                assert!(
                    dictionary
                        .exact_codes(word, i..end)
                        .eq((i..end).filter(|&n| words[n] == word))
                );
            }
        }
        assert_eq!(dictionary.get(words.len() as u16), None);
        Ok(())
    }

    #[test]
    fn adjacent_invalid_spellings_cannot_form_a_valid_utf8_character() {
        let malformed: &[u8] = &[0, 1, 0xc3, 1, 0xa9];
        assert!(Dictionary::read(&malformed, 0, 3).is_err());
    }

    #[test]
    fn short_spelling_batches_resume_at_segment_boundaries() -> Result<()> {
        struct Segmented(Vec<u8>);
        impl Source for Segmented {
            fn len(&self) -> u64 {
                self.0.len() as u64
            }
            fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> std::io::Result<()> {
                self.0.read_exact_at(offset, out)
            }
            fn segment(&self, offset: u64) -> std::io::Result<Option<(u64, &[u8])>> {
                let base = offset as usize / 4 * 4;
                let end = (base + 4).min(self.0.len());
                Ok(self.0.get(base..end).map(|bytes| (base as u64, bytes)))
            }
        }
        let long = "b".repeat(128);
        let mut bytes = vec![0, 1, b'a', 0x80, 1];
        bytes.extend_from_slice(long.as_bytes());
        bytes.extend_from_slice(&[2, b'c', b'd', 1, b'e']);
        let dict = Dictionary::read(&Segmented(bytes), 0, 5)?;
        assert_eq!(dict.get(0), Some(""));
        assert_eq!(dict.get(1), Some("a"));
        assert_eq!(dict.get(2), Some(long.as_str()));
        assert_eq!(dict.get(3), Some("cd"));
        assert_eq!(dict.get(4), Some("e"));
        Ok(())
    }

    #[test]
    fn on_demand_counts_skip_spelling_validation_until_it_is_needed() -> Result<()> {
        use crate::{DictionaryLoading, Library, Limits, Query};
        let good = include_bytes!("../../tests/fixtures/basic.gol");
        let mut bytes = good.to_vec();
        let offset = u64::from(bytes.u32(84)?);
        // The first spelling is empty. Corrupt the first byte of the second one.
        bytes[offset as usize + 4] = 0xff;
        assert!(Library::new(bytes.as_slice()).is_err());
        let lib = Library::with_dictionary_loading(
            bytes.as_slice(),
            Limits::default(),
            DictionaryLoading::OnDemand,
        )?;
        let expected = Library::new(good.as_slice())?;
        let all = Query::parse("*")?;
        assert_eq!(
            all.prepare(&lib)?.count()?,
            all.prepare(&expected)?.count()?
        );
        assert!(lib.strings.get().is_none());
        assert!(lib.global_string(1).is_err());
        assert!(Query::parse("n[name]")?.prepare(&lib).is_err());
        assert!(lib.features().next().unwrap()?.tags()?.any(|t| t.is_err()));
        Ok(())
    }

    #[test]
    fn concurrent_first_access_shares_one_successful_dictionary_after_an_io_error() -> Result<()> {
        use crate::{DictionaryLoading, Library, Limits};
        use std::{
            io,
            sync::atomic::{AtomicUsize, Ordering},
        };

        struct FailOnce {
            bytes: &'static [u8],
            first_spelling: u64,
            attempts: AtomicUsize,
        }
        impl Source for FailOnce {
            fn len(&self) -> u64 {
                self.bytes.len() as u64
            }
            fn read_exact_at(&self, offset: u64, out: &mut [u8]) -> io::Result<()> {
                if offset == self.first_spelling
                    && self.attempts.fetch_add(1, Ordering::SeqCst) == 0
                {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.bytes.read_exact_at(offset, out)
            }
        }
        let bytes = include_bytes!("../../tests/fixtures/basic.gol").as_slice();
        let source = FailOnce {
            bytes,
            first_spelling: u64::from(bytes.u32(84)?) + 2,
            attempts: AtomicUsize::new(0),
        };
        let lib = Library::with_dictionary_loading(
            source,
            Limits::default(),
            DictionaryLoading::OnDemand,
        )?;
        assert!(matches!(lib.global_string(0), Err(Error::Io(_))));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| assert_eq!(lib.global_string(0).unwrap(), ""));
            }
        });
        assert_eq!(lib.source().attempts.load(Ordering::SeqCst), 2);
        Ok(())
    }

    #[test]
    fn largest_dictionary_fits_in_one_bucket() -> Result<()> {
        let count = u16::MAX;
        let mut bytes = count.to_le_bytes().to_vec();
        bytes.resize(2 + usize::from(count), 0);
        let dictionary = Dictionary::read(&bytes, 2, bytes.u16(0)?)?;
        assert_eq!(dictionary.get(count - 1), Some(""));
        assert_eq!(dictionary.get(count), None);
        assert!(
            dictionary
                .exact_codes("", 0..usize::from(count))
                .eq(0..usize::from(count))
        );
        Ok(())
    }
}
