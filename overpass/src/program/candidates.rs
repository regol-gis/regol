//! Evaluate independent candidates on native workers, then commit them in scan order.
use super::outputs::FeaturePreparation;
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

pub(crate) struct Prepared<P> {
    pub(super) matched: regol::Result<bool>,
    pub(super) output: Option<P>,
}

pub(crate) trait Candidates<S: Source, C> {
    fn visit<'a, P: FeaturePreparation<'a, S>>(
        &self,
        iter: impl Iterator<Item = regol::Result<Feature<'a, S>>>,
        _: &[Matcher],
        _: Option<&P>,
        _: &C,
        mut visit: impl FnMut(
            regol::Result<Feature<'a, S>>,
            Option<Prepared<P::Prepared>>,
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
    fn visit<'a, P: FeaturePreparation<'a, S>>(
        &self,
        mut iter: impl Iterator<Item = regol::Result<Feature<'a, S>>>,
        filters: &[Matcher],
        preparation: Option<&P>,
        check: &C,
        mut visit: impl FnMut(
            regol::Result<Feature<'a, S>>,
            Option<Prepared<P::Prepared>>,
        ) -> Result<ControlFlow<()>, String>,
    ) -> Result<(), String>
    where
        S: 'a,
    {
        use rayon::prelude::*;
        const CHUNK: usize = 16;
        const SPECULATIVE_BYTES: usize = 8 * 1024 * 1024;
        let threads = rayon::current_num_threads();
        if (filters.is_empty() && preparation.is_none()) || threads == 1 {
            return Serial.visit(iter, filters, preparation, check, visit);
        }
        let batch_size = 1024.max(threads * CHUNK);
        let chunk_bytes = SPECULATIVE_BYTES / (batch_size / CHUNK);
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
                        let mut bytes = 0usize;
                        for (item, prepared) in chunk {
                            check()?;
                            let Ok(feature) = item else { continue };
                            let mut geometry = None;
                            let matched = matches(*feature, filters, &mut geometry, check);
                            let mut output = None;
                            if let Some(preparation) = preparation
                                && matches!(matched, Ok(true))
                            {
                                let (value, cost) = preparation.prepare(*feature, geometry);
                                bytes = bytes.saturating_add(cost);
                                if bytes > chunk_bytes {
                                    *prepared = Some(Prepared {
                                        matched,
                                        output: None,
                                    });
                                    break;
                                }
                                output = Some(value);
                            }
                            *prepared = Some(Prepared { matched, output });
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
