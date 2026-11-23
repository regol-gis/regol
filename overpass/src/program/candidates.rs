//! Evaluate independent candidates on native workers, then commit them in scan order.
#[cfg(not(target_arch = "wasm32"))]
use super::live::MAX_DISPLAY_VERTICES;
use super::live::{Display, Rendered};
use crate::{relationship::Matcher, with_geometry_reads};
use regol::{Feature, Geometry, source::Source};
use std::ops::ControlFlow;

pub(super) fn matches<S: Source>(
    f: Feature<'_, S>,
    filters: &[Matcher],
    geometry: &mut Option<Geometry>,
    check: &impl Fn() -> Result<(), String>,
) -> regol::Result<bool> {
    with_geometry_reads(f, |f| {
        for filter in filters {
            if !filter.matches(f, geometry, check)? {
                return Ok(false);
            }
        }
        Ok(true)
    })
}

pub(crate) struct Prepared {
    pub(super) matched: regol::Result<bool>,
    pub(super) display: Option<regol::Result<Rendered>>,
}

pub(crate) trait Candidates<S: Source, C> {
    fn visit<'a>(
        &self,
        iter: impl Iterator<Item = regol::Result<Feature<'a, S>>>,
        _: &[Matcher],
        _: Option<Display>,
        _: &C,
        mut visit: impl FnMut(
            regol::Result<Feature<'a, S>>,
            Option<Prepared>,
        ) -> Result<ControlFlow<()>, String>,
    ) -> Result<(), String>
    where
        S: 'a,
    {
        for candidate in iter {
            if visit(candidate, None)?.is_break() {
                break;
            }
        }
        Ok(())
    }
}

pub(crate) struct Serial;
impl<S: Source, C> Candidates<S, C> for Serial {}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct Parallel;

#[cfg(not(target_arch = "wasm32"))]
impl<S: Source + Sync, C: Fn() -> Result<(), String> + Sync> Candidates<S, C> for Parallel {
    fn visit<'a>(
        &self,
        mut iter: impl Iterator<Item = regol::Result<Feature<'a, S>>>,
        filters: &[Matcher],
        display: Option<Display>,
        check: &C,
        mut visit: impl FnMut(
            regol::Result<Feature<'a, S>>,
            Option<Prepared>,
        ) -> Result<ControlFlow<()>, String>,
    ) -> Result<(), String>
    where
        S: 'a,
    {
        use rayon::prelude::*;
        const CHUNK: usize = 16;
        let threads = rayon::current_num_threads();
        if (filters.is_empty() && display.is_none()) || threads == 1 {
            return Serial.visit(iter, filters, display, check, visit);
        }
        let batch_size = 1024.max(threads * CHUNK);
        let chunk_bytes = 8 * 1024 * 1024 / (batch_size / CHUNK);
        let mut batch = Vec::with_capacity(batch_size);
        loop {
            check()?;
            for item in iter.by_ref().take(batch_size) {
                check()?;
                batch.push((item, None));
            }
            let count = batch.len();
            if count >= CHUNK * 2 {
                batch
                    .par_chunks_mut(CHUNK)
                    .try_for_each(|chunk| -> Result<(), String> {
                        let mut bytes = 0;
                        for (item, prepared) in chunk {
                            check()?;
                            let Ok(f) = item else { continue };
                            let mut g = None;
                            let matched = matches(*f, filters, &mut g, check);
                            let display = display
                                .filter(|_| matches!(matched, Ok(true)))
                                .map(|display| display.render(*f, g, MAX_DISPLAY_VERTICES));
                            if let Some(Ok(Rendered::Feature { json: Ok(json), .. })) = &display {
                                bytes += json.get().len();
                            }
                            // Bound speculative output across the batch to 8 MiB.
                            // Commit oversized features individually on the query thread.
                            if bytes > chunk_bytes {
                                *prepared = Some(Prepared {
                                    matched,
                                    display: None,
                                });
                                break;
                            }
                            *prepared = Some(Prepared { matched, display });
                        }
                        Ok(())
                    })?;
            }
            for (candidate, prepared) in batch.drain(..) {
                if visit(candidate, prepared)?.is_break() {
                    return Ok(());
                }
            }
            if count < batch_size {
                return Ok(());
            }
        }
    }
}
