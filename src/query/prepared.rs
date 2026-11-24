mod resumable;

use super::{Literal, LocalValueTest, Predicate, Query, Test, Types, contains_bytes_in_tail};
use crate::{
    Bounds, Feature, Features, Library, Result, TagValue,
    adaptive_text_index::{ScanKey, ScanObservation},
    decode::ReadWindow,
    feature::EncodedValue,
    source::Source,
    spatial::{
        DictionaryEquality, DictionaryPair, DictionaryProbe, DictionarySet, DictionaryTruthy,
        DictionaryUnion, EqualityProbe,
    },
    text_index::IndexedMatches,
};

impl Query {
    /// Resolve common keys and index categories once for repeated execution.
    pub fn prepare<'a, 'q, S: Source>(
        &'q self,
        library: &'a Library<S>,
    ) -> Result<PreparedQuery<'a, 'q, S>> {
        let mut selectors = Vec::with_capacity(self.selectors.len());
        let mut types = 0;
        let mut required_keys = u32::MAX;
        let mut alternatives = 0;
        let mut all_indexed = true;
        for selector in &self.selectors {
            let mut codes = Vec::with_capacity(selector.predicates.len());
            let mut mask = 0;
            types |= selector.types.0;
            for predicate in &selector.predicates {
                let dictionary = library.dictionary()?;
                let code = dictionary.key_code(&predicate.key);
                let requires_presence = matches!(
                    predicate.test,
                    Test::Present
                        | Test::Truth(true)
                        | Test::Equal { negate: false, .. }
                        | Test::Numeric { .. }
                        | Test::Regex { negate: false, .. }
                );
                if requires_presence
                    && let Some(code) = code
                    && let Some((_, bits)) = library.categories.iter().find(|(key, _)| *key == code)
                {
                    mask |= bits;
                }
                let local_value = predicate.local_value_test(dictionary);
                let local_literals = if matches!(local_value, LocalValueTest::MixedEquality) {
                    Some(Box::new(predicate.local_equality_literals(dictionary)))
                } else {
                    None
                };
                let missing = predicate.matches_value(None);
                codes.push(CompiledPredicate {
                    predicate,
                    code,
                    global_matches: predicate.global_matches(library)?,
                    local_value,
                    local_literals,
                    missing,
                });
            }
            required_keys &= mask;
            alternatives |= mask;
            all_indexed &= mask != 0;
            selectors.push(CompiledSelector {
                types: selector.types,
                predicates: codes,
            });
        }
        let plan = if selectors.len() == 1 {
            let mut selector = selectors.pop().expect("one selector");
            if selector.predicates.is_empty() {
                CompiledPlan::All(selector.types)
            } else if let Some(first) = selector.predicates[0].dictionary_equality(library) {
                // A proven leading equality can skip the first predicate.
                // Evaluate the remaining predicates in their original order.
                let predicate = selector.predicates.remove(0);
                CompiledPlan::Equal(
                    selector.types,
                    predicate,
                    first,
                    selector.predicates.into_boxed_slice(),
                )
            } else if let Some(key) = selector.predicates[0].dictionary_set_key(library) {
                let predicate = selector.predicates.remove(0);
                CompiledPlan::ValueSet(
                    selector.types,
                    predicate,
                    key,
                    selector.predicates.into_boxed_slice(),
                )
            } else if selector.predicates.len() == 1 {
                let predicate = selector.predicates.pop().expect("one predicate");
                if let Some(key) = predicate.code {
                    let can_probe = !predicate.missing
                        && library.limits.max_tags != 0
                        && (!matches!(predicate.predicate.test, Test::Truth(true))
                            || !library.is_sole_category_key(key));
                    if can_probe {
                        if let Some(pattern) = predicate.single_local_byte_pattern() {
                            let has_global = predicate.global_matches.iter().any(|&w| w != 0);
                            CompiledPlan::ProbedText(
                                selector.types,
                                predicate,
                                key,
                                pattern,
                                has_global,
                            )
                        } else if matches!(predicate.predicate.test, Test::Truth(true)) {
                            let local_match = match predicate.local_value {
                                LocalValueTest::Constant(value) => Some(value),
                                _ => None,
                            };
                            if let Some(truthy) = DictionaryTruthy::from_global_matches(
                                key,
                                &predicate.global_matches,
                                library.string_count,
                                local_match,
                            ) {
                                CompiledPlan::ProbedTruthy(selector.types, predicate, truthy)
                            } else {
                                CompiledPlan::ProbedOne(selector.types, predicate, key)
                            }
                        } else {
                            CompiledPlan::ProbedOne(selector.types, predicate, key)
                        }
                    } else {
                        CompiledPlan::One(selector.types, predicate)
                    }
                } else {
                    CompiledPlan::LocalOne(selector.types, predicate)
                }
            } else if selector.predicates.len() == 2 {
                // Common two-condition selectors avoid the general nested loop.
                // Keep input order, including validation and short-circuiting.
                let second = selector.predicates.pop().expect("two predicates");
                let first = selector.predicates.pop().expect("two predicates");
                // If the later dictionary key is absent, the same prefix
                // covers every global entry the first lookup could consume.
                let pair = first
                    .code
                    .zip(second.code)
                    .filter(|(a, b)| a < b && !first.missing && !second.missing)
                    .map(|(first, second)| DictionaryPair { first, second });
                CompiledPlan::Two(selector.types, [first, second], pair)
            } else {
                CompiledPlan::Any(Box::new([selector]))
            }
        } else if let Some(tests) = compile_dictionary_union(&selectors, library) {
            let types = selectors[0].types;
            let predicates = selectors
                .into_iter()
                .map(|mut selector| {
                    selector
                        .predicates
                        .pop()
                        .expect("one exact predicate per union arm")
                })
                .collect();
            CompiledPlan::DictionaryUnion(types, predicates, tests)
        } else {
            CompiledPlan::Any(selectors.into_boxed_slice())
        };
        Ok(PreparedQuery {
            library,
            plan,
            types,
            required_keys,
            // If any alternative needs no indexed key, it may match every
            // root. Otherwise at least one category in the union is necessary.
            alternative_keys: if required_keys == 0 && all_indexed {
                alternatives
            } else {
                0
            },
        })
    }
}

impl Predicate {
    fn global_matches<S: Source>(&self, library: &Library<S>) -> Result<Box<[u64]>> {
        let dictionary = library.dictionary()?;
        let len = dictionary.len();
        let mut bits = vec![0; len.div_ceil(64)];
        let mut add_literal = |text: &str| {
            for code in dictionary.exact_codes(text, 0..len) {
                bits[code / 64] |= 1 << (code % 64);
            }
        };
        let invert = match &self.test {
            Test::Present => true,
            Test::Truth(want) => {
                add_literal("no");
                *want
            }
            Test::Equal { values, negate }
                if values.iter().all(|v| matches!(v, Literal::Exact(_))) =>
            {
                for value in values {
                    if let Literal::Exact(text) = value {
                        add_literal(text);
                    }
                }
                *negate
            }
            _ => {
                for code in 0..len {
                    if self.matches_value(Some(TagValue::Text(library.global_string(code as u16)?)))
                    {
                        bits[code / 64] |= 1 << (code % 64);
                    }
                }
                false
            }
        };
        if invert {
            for word in &mut bits {
                *word = !*word;
            }
        }
        Ok(bits.into_boxed_slice())
    }
}

/// A reusable query bound to a particular library's string codes and indexes.
pub struct PreparedQuery<'a, 'q, S> {
    pub(crate) library: &'a Library<S>,
    plan: CompiledPlan<'q>,
    types: u8,
    required_keys: u32,
    alternative_keys: u32,
}
#[derive(Clone, Copy)]
enum LocalBytePattern<'a> {
    Exact(&'a [u8]),
    Prefix(&'a [u8]),
    Suffix(&'a [u8]),
    Contains {
        needle: &'a [u8],
        splat_first: u64,
        splat_last: u64,
    },
}

trait ByteMatcher: Copy {
    fn matches_in_tail(self, tail: &[u8], len: usize) -> bool;
}

#[derive(Clone, Copy)]
struct ExactMatcher<'a>(&'a [u8]);
impl ByteMatcher for ExactMatcher<'_> {
    #[inline(always)]
    fn matches_in_tail(self, tail: &[u8], len: usize) -> bool {
        len == self.0.len() && &tail[..len] == self.0
    }
}

#[derive(Clone, Copy)]
struct PrefixMatcher<'a>(&'a [u8]);
impl ByteMatcher for PrefixMatcher<'_> {
    #[inline(always)]
    fn matches_in_tail(self, tail: &[u8], len: usize) -> bool {
        tail[..len].starts_with(self.0)
    }
}

#[derive(Clone, Copy)]
struct SuffixMatcher<'a>(&'a [u8]);
impl ByteMatcher for SuffixMatcher<'_> {
    #[inline(always)]
    fn matches_in_tail(self, tail: &[u8], len: usize) -> bool {
        tail[..len].ends_with(self.0)
    }
}

#[derive(Clone, Copy)]
struct ContainsMatcher<'a> {
    needle: &'a [u8],
    splat_first: u64,
    splat_last: u64,
}
impl ByteMatcher for ContainsMatcher<'_> {
    #[inline(always)]
    fn matches_in_tail(self, tail: &[u8], len: usize) -> bool {
        contains_bytes_in_tail(tail, len, self.needle, self.splat_first, self.splat_last)
    }
}

enum CompiledPlan<'q> {
    All(Types),
    One(Types, CompiledPredicate<'q>),
    ProbedText(
        Types,
        CompiledPredicate<'q>,
        u16,
        LocalBytePattern<'q>,
        bool,
    ),
    ProbedTruthy(Types, CompiledPredicate<'q>, DictionaryTruthy),
    ProbedOne(Types, CompiledPredicate<'q>, u16),
    Equal(
        Types,
        CompiledPredicate<'q>,
        DictionaryEquality,
        Box<[CompiledPredicate<'q>]>,
    ),
    ValueSet(
        Types,
        CompiledPredicate<'q>,
        u16,
        Box<[CompiledPredicate<'q>]>,
    ),
    LocalOne(Types, CompiledPredicate<'q>),
    Two(Types, [CompiledPredicate<'q>; 2], Option<DictionaryPair>),
    DictionaryUnion(Types, Box<[CompiledPredicate<'q>]>, DictionaryUnion),
    Any(Box<[CompiledSelector<'q>]>),
}
struct CompiledSelector<'q> {
    types: Types,
    predicates: Vec<CompiledPredicate<'q>>,
}
fn compile_dictionary_union<S: Source>(
    selectors: &[CompiledSelector<'_>],
    library: &Library<S>,
) -> Option<DictionaryUnion> {
    let types = selectors.first()?.types;
    let mut tests = Vec::with_capacity(selectors.len());
    for selector in selectors {
        let [predicate] = selector.predicates.as_slice() else {
            return None;
        };
        if selector.types != types {
            return None;
        }
        tests.push(predicate.dictionary_equality(library)?);
    }
    DictionaryUnion::from_tests(&tests)
}
struct CompiledPredicate<'q> {
    predicate: &'q Predicate,
    code: Option<u16>,
    global_matches: Box<[u64]>,
    local_value: LocalValueTest,
    local_literals: Option<Box<Box<[Literal]>>>,
    missing: bool,
}

impl CompiledPredicate<'_> {
    #[inline(always)]
    fn local_literals(&self) -> &[Literal] {
        self.local_literals
            .as_deref()
            .map_or(&[], |boxed| boxed.as_ref())
    }
}

#[derive(Clone, Copy)]
struct LocalTextProbe<'a, M> {
    key: u16,
    global_matches: &'a [u64],
    has_global: bool,
    matcher: M,
}

impl<M: ByteMatcher> DictionaryProbe for LocalTextProbe<'_, M> {
    const TAG_PROBE_LIMIT: u64 = 16;

    #[inline(always)]
    fn probe(
        self,
        bytes: &[u8],
        strings: usize,
        max_tags: u64,
        mut offset: usize,
    ) -> EqualityProbe {
        use EqualityProbe::*;
        let key = u32::from(self.key) << 2;
        let Some(mut tail) = bytes.get(offset..) else {
            return Unknown;
        };
        for _ in 0..max_tags.min(Self::TAG_PROBE_LIMIT) {
            let Some(word) = tail.first_chunk::<4>() else {
                return Unknown;
            };
            let entry = u32::from_le_bytes(*word);
            let key_and_last = entry & 0xfffc;
            if key_and_last & 0x7ffc == key {
                if entry == 0x8001 {
                    return Reject;
                }
                return match entry & 3 {
                    0 => Reject,
                    1 => {
                        let code = usize::from((entry >> 16) as u16);
                        if code >= strings {
                            Unknown
                        } else if self.has_global
                            && self.global_matches[code / 64] & (1 << (code % 64)) != 0
                        {
                            Match
                        } else {
                            Reject
                        }
                    }
                    2 => {
                        if tail.len() >= 6 {
                            Reject
                        } else {
                            Unknown
                        }
                    }
                    _ => {
                        let Some(wide) = tail.get(2..6) else {
                            return Unknown;
                        };
                        let delta = u32::from_le_bytes(wide.try_into().unwrap());
                        let target = (offset as u32).wrapping_add(2).wrapping_add(delta) as usize;
                        let Some((&len, str_tail)) =
                            bytes.get(target..).and_then(<[u8]>::split_first)
                        else {
                            return Unknown;
                        };
                        if len >= 128 {
                            return Unknown;
                        }
                        let len = usize::from(len);
                        if str_tail.len() < len {
                            return Unknown;
                        }
                        if self.matcher.matches_in_tail(str_tail, len) {
                            Match
                        } else {
                            Reject
                        }
                    }
                };
            }
            if key_and_last > key {
                return Reject;
            }
            let step = 4 + (entry as usize & 2);
            offset += step;
            let Some(next_tail) = tail.get(step..) else {
                return Unknown;
            };
            tail = next_tail;
        }
        Unknown
    }
}

#[derive(Clone, Copy)]
struct CommonKeyProbe<'a> {
    key: u16,
    predicate: &'a CompiledPredicate<'a>,
}

impl DictionaryProbe for CommonKeyProbe<'_> {
    const TAG_PROBE_LIMIT: u64 = 16;

    #[inline(always)]
    fn probe(
        self,
        bytes: &[u8],
        strings: usize,
        max_tags: u64,
        mut offset: usize,
    ) -> EqualityProbe {
        use EqualityProbe::*;
        let key = u32::from(self.key) << 2;
        let Some(mut tail) = bytes.get(offset..) else {
            return Unknown;
        };
        let missing = if self.predicate.missing {
            Match
        } else {
            Reject
        };
        for _ in 0..max_tags.min(Self::TAG_PROBE_LIMIT) {
            let Some(word) = tail.first_chunk::<4>() else {
                return Unknown;
            };
            let entry = u32::from_le_bytes(*word);
            let key_and_last = entry & 0xfffc;
            if key_and_last & 0x7ffc == key {
                if entry == 0x8001 {
                    return missing;
                }
                let matched = match entry & 3 {
                    0 => Some(
                        self.predicate
                            .predicate
                            .matches_value(Some(TagValue::Number {
                                mantissa: i32::from((entry >> 16) as u16) - 256,
                                scale: 0,
                            })),
                    ),
                    1 => {
                        let code = usize::from((entry >> 16) as u16);
                        if code >= strings {
                            None
                        } else {
                            Some(self.predicate.global_matches[code / 64] & (1 << (code % 64)) != 0)
                        }
                    }
                    2 => {
                        let Some(wide) = tail.get(2..6) else {
                            return Unknown;
                        };
                        let n = u32::from_le_bytes(wide.try_into().unwrap());
                        Some(
                            self.predicate
                                .predicate
                                .matches_value(Some(TagValue::Number {
                                    mantissa: (n >> 2) as i32 - 256,
                                    scale: (n & 3) as u8,
                                })),
                        )
                    }
                    _ => {
                        let Some(wide) = tail.get(2..6) else {
                            return Unknown;
                        };
                        match self.predicate.local_value {
                            LocalValueTest::Constant(result) => Some(result),
                            test => {
                                let delta = u32::from_le_bytes(wide.try_into().unwrap());
                                let target =
                                    (offset as u32).wrapping_add(2).wrapping_add(delta) as usize;
                                let Some((&len, str_tail)) =
                                    bytes.get(target..).and_then(<[u8]>::split_first)
                                else {
                                    return Unknown;
                                };
                                if len >= 128 {
                                    return Unknown;
                                }
                                let Some(slice) = str_tail.get(..usize::from(len)) else {
                                    return Unknown;
                                };
                                Some(self.predicate.predicate.matches_local_bytes(
                                    test,
                                    self.predicate.local_literals(),
                                    slice,
                                ))
                            }
                        }
                    }
                };
                return match matched {
                    Some(true) => Match,
                    Some(false) => Reject,
                    None => Unknown,
                };
            }
            if key_and_last > key {
                return missing;
            }
            let step = 4 + (entry as usize & 2);
            offset += step;
            let Some(next) = tail.get(step..) else {
                return Unknown;
            };
            tail = next;
        }
        Unknown
    }
}
impl<'q> CompiledPredicate<'q> {
    fn single_local_byte_pattern(&self) -> Option<LocalBytePattern<'q>> {
        if !matches!(self.local_value, LocalValueTest::Text) {
            return None;
        }
        let Test::Equal {
            negate: false,
            values,
        } = &self.predicate.test
        else {
            return None;
        };
        let [literal] = values.as_slice() else {
            return None;
        };
        match literal {
            Literal::Exact(text) => Some(LocalBytePattern::Exact(text.as_bytes())),
            Literal::Prefix(text) => Some(LocalBytePattern::Prefix(text.as_bytes())),
            Literal::Suffix(text) => Some(LocalBytePattern::Suffix(text.as_bytes())),
            Literal::Contains(text) => {
                let needle = text.as_bytes();
                let first = needle.first().copied().unwrap_or(0);
                let last = needle.last().copied().unwrap_or(0);
                Some(LocalBytePattern::Contains {
                    needle,
                    splat_first: u64::from_le_bytes([first; 8]),
                    splat_last: u64::from_le_bytes([last; 8]),
                })
            }
            Literal::Number(_) => None,
        }
    }
    fn dictionary_equality<S: Source>(&self, library: &Library<S>) -> Option<DictionaryEquality> {
        let Test::Equal {
            negate: false,
            values,
        } = &self.predicate.test
        else {
            return None;
        };
        if !matches!(values.as_slice(), [Literal::Exact(_)])
            || !matches!(self.local_value, LocalValueTest::Constant(false))
            || library.limits.max_tags == 0
        {
            return None;
        }
        // Duplicate dictionary strings can have several matching codes.
        let mut nonempty = self
            .global_matches
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, word)| *word != 0);
        let (group, bits) = nonempty.next()?;
        if !bits.is_power_of_two() || nonempty.next().is_some() {
            return None;
        }
        Some(DictionaryEquality {
            key: self.code?,
            value: (group * 64 + bits.trailing_zeros() as usize) as u16,
        })
    }

    fn dictionary_set_key<S: Source>(&self, library: &Library<S>) -> Option<u16> {
        let Test::Equal {
            negate: false,
            values,
        } = &self.predicate.test
        else {
            return None;
        };
        if values.len() < 2
            || library.limits.max_tags == 0
            || !values
                .iter()
                .all(|value| matches!(value, Literal::Exact(_)))
            || !self.global_matches.iter().any(|&word| word != 0)
        {
            return None;
        }
        self.code
    }

    #[inline(always)]
    fn scan_dictionary<'a, S: Source>(
        &self,
        features: &mut Features<'a, S>,
        step: impl FnMut() -> Result<bool>,
        mut emit: impl FnMut(Feature<'a, S>) -> Result<bool>,
        types: Types,
        rest: &[Self],
        probe: impl DictionaryProbe,
    ) -> Option<Result<Feature<'a, S>>> {
        features.next_equality(
            |feature, window, strings, first_matches| {
                Ok(
                    self.matches_leading(types, rest, feature, window, strings, first_matches)?
                        && emit(feature)?,
                )
            },
            step,
            probe,
        )
    }

    #[inline(always)]
    fn matches<S: Source>(
        &self,
        feature: Feature<'_, S>,
        window: &mut ReadWindow<'_, S>,
        strings: &mut ReadWindow<'_, S>,
    ) -> Result<bool> {
        let value = feature.encoded_tag(&self.predicate.key, self.code, window, strings)?;
        self.matches_encoded(feature, strings, value)
    }
    #[inline(always)]
    fn matches_encoded<S: Source>(
        &self,
        feature: Feature<'_, S>,
        strings: &mut ReadWindow<'_, S>,
        value: Option<EncodedValue>,
    ) -> Result<bool> {
        match value {
            None => Ok(self.missing),
            Some(EncodedValue::Global(code)) => {
                let code = usize::from(code);
                Ok(self.global_matches[code / 64] & (1 << (code % 64)) != 0)
            }
            Some(EncodedValue::Local { field, delta }) => {
                self.predicate.matches_local_value_prepared(
                    self.local_value,
                    self.local_literals(),
                    feature,
                    strings,
                    field,
                    delta,
                )
            }
            Some(EncodedValue::Number { mantissa, scale }) => Ok(self
                .predicate
                .matches_value(Some(TagValue::Number { mantissa, scale }))),
        }
    }
}
// Keep the hot serial plans in the iterator closure. The remaining plans
// use the normal dispatcher, and no plan owns per-result state.
enum StreamPlan<'a, 'p, 'q, S> {
    Resumable(resumable::Pool<'a, S>),
    Indexed(IndexedMatches<'a, S>),
    All(Features<'a, S>),
    Types(Features<'a, S>, Types),
    Equal(
        Features<'a, S>,
        Types,
        &'p CompiledPredicate<'q>,
        DictionaryEquality,
        &'p [CompiledPredicate<'q>],
    ),
    Other(Features<'a, S>),
}

impl CompiledPredicate<'_> {
    #[inline(always)]
    fn matches_leading<S: Source>(
        &self,
        types: Types,
        rest: &[Self],
        feature: Feature<'_, S>,
        window: &mut ReadWindow<'_, S>,
        strings: &mut ReadWindow<'_, S>,
        first_matches: bool,
    ) -> Result<bool> {
        if !types.matches(feature) || (!first_matches && !self.matches(feature, window, strings)?) {
            return Ok(false);
        }
        for predicate in rest {
            if !predicate.matches(feature, window, strings)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl<'a, S: Source> PreparedQuery<'a, '_, S> {
    // Select the traversal specialization once per yielded result, rather than
    // dispatching the plan for every rejected candidate in a selective query.
    #[cfg(any(feature = "parallel", test))]
    pub(crate) fn next_in(
        &self,
        features: &mut Features<'a, S>,
        step: impl FnMut() -> Result<bool>,
    ) -> Option<Result<Feature<'a, S>>> {
        self.scan_in(features, step, |_| Ok(true))
    }

    /// Count matching features without delivering individual handles to a consumer.
    ///
    /// This consumes the complete query and preserves traversal, header, bounds,
    /// and predicate validation. It does not cache answers or decode unused fields.
    pub fn count(&self) -> Result<u64> {
        self.count_in(None)
    }

    /// Count features whose stored bounding boxes intersect `bounds`.
    /// Uses the same inclusive candidate semantics as [`Self::in_bounds`].
    pub fn count_in_bounds(&self, bounds: Bounds) -> Result<u64> {
        self.count_in(Some(bounds))
    }

    fn count_in(&self, bounds: Option<Bounds>) -> Result<u64> {
        let mut features = self.features(bounds);
        self.count_features(&mut features, || Ok(true))
    }

    pub(crate) fn count_features(
        &self,
        features: &mut Features<'a, S>,
        step: impl FnMut() -> Result<bool>,
    ) -> Result<u64> {
        if self.library.source.as_bytes().is_some() {
            match &self.plan {
                CompiledPlan::All(types) => return features.count_all(*types, step),
                CompiledPlan::One(types, predicate) => {
                    if let Some(key) = predicate.code.filter(|_| {
                        matches!(predicate.predicate.test, Test::Truth(true))
                            && self.library.limits.max_tags != 0
                    }) {
                        let local_match = match predicate.local_value {
                            LocalValueTest::Constant(value) => Some(value),
                            _ => None,
                        };
                        if let Some(truthy) = DictionaryTruthy::from_global_matches(
                            key,
                            &predicate.global_matches,
                            self.library.string_count,
                            local_match,
                        ) {
                            return features.count_dictionary(
                                *types,
                                truthy,
                                |feature, window, strings| {
                                    predicate.matches(feature, window, strings)
                                },
                                step,
                            );
                        }
                        return features.count_dictionary(
                            *types,
                            DictionarySet {
                                key,
                                values: &predicate.global_matches,
                                numbers_match: true,
                                local_match,
                            },
                            |feature, window, strings| predicate.matches(feature, window, strings),
                            step,
                        );
                    }
                }
                CompiledPlan::Equal(types, first, equality, rest) if rest.is_empty() => {
                    return features.count_dictionary(
                        *types,
                        *equality,
                        |feature, window, strings| first.matches(feature, window, strings),
                        step,
                    );
                }
                CompiledPlan::ValueSet(types, first, key, rest) if rest.is_empty() => {
                    return features.count_dictionary(
                        *types,
                        DictionarySet {
                            key: *key,
                            values: &first.global_matches,
                            numbers_match: false,
                            local_match: match first.local_value {
                                LocalValueTest::Constant(value) => Some(value),
                                _ => None,
                            },
                        },
                        |feature, window, strings| first.matches(feature, window, strings),
                        step,
                    );
                }
                CompiledPlan::ProbedTruthy(types, predicate, truthy) => {
                    return features.count_dictionary(
                        *types,
                        *truthy,
                        |feature, window, strings| predicate.matches(feature, window, strings),
                        step,
                    );
                }
                CompiledPlan::ProbedText(types, predicate, key, pattern, has_global) => {
                    return match *pattern {
                        LocalBytePattern::Exact(needle) => features.count_dictionary(
                            *types,
                            LocalTextProbe {
                                key: *key,
                                global_matches: &predicate.global_matches,
                                has_global: *has_global,
                                matcher: ExactMatcher(needle),
                            },
                            |feature, window, strings| predicate.matches(feature, window, strings),
                            step,
                        ),
                        LocalBytePattern::Prefix(needle) => features.count_dictionary(
                            *types,
                            LocalTextProbe {
                                key: *key,
                                global_matches: &predicate.global_matches,
                                has_global: *has_global,
                                matcher: PrefixMatcher(needle),
                            },
                            |feature, window, strings| predicate.matches(feature, window, strings),
                            step,
                        ),
                        LocalBytePattern::Suffix(needle) => features.count_dictionary(
                            *types,
                            LocalTextProbe {
                                key: *key,
                                global_matches: &predicate.global_matches,
                                has_global: *has_global,
                                matcher: SuffixMatcher(needle),
                            },
                            |feature, window, strings| predicate.matches(feature, window, strings),
                            step,
                        ),
                        LocalBytePattern::Contains {
                            needle,
                            splat_first,
                            splat_last,
                        } => features.count_dictionary(
                            *types,
                            LocalTextProbe {
                                key: *key,
                                global_matches: &predicate.global_matches,
                                has_global: *has_global,
                                matcher: ContainsMatcher {
                                    needle,
                                    splat_first,
                                    splat_last,
                                },
                            },
                            |feature, window, strings| predicate.matches(feature, window, strings),
                            step,
                        ),
                    };
                }
                CompiledPlan::ProbedOne(types, predicate, key) => {
                    return features.count_dictionary(
                        *types,
                        CommonKeyProbe {
                            key: *key,
                            predicate,
                        },
                        |feature, window, strings| predicate.matches(feature, window, strings),
                        step,
                    );
                }
                CompiledPlan::DictionaryUnion(types, predicates, union) => {
                    return features.count_dictionary(
                        *types,
                        union,
                        |feature, window, strings| {
                            for predicate in predicates {
                                if predicate.matches(feature, window, strings)? {
                                    return Ok(true);
                                }
                            }
                            Ok(false)
                        },
                        step,
                    );
                }
                CompiledPlan::Two(types, [first, second], Some(pair)) => {
                    return features.count_dictionary(
                        *types,
                        *pair,
                        |feature, window, strings| {
                            Ok(first.matches(feature, window, strings)?
                                && second.matches(feature, window, strings)?)
                        },
                        step,
                    );
                }
                _ => {}
            }
        }
        self.fold_in(features, step, 0, |count, _| Ok(count + 1))
    }

    /// Reduce matching features without returning through an iterator per match.
    ///
    /// The callback receives checked handles and may read fields lazily. The
    /// first traversal or callback error stops execution and discards the partial
    /// accumulator. Callback side effects already performed are not rolled back.
    pub fn try_fold<T>(
        &self,
        initial: T,
        fold: impl FnMut(T, Feature<'a, S>) -> Result<T>,
    ) -> Result<T> {
        self.fold_unbounded(&mut self.features(None), initial, fold)
    }

    /// Reduce features whose stored bounding boxes intersect `bounds`.
    /// Uses the same inclusive candidate semantics as [`Self::in_bounds`].
    pub fn try_fold_in_bounds<T>(
        &self,
        bounds: Bounds,
        initial: T,
        fold: impl FnMut(T, Feature<'a, S>) -> Result<T>,
    ) -> Result<T> {
        self.fold_unbounded(&mut self.features(Some(bounds)), initial, fold)
    }

    pub(crate) fn fold_unbounded<T>(
        &self,
        features: &mut Features<'a, S>,
        initial: T,
        mut fold: impl FnMut(T, Feature<'a, S>) -> Result<T>,
    ) -> Result<T> {
        if let CompiledPlan::All(types) = &self.plan
            && self.library.source.as_bytes().is_some()
        {
            return features.try_fold_all(*types, initial, fold, || Ok(true));
        }
        let mut value = Some(initial);
        self.scan_unbounded_erased(features, &mut |feature| {
            let previous = value
                .take()
                .expect("a successful fold retains its accumulator");
            value = Some(fold(previous, feature)?);
            Ok(false)
        })
        .transpose()?;
        Ok(value.expect("a successful fold retains its accumulator"))
    }

    pub(crate) fn fold_in<T>(
        &self,
        features: &mut Features<'a, S>,
        step: impl FnMut() -> Result<bool>,
        initial: T,
        mut fold: impl FnMut(T, Feature<'a, S>) -> Result<T>,
    ) -> Result<T> {
        if let CompiledPlan::All(types) = &self.plan
            && self.library.source.as_bytes().is_some()
        {
            return features.try_fold_all(*types, initial, fold, step);
        }
        let mut value = Some(initial);
        self.scan_in(features, step, |feature| {
            let previous = value
                .take()
                .expect("a successful fold retains its accumulator");
            value = Some(fold(previous, feature)?);
            Ok(false)
        })
        .transpose()?;
        Ok(value.expect("a successful fold retains its accumulator"))
    }

    #[inline(never)]
    fn scan_probed_text<M: ByteMatcher>(
        predicate: &CompiledPredicate<'_>,
        features: &mut Features<'a, S>,
        step: impl FnMut() -> Result<bool>,
        emit: impl FnMut(Feature<'a, S>) -> Result<bool>,
        types: Types,
        probe: LocalTextProbe<'_, M>,
    ) -> Option<Result<Feature<'a, S>>> {
        predicate.scan_dictionary(features, step, emit, types, &[], probe)
    }

    #[inline(never)]
    fn scan_probed_truthy(
        predicate: &CompiledPredicate<'_>,
        features: &mut Features<'a, S>,
        step: impl FnMut() -> Result<bool>,
        emit: impl FnMut(Feature<'a, S>) -> Result<bool>,
        types: Types,
        truthy: DictionaryTruthy,
    ) -> Option<Result<Feature<'a, S>>> {
        predicate.scan_dictionary(features, step, emit, types, &[], truthy)
    }

    #[inline(never)]
    fn scan_probed_one(
        predicate: &CompiledPredicate<'_>,
        features: &mut Features<'a, S>,
        step: impl FnMut() -> Result<bool>,
        emit: impl FnMut(Feature<'a, S>) -> Result<bool>,
        types: Types,
        key: u16,
    ) -> Option<Result<Feature<'a, S>>> {
        predicate.scan_dictionary(
            features,
            step,
            emit,
            types,
            &[],
            CommonKeyProbe { key, predicate },
        )
    }

    pub(crate) fn scan_in(
        &self,
        features: &mut Features<'a, S>,
        mut step: impl FnMut() -> Result<bool>,
        mut emit: impl FnMut(Feature<'a, S>) -> Result<bool>,
    ) -> Option<Result<Feature<'a, S>>> {
        self.scan_in_erased(features, &mut step, &mut emit)
    }

    // Single concrete instantiation for unbounded iterator scans: both `step`
    // and `emit` constant-fold away inside tight whole-tile leaf loops.
    #[inline]
    fn next_unbounded(&self, features: &mut Features<'a, S>) -> Option<Result<Feature<'a, S>>> {
        self.scan_plan(features, || Ok(true), |_| Ok(true))
    }

    // Single concrete instantiation for unbounded folds and spatial filters:
    // `step` constant-folds away on rejected candidates while `emit` stays
    // erased so callers do not multiply plan arms per closure.
    #[inline(never)]
    pub(crate) fn scan_unbounded_erased(
        &self,
        features: &mut Features<'a, S>,
        emit: &mut dyn FnMut(Feature<'a, S>) -> Result<bool>,
    ) -> Option<Result<Feature<'a, S>>> {
        self.scan_plan(features, || Ok(true), emit)
    }

    // Type-erase `step` and `emit` for budgeted parallel leases so callers do
    // not multiply all ten CompiledPlan arms and Features::advance per closure.
    #[inline(never)]
    fn scan_in_erased(
        &self,
        features: &mut Features<'a, S>,
        step: &mut dyn FnMut() -> Result<bool>,
        emit: &mut dyn FnMut(Feature<'a, S>) -> Result<bool>,
    ) -> Option<Result<Feature<'a, S>>> {
        self.scan_plan(features, step, emit)
    }

    #[inline(always)]
    fn scan_plan(
        &self,
        features: &mut Features<'a, S>,
        step: impl FnMut() -> Result<bool>,
        mut emit: impl FnMut(Feature<'a, S>) -> Result<bool>,
    ) -> Option<Result<Feature<'a, S>>> {
        match &self.plan {
            CompiledPlan::All(types) => features.next_with_budget::<true>(
                |feature, _, _| Ok(types.matches(feature) && emit(feature)?),
                step,
            ),
            CompiledPlan::LocalOne(types, predicate) => features.next_with_budget::<false>(
                |feature, window, strings| {
                    if !types.matches(feature) {
                        return Ok(false);
                    }
                    let value =
                        feature.encoded_local_tag(&predicate.predicate.key, window, strings)?;
                    Ok(predicate.matches_encoded(feature, strings, value)? && emit(feature)?)
                },
                step,
            ),
            CompiledPlan::Equal(types, first, equality, rest) => {
                first.scan_dictionary(features, step, emit, *types, rest, *equality)
            }
            CompiledPlan::ValueSet(types, first, key, rest) => first.scan_dictionary(
                features,
                step,
                emit,
                *types,
                rest,
                DictionarySet {
                    key: *key,
                    values: &first.global_matches,
                    numbers_match: false,
                    local_match: match first.local_value {
                        LocalValueTest::Constant(value) => Some(value),
                        _ => None,
                    },
                },
            ),
            CompiledPlan::One(types, predicate) => features.next_with_budget::<false>(
                |feature, window, strings| {
                    if !types.matches(feature) {
                        return Ok(false);
                    }
                    Ok(predicate.matches(feature, window, strings)? && emit(feature)?)
                },
                step,
            ),
            CompiledPlan::ProbedText(types, predicate, key, pattern, has_global) => {
                match *pattern {
                    LocalBytePattern::Exact(needle) => Self::scan_probed_text(
                        predicate,
                        features,
                        step,
                        emit,
                        *types,
                        LocalTextProbe {
                            key: *key,
                            global_matches: &predicate.global_matches,
                            has_global: *has_global,
                            matcher: ExactMatcher(needle),
                        },
                    ),
                    LocalBytePattern::Prefix(needle) => Self::scan_probed_text(
                        predicate,
                        features,
                        step,
                        emit,
                        *types,
                        LocalTextProbe {
                            key: *key,
                            global_matches: &predicate.global_matches,
                            has_global: *has_global,
                            matcher: PrefixMatcher(needle),
                        },
                    ),
                    LocalBytePattern::Suffix(needle) => Self::scan_probed_text(
                        predicate,
                        features,
                        step,
                        emit,
                        *types,
                        LocalTextProbe {
                            key: *key,
                            global_matches: &predicate.global_matches,
                            has_global: *has_global,
                            matcher: SuffixMatcher(needle),
                        },
                    ),
                    LocalBytePattern::Contains {
                        needle,
                        splat_first,
                        splat_last,
                    } => Self::scan_probed_text(
                        predicate,
                        features,
                        step,
                        emit,
                        *types,
                        LocalTextProbe {
                            key: *key,
                            global_matches: &predicate.global_matches,
                            has_global: *has_global,
                            matcher: ContainsMatcher {
                                needle,
                                splat_first,
                                splat_last,
                            },
                        },
                    ),
                }
            }
            CompiledPlan::ProbedTruthy(types, predicate, truthy) => {
                Self::scan_probed_truthy(predicate, features, step, emit, *types, *truthy)
            }
            CompiledPlan::ProbedOne(types, predicate, key) => {
                Self::scan_probed_one(predicate, features, step, emit, *types, *key)
            }
            CompiledPlan::Two(types, [first, second], pair) => {
                let mut matches =
                    |feature, window: &mut ReadWindow<'_, S>, strings: &mut ReadWindow<'_, S>| {
                        Ok(types.matches(feature)
                            && first.matches(feature, window, strings)?
                            && second.matches(feature, window, strings)?
                            && emit(feature)?)
                    };
                if let Some(pair) = pair {
                    features.next_equality(|f, w, s, _| matches(f, w, s), step, *pair)
                } else {
                    features.next_with_budget::<false>(matches, step)
                }
            }
            CompiledPlan::DictionaryUnion(types, predicates, tests) => features.next_equality(
                |feature, window, strings, probed| {
                    if !types.matches(feature) {
                        return Ok(false);
                    }
                    if probed {
                        return emit(feature);
                    }
                    for predicate in predicates {
                        if predicate.matches(feature, window, strings)? {
                            return emit(feature);
                        }
                    }
                    Ok(false)
                },
                step,
                tests,
            ),
            CompiledPlan::Any(selectors) => features.next_with_budget::<false>(
                |feature, window, strings| {
                    for selector in selectors {
                        if !selector.types.matches(feature) {
                            continue;
                        }
                        let mut yes = true;
                        for predicate in &selector.predicates {
                            if !predicate.matches(feature, window, strings)? {
                                yes = false;
                                break;
                            }
                        }
                        if yes {
                            return emit(feature);
                        }
                    }
                    Ok(false)
                },
                step,
            ),
        }
    }
    /// Stream every matching feature in file traversal order.
    pub fn iter(&self) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, S>>> + '_ {
        self.execute(None)
    }
    fn exact_text(&self) -> Option<(Types, &CompiledPredicate<'_>, &str)> {
        let (types, predicate) = match &self.plan {
            CompiledPlan::Equal(types, predicate, _, rest) if rest.is_empty() => (types, predicate),
            CompiledPlan::One(types, predicate)
            | CompiledPlan::ProbedText(types, predicate, ..)
            | CompiledPlan::ProbedOne(types, predicate, _)
            | CompiledPlan::LocalOne(types, predicate) => (types, predicate),
            _ => return None,
        };
        let Test::Equal {
            negate: false,
            values,
        } = &predicate.predicate.test
        else {
            return None;
        };
        let [Literal::Exact(value)] = values.as_slice() else {
            return None;
        };
        Some((*types, predicate, value))
    }

    pub(crate) fn indexed_matches(&self, bounds: Option<Bounds>) -> Option<IndexedMatches<'a, S>> {
        if bounds.is_some() {
            return None;
        }
        let adaptive = self.library.adaptive_text_index.index.get();
        if self.library.text_indexes.is_empty() && adaptive.is_none() {
            return None;
        }
        let (types, predicate, value) = self.exact_text()?;
        let index = self
            .library
            .text_indexes
            .iter()
            .chain(adaptive)
            .find(|index| index.matches_key(&predicate.predicate.key, types))?;
        Some(index.matching(self.library, value))
    }

    pub(crate) fn scan_observation(
        &self,
        bounds: Option<Bounds>,
    ) -> Option<ScanObservation<'a, '_, S>> {
        if bounds.is_some()
            || self.required_keys == 0
            || !self.library.adaptive_text_index.learning()
            || self.library.source.as_bytes().is_none()
        {
            return None;
        }
        let (types, predicate, _) = self.exact_text()?;
        let key = ScanKey {
            code: predicate.code?,
            types,
        };
        Some(ScanObservation::new(
            self.library,
            key,
            &predicate.predicate.key,
        ))
    }
    /// Stream matches whose stored bounding boxes intersect `bounds`, inclusively.
    pub fn in_bounds(
        &self,
        bounds: Bounds,
    ) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, S>>> + '_ {
        self.execute(Some(bounds))
    }
    pub(crate) fn features(&self, bounds: Option<Bounds>) -> Features<'a, S> {
        Features::new(self.library, bounds)
            .restrict(self.types, self.required_keys)
            .require_any(self.alternative_keys)
    }
    #[cfg(feature = "parallel")]
    pub(crate) fn step_limit(&self) -> u64 {
        self.library.limits.max_traversal_steps
    }
    pub(crate) fn execute(
        &self,
        bounds: Option<Bounds>,
    ) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, S>>> + '_ {
        self.execute_with_branch_width(bounds, 0)
    }

    /// Traverse only R-tree branches that can contain a feature at least this wide
    /// in projected X units. Surviving leaves can still contain narrower features.
    pub fn candidates_in_bounds_with_min_width(
        &self,
        bounds: Bounds,
        min_width: u32,
    ) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, S>>> + '_ {
        self.execute_with_branch_width(Some(bounds), min_width)
    }

    fn execute_with_branch_width(
        &self,
        bounds: Option<Bounds>,
        min_branch_width: u32,
    ) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, S>>> + '_ {
        let mut observation = None;
        let mut seen = 0;
        let mut stream = if let Some(indexed) = self.indexed_matches(bounds) {
            StreamPlan::Indexed(indexed)
        } else if bounds.is_some()
            && self
                .library
                .source()
                .range_query()
                .is_some_and(|schedule| schedule.width() != 0)
        {
            let mut features = self.features(bounds);
            features.min_branch_width = min_branch_width;
            StreamPlan::Resumable(resumable::Pool::new(features))
        } else {
            observation = self.scan_observation(bounds);
            let mut features = self.features(bounds);
            features.min_branch_width = min_branch_width;
            #[cfg(all(feature = "mmap", target_os = "linux"))]
            let features = features.enable_mapped_ahead();
            match &self.plan {
                CompiledPlan::All(Types::ALL) => StreamPlan::All(features),
                CompiledPlan::All(types) => StreamPlan::Types(features, *types),
                CompiledPlan::Equal(types, first, equality, rest) => {
                    StreamPlan::Equal(features, *types, first, *equality, rest)
                }
                _ => StreamPlan::Other(features),
            }
        };
        std::iter::from_fn(move || match &mut stream {
            StreamPlan::Resumable(pool) => pool.next(self),
            StreamPlan::Indexed(matches) => matches.next(),
            StreamPlan::All(features) => {
                features.next_with_budget::<true>(|_, _, _| Ok(true), || Ok(true))
            }
            StreamPlan::Types(features, types) => features
                .next_with_budget::<true>(|feature, _, _| Ok(types.matches(feature)), || Ok(true)),
            StreamPlan::Equal(features, types, first, equality, rest) => {
                let result = features.next_equality(
                    |feature, window, strings, first_matches| {
                        first.matches_leading(*types, rest, feature, window, strings, first_matches)
                    },
                    || Ok(true),
                    *equality,
                );
                observe_result(&mut observation, &mut seen, &result, features.steps());
                result
            }
            StreamPlan::Other(features) => {
                let result = self.next_unbounded(features);
                observe_result(&mut observation, &mut seen, &result, features.steps());
                result
            }
        })
        .fuse()
    }
}

#[inline]
fn observe_result<S: Source>(
    observation: &mut Option<ScanObservation<'_, '_, S>>,
    seen: &mut u8,
    result: &Option<Result<Feature<'_, S>>>,
    steps: u64,
) {
    if observation.is_some() {
        match result {
            Some(Ok(_)) => *seen = (*seen + 1).min(2),
            Some(Err(_)) => *observation = None,
            None => observation
                .take()
                .expect("active observation")
                .finish(steps, *seen),
        }
    }
}

#[cfg(test)]
mod equality_tests;
#[cfg(test)]
mod plan_tests;

#[cfg(all(unix, feature = "experimental-tag-sidecars"))]
mod tag_sidecar;
