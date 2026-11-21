use super::*;
#[test]
fn chunks_match_checked_tail_at_boundaries_and_overflows() {
    let data = [7u8; 40];
    let source = data.as_slice();
    for base in [0u64, 3, u64::MAX - 20] {
        let window = ReadWindow {
            source: &source,
            base,
            bytes: Cow::Borrowed(&data[..17.min((u64::MAX - base) as usize)]),
        };
        let mut positions: Vec<u64> = (0..45).map(|n| base.saturating_add(n)).collect();
        positions.extend([0, 1, base.saturating_sub(1), u64::MAX]);
        for &offset in &positions {
            for &limit in &positions {
                macro_rules! check {
                    ($n:expr) => {
                        assert_eq!(
                            window.record::<$n, true>(offset, limit),
                            window
                                .tail(offset, limit)
                                .and_then(|b| b.split_first_chunk::<$n>())
                                .map(|(a, _)| a)
                        );
                    };
                }
                check!(0);
                check!(1);
                check!(4);
                check!(8);
                check!(12);
                check!(16);
                check!(32);
            }
        }
    }
}
