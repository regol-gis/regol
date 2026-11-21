use crate::{
    Bounds, Error, Feature, FeatureType, Features, Library, Result, SpatialFilter, TagValue,
    decode::ReadWindow, feature::EncodedValue, source::Source,
};
use regex::bytes::Regex;

mod parser;
mod prepared;
#[cfg(all(unix, feature = "experimental-tag-sidecars"))]
mod tag_sidecar;
pub use prepared::PreparedQuery;

/// GOQL categories: area ways/relations belong to AREA, not WAY/RELATION.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Types(pub(crate) u8);
impl Types {
    /// Select nodes.
    pub const NODE: Self = Self(1);
    /// Select non-area ways.
    pub const WAY: Self = Self(2);
    /// Select area ways and area relations.
    pub const AREA: Self = Self(4);
    /// Select non-area relations.
    pub const RELATION: Self = Self(8);
    /// Select all feature categories.
    pub const ALL: Self = Self(15);
    /// Combine the categories from both masks.
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    pub(crate) fn matches<S: Source>(self, f: Feature<'_, S>) -> bool {
        let bit = if f.is_area() {
            4
        } else {
            match f.kind() {
                FeatureType::Node => 1,
                FeatureType::Way => 2,
                FeatureType::Relation => 8,
            }
        };
        self.0 & bit != 0
    }
    #[inline(always)]
    pub(crate) fn matches_flags(self, flags: u16) -> bool {
        let bit = if flags & 2 != 0 {
            4
        } else {
            match flags & 24 {
                0 => 1,
                8 => 2,
                _ => 8,
            }
        };
        self.0 & bit != 0
    }
}

#[derive(Clone, Debug)]
enum Test {
    Truth(bool),
    Equal { values: Vec<Literal>, negate: bool },
    Numeric { op: OrderingOp, value: f64 },
    Regex { pattern: Regex, negate: bool },
}
#[derive(Clone, Debug)]
enum Literal {
    Exact(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
    Number(f64),
}
impl Literal {
    fn text(mut pattern: String) -> Self {
        let starts = pattern.starts_with('*');
        let ends = pattern.ends_with('*');
        // Strip only ASCII edge wildcards, preserving UTF-8 boundaries and the
        // existing allocation. Classify once, rather than for every local tag.
        pattern.truncate(pattern.trim_end_matches('*').len());
        let leading = pattern.len() - pattern.trim_start_matches('*').len();
        pattern.drain(..leading);
        match (starts, ends) {
            (true, true) => Self::Contains(pattern),
            (true, false) => Self::Suffix(pattern),
            (false, true) => Self::Prefix(pattern),
            (false, false) => Self::Exact(pattern),
        }
    }
    #[inline(always)]
    fn matches_bytes(&self, bytes: &[u8]) -> bool {
        match self {
            Self::Exact(pattern) => bytes == pattern.as_bytes(),
            Self::Prefix(pattern) => bytes.starts_with(pattern.as_bytes()),
            Self::Suffix(pattern) => bytes.ends_with(pattern.as_bytes()),
            Self::Contains(pattern) => contains_bytes(bytes, pattern.as_bytes()),
            Self::Number(expected) => number_bytes(bytes).is_some_and(|actual| actual == *expected),
        }
    }
    fn matches(&self, value: &TagValue<'_>) -> bool {
        match self {
            Self::Exact(pattern) => {
                matches!(value, TagValue::Text(text) if text.as_ref() == pattern.as_str())
            }
            Self::Prefix(pattern) => {
                matches!(value, TagValue::Text(text) if text.starts_with(pattern))
            }
            Self::Suffix(pattern) => {
                matches!(value, TagValue::Text(text) if text.ends_with(pattern))
            }
            Self::Contains(pattern) => {
                matches!(value, TagValue::Text(text) if text.contains(pattern))
            }
            Self::Number(expected) => number(value).is_some_and(|actual| actual == *expected),
        }
    }
}

#[inline(always)]
fn zero_bytes_u64(v: u64) -> u64 {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    v.wrapping_sub(LO) & !v & HI
}

#[inline(always)]
fn zero_bytes_u32(v: u32) -> u32 {
    const LO: u32 = 0x0101_0101;
    const HI: u32 = 0x8080_8080;
    v.wrapping_sub(LO) & !v & HI
}

#[inline(always)]
fn find_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    let len = haystack.len();
    if len >= 8 {
        let splat = u64::from_le_bytes([needle; 8]);
        let mut i = 0;
        loop {
            let word = u64::from_le_bytes(haystack[i..i + 8].try_into().unwrap());
            let hits = zero_bytes_u64(word ^ splat);
            if hits != 0 {
                return Some(i + (hits.trailing_zeros() >> 3) as usize);
            }
            if i + 8 == len {
                return None;
            }
            i = (i + 8).min(len - 8);
        }
    }
    if len >= 4 {
        let splat = u32::from_le_bytes([needle; 4]);
        let w0 = u32::from_le_bytes(haystack[..4].try_into().unwrap());
        let hits0 = zero_bytes_u32(w0 ^ splat);
        if hits0 != 0 {
            return Some((hits0.trailing_zeros() >> 3) as usize);
        }
        let tail = len - 4;
        if tail != 0 {
            let w1 = u32::from_le_bytes(haystack[tail..len].try_into().unwrap());
            let hits1 = zero_bytes_u32(w1 ^ splat);
            if hits1 != 0 {
                return Some(tail + (hits1.trailing_zeros() >> 3) as usize);
            }
        }
        return None;
    }
    for (i, &b) in haystack.iter().enumerate() {
        if b == needle {
            return Some(i);
        }
    }
    None
}

const LOW_BYTE_MASK: [u64; 9] = [
    0x0000_0000_0000_0000,
    0x0000_0000_0000_00ff,
    0x0000_0000_0000_ffff,
    0x0000_0000_00ff_ffff,
    0x0000_0000_ffff_ffff,
    0x0000_00ff_ffff_ffff,
    0x0000_ffff_ffff_ffff,
    0x00ff_ffff_ffff_ffff,
    0xffff_ffff_ffff_ffff,
];

/// Branchless two-byte SWAR substring check using the surrounding tile slice
/// (`tail`, where `tail[..len]` is the haystack) so short OSM strings (<= 20 bytes)
/// avoid unpredictable length-dispatch branches and scalar tail loops.
#[inline(always)]
pub(crate) fn contains_bytes_in_tail(
    tail: &[u8],
    len: usize,
    needle: &[u8],
    splat_first: u64,
    splat_last: u64,
) -> bool {
    let n = needle.len();
    if len < n {
        return false;
    }
    if n <= 1 {
        return n == 0 || find_byte(&tail[..len], needle[0]).is_some();
    }
    let last_idx = n - 1;
    let limit = len - last_idx;
    if limit <= 16 && tail.len() >= 16 + last_idx {
        let w0 = u64::from_le_bytes(tail[0..8].try_into().unwrap());
        let w1 = u64::from_le_bytes(tail[8..16].try_into().unwrap());
        let l0 = u64::from_le_bytes(tail[last_idx..last_idx + 8].try_into().unwrap());
        let l1 = u64::from_le_bytes(tail[8 + last_idx..16 + last_idx].try_into().unwrap());
        let mut m0 = zero_bytes_u64(w0 ^ splat_first)
            & zero_bytes_u64(l0 ^ splat_last)
            & LOW_BYTE_MASK[limit.min(8)];
        let mut m1 = zero_bytes_u64(w1 ^ splat_first)
            & zero_bytes_u64(l1 ^ splat_last)
            & LOW_BYTE_MASK[limit.saturating_sub(8)];
        if (m0 | m1) == 0 {
            return false;
        }
        let mid = &needle[1..last_idx];
        while m0 != 0 {
            let pos = (m0.trailing_zeros() >> 3) as usize;
            if mid.is_empty() || &tail[pos + 1..pos + last_idx] == mid {
                return true;
            }
            m0 &= m0 - 1;
        }
        while m1 != 0 {
            let pos = 8 + (m1.trailing_zeros() >> 3) as usize;
            if mid.is_empty() || &tail[pos + 1..pos + last_idx] == mid {
                return true;
            }
            m1 &= m1 - 1;
        }
        return false;
    }
    contains_bytes_prepared(&tail[..len], needle, splat_first, splat_last)
}

#[inline(always)]
pub(crate) fn contains_bytes_prepared(
    haystack: &[u8],
    needle: &[u8],
    splat_first: u64,
    splat_last: u64,
) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    let hlen = haystack.len();
    if hlen < n {
        return false;
    }
    if n == 1 {
        return find_byte(haystack, needle[0]).is_some();
    }
    let last_idx = n - 1;
    let limit = hlen - last_idx;
    if limit >= 8 {
        let mid = &needle[1..last_idx];
        let mut i = 0;
        loop {
            let w0 = u64::from_le_bytes(haystack[i..i + 8].try_into().unwrap());
            let w1 =
                u64::from_le_bytes(haystack[i + last_idx..i + last_idx + 8].try_into().unwrap());
            let mut hits = zero_bytes_u64(w0 ^ splat_first) & zero_bytes_u64(w1 ^ splat_last);
            while hits != 0 {
                let pos = i + (hits.trailing_zeros() >> 3) as usize;
                if mid.is_empty() || &haystack[pos + 1..pos + last_idx] == mid {
                    return true;
                }
                hits &= hits - 1;
            }
            if i + 8 == limit {
                return false;
            }
            i = (i + 8).min(limit - 8);
        }
    }
    let first = needle[0];
    let last = needle[last_idx];
    let mid = &needle[1..last_idx];
    for i in 0..limit {
        if haystack[i] == first
            && haystack[i + last_idx] == last
            && (mid.is_empty() || &haystack[i + 1..i + last_idx] == mid)
        {
            return true;
        }
    }
    false
}

#[inline(always)]
pub(crate) fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    let Some(&first) = needle.first() else {
        return true;
    };
    let last = needle[needle.len() - 1];
    contains_bytes_prepared(
        haystack,
        needle,
        u64::from_le_bytes([first; 8]),
        u64::from_le_bytes([last; 8]),
    )
}

// GOQL reads an ASCII decimal prefix from text tags. Leading whitespace and a
// minus sign are accepted; plus signs and exponents are not part of tag numbers.
// Numeric literals in queries are parsed separately and may use an exponent.
fn number_bytes(mut bytes: &[u8]) -> Option<f64> {
    while let Some((&first, rest)) = bytes.split_first() {
        if first.is_ascii_whitespace() {
            bytes = rest;
        } else {
            break;
        }
    }
    let mut end = usize::from(bytes.first() == Some(&b'-'));
    let start = end;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    let mut digits = end - start;
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        let start = end;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        digits += end - start;
    }
    if digits == 0 {
        None
    } else {
        // Slice contains only ASCII '-', '.', and digits, so it is valid UTF-8.
        std::str::from_utf8(&bytes[..end]).ok()?.parse().ok()
    }
}

fn number(value: &TagValue<'_>) -> Option<f64> {
    match value {
        TagValue::Text(text) => number_bytes(text.as_bytes()),
        _ => value.as_number(),
    }
}

#[derive(Clone, Copy, Debug)]
enum OrderingOp {
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

/// A GOQL tag condition for a programmatically constructed selector.
#[derive(Clone, Debug)]
pub struct Predicate {
    key: String,
    test: Test,
}

/// What the selected GOQL operands can match when the stored value is local text.
#[derive(Clone, Copy)]
enum LocalValueTest {
    Constant(bool),
    Text,
    MixedEquality,
}
impl Predicate {
    /// GOQL truth test: present and not equal to `no`. Empty values are true.
    pub fn truthy(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            test: Test::Truth(true),
        }
    }
    /// Match a missing key or a value treated as false by GOQL.
    pub fn falsy(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            test: Test::Truth(false),
        }
    }
    /// Match a text value. Leading and trailing asterisks are wildcards.
    pub fn equal(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            test: Test::Equal {
                values: vec![Literal::text(value.into())],
                negate: false,
            },
        }
    }
    /// Match a missing key or a value outside the specified text pattern.
    pub fn not_equal(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            test: Test::Equal {
                values: vec![Literal::text(value.into())],
                negate: true,
            },
        }
    }
    /// Numeric equality, including GOQL decimal-prefix conversion of text tags.
    pub fn equal_number(key: impl Into<String>, value: f64) -> Result<Self> {
        if !value.is_finite() {
            return Err(Error::Query {
                offset: 0,
                message: "expected finite number".into(),
            });
        }
        Ok(Self {
            key: key.into(),
            test: Test::Equal {
                values: vec![Literal::Number(value)],
                negate: false,
            },
        })
    }
    /// Require the GOQL numeric value to meet or exceed the threshold.
    pub fn at_least(key: impl Into<String>, value: f64) -> Self {
        Self {
            key: key.into(),
            test: Test::Numeric {
                op: OrderingOp::GreaterEqual,
                value,
            },
        }
    }
    fn matches<S: Source>(
        &self,
        feature: Feature<'_, S>,
        tags: &mut ReadWindow<'_, S>,
        strings: &mut ReadWindow<'_, S>,
    ) -> Result<bool> {
        let library = feature.library;
        let dictionary = library.dictionary()?;
        let value =
            match feature.encoded_tag(&self.key, dictionary.key_code(&self.key), tags, strings)? {
                None => None,
                Some(EncodedValue::Global(code)) => {
                    Some(TagValue::Text(library.global_string(code)?))
                }
                Some(EncodedValue::Number { mantissa, scale }) => {
                    Some(TagValue::Number { mantissa, scale })
                }
                Some(EncodedValue::Local { field, delta }) => {
                    return self.matches_local_value(
                        self.local_value_test(dictionary),
                        feature,
                        strings,
                        field,
                        delta,
                    );
                }
            };
        Ok(self.matches_value(value))
    }
    fn local_value_test(&self, dictionary: &crate::library::Dictionary) -> LocalValueTest {
        match &self.test {
            Test::Truth(want) if dictionary.has_global_value("no") => {
                LocalValueTest::Constant(*want)
            }
            Test::Equal { values, negate } => {
                let encoded = values.iter().filter(|value| {
                    matches!(value, Literal::Exact(text) if dictionary.has_global_value(text))
                }).count();
                if encoded == 0 {
                    LocalValueTest::Text
                } else if encoded == values.len() {
                    LocalValueTest::Constant(*negate)
                } else {
                    LocalValueTest::MixedEquality
                }
            }
            _ => LocalValueTest::Text,
        }
    }
    fn local_equality_literals(&self, dictionary: &crate::library::Dictionary) -> Box<[Literal]> {
        let Test::Equal { values, .. } = &self.test else {
            return Box::new([]);
        };
        values
            .iter()
            .filter(|literal| {
                !matches!(literal, Literal::Exact(text) if dictionary.has_global_value(text))
            })
            .cloned()
            .collect()
    }
    #[inline]
    fn matches_local_bytes(
        &self,
        test: LocalValueTest,
        local_literals: &[Literal],
        bytes: &[u8],
    ) -> bool {
        match test {
            LocalValueTest::Constant(result) => result,
            LocalValueTest::MixedEquality => {
                let Test::Equal { negate, .. } = &self.test else {
                    unreachable!("mixed equality requires equal test");
                };
                local_literals
                    .iter()
                    .any(|literal| literal.matches_bytes(bytes))
                    != *negate
            }
            LocalValueTest::Text => match &self.test {
                Test::Truth(want) => (bytes != b"no") == *want,
                Test::Equal { values, negate } => {
                    values.iter().any(|literal| literal.matches_bytes(bytes)) != *negate
                }
                Test::Regex { pattern, negate } => pattern.is_match(bytes) != *negate,
                Test::Numeric {
                    op,
                    value: expected,
                } => number_bytes(bytes).is_some_and(|actual| match op {
                    OrderingOp::Less => actual < *expected,
                    OrderingOp::LessEqual => actual <= *expected,
                    OrderingOp::Greater => actual > *expected,
                    OrderingOp::GreaterEqual => actual >= *expected,
                }),
            },
        }
    }
    fn matches_local_value_prepared<S: Source>(
        &self,
        test: LocalValueTest,
        local_literals: &[Literal],
        feature: Feature<'_, S>,
        strings: &mut ReadWindow<'_, S>,
        field: u64,
        delta: i32,
    ) -> Result<bool> {
        if let LocalValueTest::Constant(result) = test {
            return Ok(result);
        }
        let p = feature.local_value_target(field, delta)?;
        strings.seek(p)?;
        let bytes = feature.local_bytes_window(strings, p)?;
        Ok(self.matches_local_bytes(test, local_literals, &bytes))
    }
    fn matches_local_value<S: Source>(
        &self,
        test: LocalValueTest,
        feature: Feature<'_, S>,
        strings: &mut ReadWindow<'_, S>,
        field: u64,
        delta: i32,
    ) -> Result<bool> {
        if let LocalValueTest::Constant(result) = test {
            return Ok(result);
        }
        let p = feature.local_value_target(field, delta)?;
        strings.seek(p)?;
        let bytes = feature.local_bytes_window(strings, p)?;
        if let (LocalValueTest::MixedEquality, Test::Equal { values, negate }) = (test, &self.test)
        {
            // Prefixes, numeric tests and exact literals absent from the dictionary
            // still inspect the local text in a mixed list.
            let dictionary = feature.library.dictionary()?;
            let matched = values.iter().any(|literal| {
                literal.matches_bytes(&bytes)
                    && !matches!(literal, Literal::Exact(text) if dictionary.has_global_value(text))
            });
            return Ok(matched != *negate);
        }
        Ok(self.matches_local_bytes(test, &[], &bytes))
    }
    fn matches_value(&self, value: Option<TagValue<'_>>) -> bool {
        match &self.test {
            Test::Truth(want) => {
                value
                    .as_ref()
                    .is_some_and(|v| !matches!(v, TagValue::Text(s) if s == "no"))
                    == *want
            }
            Test::Equal { values, negate } => {
                value
                    .as_ref()
                    .is_some_and(|v| values.iter().any(|p| p.matches(v)))
                    != *negate
            }
            Test::Regex { pattern, negate } => {
                value.as_ref().is_some_and(
                    |v| matches!(v, TagValue::Text(s) if pattern.is_match(s.as_bytes())),
                ) != *negate
            }
            Test::Numeric {
                op,
                value: expected,
            } => value
                .as_ref()
                .and_then(number)
                .is_some_and(|actual| match op {
                    OrderingOp::Less => actual < *expected,
                    OrderingOp::LessEqual => actual <= *expected,
                    OrderingOp::Greater => actual > *expected,
                    OrderingOp::GreaterEqual => actual >= *expected,
                }),
        }
    }
}

/// A category mask and a conjunction of tag predicates.
#[derive(Clone, Debug)]
pub struct Selector {
    types: Types,
    predicates: Vec<Predicate>,
}
impl Selector {
    /// Select the given categories without a tag restriction.
    pub fn new(types: Types) -> Self {
        Self {
            types,
            predicates: Vec::new(),
        }
    }
    /// Require an additional predicate on every selected feature.
    pub fn and(mut self, predicate: Predicate) -> Self {
        self.predicates.push(predicate);
        self
    }
    fn merge_alternative(&mut self, next: &mut Self) -> bool {
        if self.types != next.types {
            return false;
        }
        let ([first], [second]) = (
            self.predicates.as_mut_slice(),
            next.predicates.as_mut_slice(),
        ) else {
            return false;
        };
        if first.key != second.key {
            return false;
        }
        let (
            Test::Equal {
                values,
                negate: false,
            },
            Test::Equal {
                values: more,
                negate: false,
            },
        ) = (&mut first.test, &mut second.test)
        else {
            return false;
        };
        // The type gate and checked tag lookup are identical. Literal tests
        // cannot fail and stay in their original order.
        values.append(more);
        true
    }

    /// Check whether a feature satisfies this selector's type and tag conditions.
    pub fn matches<S: Source>(&self, feature: Feature<'_, S>) -> Result<bool> {
        let source = feature.library.source();
        self.matches_in(
            feature,
            &mut ReadWindow::new(source),
            &mut ReadWindow::new(source),
        )
    }
    fn matches_in<S: Source>(
        &self,
        feature: Feature<'_, S>,
        tags: &mut ReadWindow<'_, S>,
        strings: &mut ReadWindow<'_, S>,
    ) -> Result<bool> {
        if !self.types.matches(feature) {
            return Ok(false);
        }
        for p in &self.predicates {
            if !p.matches(feature, tags, strings)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Reusable GOQL selectors. Compilation allocates and evaluation streams.
///
/// As in GeoDesk, the library dictionary determines which encoding to search
/// for a common key or an exact text operand. A locally stored spelling can
/// therefore be visible through [`Feature::tags`] and absent from query results.
#[derive(Clone, Debug)]
pub struct Query {
    selectors: Vec<Selector>,
}
impl Query {
    /// Construct a query from one selector.
    pub fn new(selector: Selector) -> Self {
        Self {
            selectors: vec![selector],
        }
    }
    fn from_selectors(mut selectors: Vec<Selector>) -> Self {
        selectors.dedup_by(|next, previous| previous.merge_alternative(next));
        Self { selectors }
    }

    /// Construct a query matching every feature.
    pub fn all() -> Self {
        Self::new(Selector::new(Types::ALL))
    }
    /// Include features matching the additional selector.
    pub fn or(mut self, mut selector: Selector) -> Self {
        if !self
            .selectors
            .last_mut()
            .is_some_and(|previous| previous.merge_alternative(&mut selector))
        {
            self.selectors.push(selector);
        }
        self
    }
    /// Check whether any selector in this query matches a feature.
    pub fn matches<S: Source>(&self, feature: Feature<'_, S>) -> Result<bool> {
        let source = feature.library.source();
        self.matches_in(
            feature,
            &mut ReadWindow::new(source),
            &mut ReadWindow::new(source),
        )
    }
    fn matches_in<S: Source>(
        &self,
        feature: Feature<'_, S>,
        tags: &mut ReadWindow<'_, S>,
        strings: &mut ReadWindow<'_, S>,
    ) -> Result<bool> {
        for s in &self.selectors {
            if s.matches_in(feature, tags, strings)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl<S: Source> Library<S> {
    /// Stream an unprepared query. For repeated or selective searches, see [`Query::prepare`].
    pub fn select<'a, 'q>(&'a self, query: &'q Query) -> QueryIter<'a, 'q, S> {
        QueryIter {
            features: self.features(),
            query,
        }
    }
    /// Stream an unprepared query restricted to intersecting feature bounds.
    pub fn select_in<'a, 'q>(&'a self, query: &'q Query, bounds: Bounds) -> QueryIter<'a, 'q, S> {
        QueryIter {
            features: self.in_bounds(bounds),
            query,
        }
    }

    /// Stream features matching both the GOQL query and the exact spatial filter.
    /// Stops after the first traversal or geometry error.
    pub fn select_spatial<'a, 'q>(
        &'a self,
        query: &'q Query,
        filter: &'q SpatialFilter,
    ) -> impl std::iter::FusedIterator<Item = Result<Feature<'a, S>>> + 'q
    where
        'a: 'q,
    {
        let mut features = Features::new(self, filter.candidate_bounds());
        std::iter::from_fn(move || {
            if filter.has_no_matches() {
                return None;
            }
            features.next_matching(|feature, tags, strings| {
                Ok(query.matches_in(feature, tags, strings)?
                    && filter.matches_candidate(feature)?)
            })
        })
        .fuse()
    }
}
/// Streaming results of [`Library::select`] or [`Library::select_in`].
pub struct QueryIter<'a, 'q, S> {
    features: Features<'a, S>,
    query: &'q Query,
}
impl<'a, S: Source> Iterator for QueryIter<'a, '_, S> {
    type Item = Result<Feature<'a, S>>;
    fn next(&mut self) -> Option<Self::Item> {
        self.features
            .next_matching(|feature, tags, strings| self.query.matches_in(feature, tags, strings))
    }
}
impl<S: Source> std::iter::FusedIterator for QueryIter<'_, '_, S> {}

#[cfg(test)]
mod swar_tests {
    use super::{contains_bytes, contains_bytes_in_tail};

    #[test]
    fn contains_bytes_in_tail_matches_slice_search_across_lengths_and_boundaries() {
        let needles: &[&[u8]] = &[
            b"",
            b"R",
            b"Ro",
            b"Roy",
            b"Royal",
            b"Royal Arms",
            b"aa",
            b"aba",
        ];
        for &needle in needles {
            let first = needle.first().copied().unwrap_or(0);
            let last = needle.last().copied().unwrap_or(0);
            let sf = u64::from_le_bytes([first; 8]);
            let sl = u64::from_le_bytes([last; 8]);
            for hlen in 0..=36 {
                // Suffix the buffer with the needle itself to ensure bytes
                // past `hlen` inside `tail` can never trigger a false match.
                let mut buf = vec![b'x'; hlen];
                buf.extend_from_slice(needle);
                buf.extend_from_slice(&b"Royalz".repeat(4));
                for extra_tail in [0, 1, 7, 16, 32] {
                    let tail = &buf[..buf.len().min(hlen + extra_tail)];
                    let expected = needle.is_empty()
                        || tail[..hlen].windows(needle.len()).any(|w| w == needle);
                    assert_eq!(
                        contains_bytes_in_tail(tail, hlen, needle, sf, sl),
                        expected,
                        "no-match needle={needle:?} hlen={hlen} extra={extra_tail}"
                    );
                    assert_eq!(contains_bytes(&tail[..hlen], needle), expected);
                }
                if !needle.is_empty() && hlen >= needle.len() {
                    for pos in 0..=(hlen - needle.len()) {
                        let mut buf = vec![b'R'; hlen];
                        buf[pos..pos + needle.len()].copy_from_slice(needle);
                        buf.extend_from_slice(&[b'z'; 32]);
                        for extra_tail in [0, 1, 15, 24] {
                            let tail = &buf[..buf.len().min(hlen + extra_tail)];
                            assert!(
                                contains_bytes_in_tail(tail, hlen, needle, sf, sl),
                                "match needle={needle:?} hlen={hlen} pos={pos} extra={extra_tail}"
                            );
                            assert!(contains_bytes(&tail[..hlen], needle));
                        }
                    }
                }
            }
        }
    }
}
