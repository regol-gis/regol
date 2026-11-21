use super::{Literal, Predicate, Query, Test};
use crate::{Feature, Result, decode::ReadWindow, source::Source};

impl Query {
    pub(crate) fn sidecar_simple_equality(&self) -> bool {
        let [selector] = self.selectors.as_slice() else {
            return false;
        };
        let [predicate] = selector.predicates.as_slice() else {
            return false;
        };
        matches!(&predicate.test, Test::Equal { values, negate: false }
            if matches!(values.as_slice(), [Literal::Exact(_)]))
    }

    /// A mask of necessary leading equalities. A disjunction requires every
    /// arm to be covered. The normal predicates still run on each candidate.
    pub(crate) fn sidecar_mask(&self, pairs: &[(String, String)]) -> Option<u32> {
        let mut mask = 0;
        for selector in &self.selectors {
            let predicate = selector.predicates.first()?;
            let Test::Equal {
                values,
                negate: false,
            } = &predicate.test
            else {
                return None;
            };
            for value in values {
                let Literal::Exact(value) = value else {
                    return None;
                };
                let slot = pairs
                    .iter()
                    .position(|(key, text)| key == &predicate.key && text == value)?;
                mask |= 1 << slot;
            }
        }
        (mask != 0).then_some(mask)
    }
}

impl Predicate {
    pub(crate) fn sidecar_exact(key: &str, value: &str) -> Self {
        Self {
            key: key.into(),
            test: Test::Equal {
                values: vec![Literal::Exact(value.into())],
                negate: false,
            },
        }
    }

    pub(crate) fn sidecar_matches<S: Source>(
        &self,
        feature: Feature<'_, S>,
        tags: &mut ReadWindow<'_, S>,
        strings: &mut ReadWindow<'_, S>,
    ) -> Result<bool> {
        self.matches(feature, tags, strings)
    }
}
