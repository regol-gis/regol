use super::{invalid, u32_at};
use crate::Result;

pub(super) enum Cursor<'a> {
    Empty,
    Exact { leaf: u64, list: &'a [u8] },
    Disjunction { leaf: u64, lists: Vec<&'a [u8]> },
    Words(WordCandidates<'a>),
}

impl<'a> Cursor<'a> {
    pub(super) fn exact(
        mut rest: &'a [u8],
        leaf: u64,
        mut present: u32,
        wanted: u32,
    ) -> Result<Self> {
        let wanted = present & wanted;
        let mut single = None;
        let mut lists = Vec::new();
        while present != 0 {
            let bit = 1u32 << present.trailing_zeros();
            present ^= bit;
            let (count, tail) = rest
                .split_first_chunk::<4>()
                .ok_or_else(|| invalid("short posting length"))?;
            let bytes = (u32::from_le_bytes(*count) as usize)
                .checked_mul(4)
                .ok_or_else(|| invalid("tag posting too large"))?;
            let (list, tail) = tail
                .split_at_checked(bytes)
                .ok_or_else(|| invalid("short posting list"))?;
            rest = tail;
            if wanted & bit != 0 && !list.is_empty() {
                if wanted == bit {
                    return Ok(Self::Exact { leaf, list });
                }
                if let Some(previous) = single {
                    lists.push(previous);
                }
                single = Some(list);
            }
        }
        match single {
            None => Ok(Self::Empty),
            Some(list) if lists.is_empty() => Ok(Self::Exact { leaf, list }),
            Some(list) => {
                lists.push(list);
                Ok(Self::Disjunction { leaf, lists })
            }
        }
    }

    pub(super) fn words(
        payload: &'a [u8],
        leaf: u64,
        node: bool,
        mask: u32,
        count: u32,
        probe: Probe,
    ) -> Result<Self> {
        let bytes = (count as usize)
            .checked_mul(4)
            .ok_or_else(|| invalid("invalid word count"))?;
        let (words, widths) = payload
            .split_at_checked(bytes)
            .ok_or_else(|| invalid("short filter words"))?;
        if !widths.is_empty() && (!node || widths.len() != count.div_ceil(32) as usize * 4) {
            return Err(invalid("invalid filter-word node widths"));
        }
        Ok(Self::Words(WordCandidates {
            words,
            widths,
            mask,
            base: leaf,
            width: if node { 20 } else { 32 },
            pending: 0,
            group_wide: 0,
            group_base: 0,
            probe,
        }))
    }

    #[inline]
    pub(super) fn next_position(&mut self) -> Option<u64> {
        match self {
            Self::Empty => None,
            Self::Exact { leaf, list } => {
                let (offset, rest) = list.split_first_chunk::<4>()?;
                *list = rest;
                Some(*leaf + u64::from(u32::from_le_bytes(*offset)))
            }
            Self::Disjunction { leaf, lists } => {
                let first = lists
                    .iter()
                    .filter(|list| !list.is_empty())
                    .map(|list| u32_at(list))
                    .min()?;
                for list in lists {
                    if !list.is_empty() && u32_at(list) == first {
                        *list = &list[4..];
                    }
                }
                Some(*leaf + u64::from(first))
            }
            Self::Words(words) => words.next_position(),
        }
    }
}

pub(super) type Probe = fn(&[u8; 128], u32) -> u32;

pub(super) struct WordCandidates<'a> {
    words: &'a [u8],
    widths: &'a [u8],
    mask: u32,
    base: u64,
    width: u64,
    pending: u32,
    group_wide: u32,
    group_base: u64,
    probe: Probe,
}

impl WordCandidates<'_> {
    fn next_position(&mut self) -> Option<u64> {
        while self.pending == 0 {
            if self.words.is_empty() {
                return None;
            }
            let (pending, words, lanes) =
                if let Some((group, rest)) = self.words.split_first_chunk::<128>() {
                    ((self.probe)(group, self.mask), rest, 32u64)
                } else {
                    (
                        scalar(self.words, self.mask),
                        &[][..],
                        self.words.len() as u64 / 4,
                    )
                };
            self.words = words;
            self.pending = pending;
            self.group_wide = if let Some((bits, rest)) = self.widths.split_first_chunk::<4>() {
                self.widths = rest;
                u32::from_le_bytes(*bits)
            } else {
                0
            };
            self.group_base = self.base;
            self.base += lanes * self.width + 4 * u64::from(self.group_wide.count_ones());
        }
        let lane = self.pending.trailing_zeros();
        self.pending &= self.pending - 1;
        let before = (1u32 << lane) - 1;
        Some(
            self.group_base
                + u64::from(lane) * self.width
                + 4 * u64::from((self.group_wide & before).count_ones()),
        )
    }
}

fn scalar(bytes: &[u8], mask: u32) -> u32 {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .fold(0, |bits, (lane, word)| {
            bits | (u32::from(u32_at(word) & mask != 0) << lane)
        })
}

pub(super) fn scalar_probe(bytes: &[u8; 128], mask: u32) -> u32 {
    scalar(bytes, mask)
}

pub(super) fn probe_for_host() -> Probe {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        return avx2::probe;
    }
    scalar_probe
}

#[cfg(target_arch = "x86_64")]
#[allow(unsafe_code)]
mod avx2 {
    use std::arch::x86_64::*;

    pub(super) fn probe(bytes: &[u8; 128], mask: u32) -> u32 {
        // SAFETY: probe_for_host returns this function only when AVX2 is available.
        unsafe { filter(bytes, mask) }
    }

    #[target_feature(enable = "avx2")]
    unsafe fn filter(bytes: &[u8; 128], mask: u32) -> u32 {
        // SAFETY: The caller checked AVX2 support. Each exact 32-byte chunk
        // contains all bytes read by the unaligned vector load.
        unsafe {
            let mask = _mm256_set1_epi32(mask as i32);
            let zero = _mm256_setzero_si256();
            let mut lanes = 0;
            for (group, words) in bytes.as_chunks::<32>().0.iter().enumerate() {
                let input = _mm256_loadu_si256(words.as_ptr().cast());
                let empty = _mm256_cmpeq_epi32(_mm256_and_si256(input, mask), zero);
                let bits = _mm256_movemask_ps(_mm256_castsi256_ps(empty)) as u32;
                lanes |= (bits ^ 255) << (group * 8);
            }
            lanes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_probe_and_node_widths_keep_exact_lane_positions() {
        let mut payload = Vec::new();
        let mut position = 100u64;
        let mut expected = Vec::new();
        let mut widths = [0u32; 3];
        for lane in 0..77 {
            let value = (lane * 2654435761u32 as u64) as u32;
            let wide = lane % 7 == 3;
            payload.extend_from_slice(&value.to_le_bytes());
            if wide {
                widths[lane as usize / 32] |= 1 << (lane % 32);
            }
            if value & 19 != 0 {
                expected.push(position);
            }
            position += if wide { 24 } else { 20 };
        }
        for mask in widths {
            payload.extend_from_slice(&mask.to_le_bytes());
        }
        let mut cursor = Cursor::words(&payload, 100, true, 19, 77, probe_for_host()).unwrap();
        assert_eq!(
            std::iter::from_fn(|| cursor.next_position()).collect::<Vec<_>>(),
            expected
        );
    }
}
