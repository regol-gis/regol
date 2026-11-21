use super::{CompiledPlan, CompiledPredicate, PreparedQuery};
use crate::{Bounds, Feature, Result, decode::ReadWindow, source::Source};

impl<'a, S: Source> PreparedQuery<'a, '_, S> {
    pub(crate) fn execute_sidecar_fallback(
        &self,
        bounds: Option<Bounds>,
    ) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, S>>> + '_ {
        self.execute(bounds)
    }

    pub(crate) fn matches_sidecar_candidate(
        &self,
        feature: Feature<'a, S>,
        tags: &mut ReadWindow<'a, S>,
        strings: &mut ReadWindow<'a, S>,
    ) -> Result<bool> {
        let mut matches =
            |first: &CompiledPredicate<'_>, rest: &[CompiledPredicate<'_>]| -> Result<bool> {
                if !first.matches(feature, tags, strings)? {
                    return Ok(false);
                }
                for predicate in rest {
                    if !predicate.matches(feature, tags, strings)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            };
        match &self.plan {
            CompiledPlan::All(types) => Ok(types.matches(feature)),
            CompiledPlan::One(types, first)
            | CompiledPlan::ProbedText(types, first, ..)
            | CompiledPlan::ProbedTruthy(types, first, _)
            | CompiledPlan::ProbedOne(types, first, _)
            | CompiledPlan::LocalOne(types, first) => {
                Ok(types.matches(feature) && matches(first, &[])?)
            }
            CompiledPlan::Equal(types, first, _, rest)
            | CompiledPlan::ValueSet(types, first, _, rest) => {
                Ok(types.matches(feature) && matches(first, rest)?)
            }
            CompiledPlan::Two(types, [first, second], _) => {
                Ok(types.matches(feature) && matches(first, std::slice::from_ref(second))?)
            }
            CompiledPlan::DictionaryUnion(types, predicates, _) => {
                if types.matches(feature) {
                    for predicate in predicates {
                        if matches(predicate, &[])? {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
            CompiledPlan::Any(selectors) => {
                for selector in selectors {
                    if !selector.types.matches(feature) {
                        continue;
                    }
                    let Some((first, rest)) = selector.predicates.split_first() else {
                        return Ok(true);
                    };
                    if matches(first, rest)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }
}
