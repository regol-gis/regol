//! Element evaluators used by Overpass QL's `(if:...)` filter.
use regex::Regex;
use regol::{Feature, TagValue, source::Source};
use serde::Deserialize;
use std::{borrow::Cow, cmp::Ordering, sync::LazyLock};

const MAX_NODES: usize = 128;
const MAX_DEPTH: usize = 32;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Condition {
    Value {
        value: String,
    },
    Tag {
        key: String,
    },
    Not {
        arg: Box<Self>,
    },
    Number {
        arg: Box<Self>,
    },
    IsNumber {
        arg: Box<Self>,
    },
    Binary {
        operator: Operator,
        left: Box<Self>,
        right: Box<Self>,
    },
}

#[derive(Deserialize)]
pub(super) enum Operator {
    #[serde(rename = "||")]
    Or,
    #[serde(rename = "&&")]
    And,
    #[serde(rename = "==")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
}

pub(super) fn validate(conditions: &[Condition]) -> Result<(), String> {
    let mut remaining = MAX_NODES;
    for condition in conditions {
        condition.validate(1, &mut remaining)?;
    }
    Ok(())
}

impl Condition {
    fn validate(&self, depth: usize, remaining: &mut usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("Per-feature condition nesting limit reached.".into());
        }
        *remaining = remaining
            .checked_sub(1)
            .ok_or("Per-feature condition limit reached.")?;
        match self {
            Self::Value { .. } | Self::Tag { .. } => Ok(()),
            Self::Not { arg } | Self::Number { arg } | Self::IsNumber { arg } => {
                arg.validate(depth + 1, remaining)
            }
            Self::Binary { left, right, .. } => {
                left.validate(depth + 1, remaining)?;
                right.validate(depth + 1, remaining)
            }
        }
    }

    pub(super) fn matches<S: Source>(&self, feature: Feature<'_, S>) -> regol::Result<bool> {
        Ok(truthy(&self.evaluate(feature)?))
    }

    fn evaluate<'a, S: Source>(&'a self, feature: Feature<'a, S>) -> regol::Result<Cow<'a, str>> {
        Ok(match self {
            Self::Value { value } => Cow::Borrowed(value),
            Self::Tag { key } => match feature.tag(key)? {
                Some(TagValue::Text(value)) => value,
                Some(value) => Cow::Owned(value.to_string()),
                None => Cow::Borrowed(""),
            },
            Self::Not { arg } => boolean(!truthy(&arg.evaluate(feature)?)),
            Self::Number { arg } => match number(&arg.evaluate(feature)?) {
                Some(Number::Integer(value)) => Cow::Owned(value.to_string()),
                Some(Number::Float(value)) => Cow::Owned(format_number(value)),
                None => Cow::Borrowed("NaN"),
            },
            Self::IsNumber { arg } => boolean(number(&arg.evaluate(feature)?).is_some()),
            Self::Binary {
                operator,
                left,
                right,
            } => {
                let left = left.evaluate(feature)?;
                let right = right.evaluate(feature)?;
                boolean(operator.evaluate(&left, &right))
            }
        })
    }
}

impl Operator {
    fn evaluate(&self, left: &str, right: &str) -> bool {
        match self {
            Self::Or => truthy(left) || truthy(right),
            Self::And => truthy(left) && truthy(right),
            _ => {
                let order = compare(left, right);
                match self {
                    Self::Eq => order == Some(Ordering::Equal),
                    Self::Ne => order != Some(Ordering::Equal),
                    Self::Lt => order == Some(Ordering::Less),
                    Self::Le => matches!(order, Some(Ordering::Less | Ordering::Equal)),
                    Self::Gt => order == Some(Ordering::Greater),
                    Self::Ge => matches!(order, Some(Ordering::Greater | Ordering::Equal)),
                    Self::Or | Self::And => unreachable!(),
                }
            }
        }
    }
}

fn boolean(value: bool) -> Cow<'static, str> {
    Cow::Borrowed(if value { "1" } else { "0" })
}

fn truthy(value: &str) -> bool {
    !value.is_empty() && float(value) != Some(0.0)
}

fn compare(left: &str, right: &str) -> Option<Ordering> {
    if let (Some(left), Some(right)) = (integer(left), integer(right)) {
        return Some(left.cmp(&right));
    }
    if let (Some(left), Some(right)) = (float(left), float(right)) {
        return left.partial_cmp(&right);
    }
    Some(left.cmp(right))
}

fn leading_space(value: &str) -> &str {
    value.trim_start_matches(|c| matches!(c, ' ' | '\t'..='\r'))
}

// Overpass uses full base-0 C integer parsing: a leading zero denotes octal,
// and a fully consumed out-of-range integer is clamped to the signed limit.
fn integer(value: &str) -> Option<i64> {
    let value = leading_space(value);
    let negative = value.starts_with('-');
    let digits = value.strip_prefix(['-', '+']).unwrap_or(value);
    if !digits.starts_with('0')
        && let Ok(integer) = value.parse()
    {
        return Some(integer);
    }
    let (digits, radix) = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        (hex, 16)
    } else if digits.starts_with('0') {
        (digits, 8)
    } else {
        (digits, 10)
    };
    if digits.is_empty() {
        return None;
    }
    let mut magnitude = 0u64;
    for digit in digits.chars() {
        magnitude = magnitude
            .saturating_mul(u64::from(radix))
            .saturating_add(u64::from(digit.to_digit(radix)?));
    }
    Some(if negative {
        -(magnitude.min(i64::MAX as u64) as i64) - i64::from(magnitude > i64::MAX as u64)
    } else {
        magnitude.min(i64::MAX as u64) as i64
    })
}

static FLOAT_PREFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i-u)^[+-]?(?:inf(?:inity)?|nan(?:\([a-z_0-9]*\))?|",
        r"0x(?:[0-9a-f]+(?:\.[0-9a-f]*)?|\.[0-9a-f]+)(?:p[+-]?[0-9]+)?|",
        r"(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:e[+-]?[0-9]+)?)",
    ))
    .expect("valid pattern for a numeric prefix")
});

fn float(value: &str) -> Option<f64> {
    let value = leading_space(value);
    value.parse().ok().or_else(|| {
        let unsigned = value.strip_prefix(['-', '+']).unwrap_or(value);
        if !unsigned.starts_with("0x")
            && !unsigned.starts_with("0X")
            && !unsigned
                .get(..4)
                .is_some_and(|start| start.eq_ignore_ascii_case("nan("))
        {
            return None;
        }
        let prefix = FLOAT_PREFIX.find(value)?.as_str();
        (prefix.len() == value.len()).then(|| parse_float_prefix(prefix).0)
    })
}

// The second result is true when the C prefix conversion succeeds without ERANGE.
fn parse_float_prefix(value: &str) -> (f64, bool) {
    let negative = value.starts_with('-');
    let unsigned = value.strip_prefix(['-', '+']).unwrap_or(value);
    let (number, in_range) = if let Some(hex) = unsigned
        .strip_prefix("0x")
        .or_else(|| unsigned.strip_prefix("0X"))
    {
        hex_float(hex)
    } else if unsigned.starts_with(['i', 'I']) {
        (f64::INFINITY, true)
    } else if unsigned.starts_with(['n', 'N']) {
        (f64::NAN, true)
    } else {
        let value = unsigned.parse::<f64>().expect("decimal numeric prefix");
        (value, decimal_in_range(unsigned, value))
    };
    (if negative { -number } else { number }, in_range)
}

fn hex_float(value: &str) -> (f64, bool) {
    let (mantissa, exponent) = value.split_once(['p', 'P']).unwrap_or((value, "0"));
    let exponent = exponent.parse::<i32>().unwrap_or_else(|_| {
        if exponent.starts_with('-') {
            i32::MIN
        } else {
            i32::MAX
        }
    });
    let integer_digits = mantissa.find('.').unwrap_or(mantissa.len());
    // Keep enough bits to round to a double, and track the discarded bits.
    let mut significand = 0u64;
    let mut used = 0usize;
    let mut discarded = false;
    for digit in mantissa.chars().filter_map(|c| c.to_digit(16)) {
        if significand < 1 << 56 {
            significand = significand * 16 + u64::from(digit);
            used += 1;
        } else {
            discarded |= digit != 0;
        }
    }
    if significand == 0 {
        return (0.0, true);
    }
    let scale =
        (i64::from(exponent) + 4 * (integer_digits as i64 - used as i64)).clamp(-1200, 1200) as i32;
    let significant_bits = significand.ilog2() as i32 + 1;
    // Round once to a 53-bit significand or to the subnormal spacing of 2^-1074.
    let shift = 0.max(significant_bits - 53).max(-1074 - scale);
    if shift >= 64 {
        return (0.0, false);
    }
    let mut rounded = significand >> shift;
    let remainder = significand - (rounded << shift);
    let half = (1u64 << shift) / 2;
    if shift > 0
        && (remainder > half || remainder == half && (discarded || !rounded.is_multiple_of(2)))
    {
        rounded += 1;
    }
    let value = rounded as f64
        * match scale + shift {
            exponent @ -1074..=-1023 => f64::from_bits(1 << (exponent + 1074)),
            exponent @ -1022..=1023 => f64::from_bits(((exponent + 1023) as u64) << 52),
            _ => f64::INFINITY,
        };
    let inexact = remainder != 0 || discarded;
    let tiny = scale + significant_bits - 1 < -1022;
    // glibc also reports underflow if rounding at normal precision would
    // stay below the minimum normal before the value is rounded to binary64.
    let below_normal_cutoff = tiny && (u128::from(remainder) << 2) < (3u128 << shift);
    let underflow = inexact && (value < f64::MIN_POSITIVE || below_normal_cutoff);
    (value, value.is_finite() && !underflow)
}

// Normalized decimal digits in lexicographically comparable scientific notation.
fn decimal_digits(text: &str) -> (i64, String) {
    let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let exponent = exponent.parse::<i64>().unwrap_or_else(|_| {
        if exponent.starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        }
    });
    let point = mantissa.find('.').unwrap_or(mantissa.len()) as i64;
    let digits = mantissa.replace('.', "");
    let trimmed = digits.trim_start_matches('0');
    let leading = (digits.len() - trimmed.len()) as i64;
    (
        exponent.saturating_add(point - leading - 1),
        trimmed.trim_end_matches('0').into(),
    )
}

static MIN_NORMAL_CUTOFF: LazyLock<(i64, String)> = LazyLock::new(|| {
    // The cutoff is one quarter of a subnormal step below the minimum normal.
    // Its exact decimal expansion is (2 * minimum + next_down(2 * minimum)) / 4.
    let twice = 2.0 * f64::MIN_POSITIVE;
    let below_twice = twice.next_down();
    let mut digits: Vec<_> = format!("{twice:.1076}")
        .bytes()
        .zip(format!("{below_twice:.1076}").bytes())
        .filter(|&(left, _)| left != b'.')
        .map(|(left, right)| left - b'0' + right - b'0')
        .collect();
    let mut carry = 0;
    for digit in digits.iter_mut().rev() {
        let sum = *digit + carry;
        *digit = sum % 10;
        carry = sum / 10;
    }
    let mut remainder = 0;
    let mut cutoff: String = digits
        .into_iter()
        .map(|digit| {
            let value = remainder * 10 + digit;
            remainder = value % 4;
            char::from(b'0' + value / 4)
        })
        .collect();
    cutoff.insert(1, '.');
    decimal_digits(&cutoff)
});

fn decimal_in_range(text: &str, value: f64) -> bool {
    if value > f64::MIN_POSITIVE {
        return value.is_finite();
    }
    if value == 0.0 {
        let mantissa = text.split(['e', 'E']).next().expect("decimal mantissa");
        return mantissa.bytes().all(|byte| matches!(byte, b'0' | b'.'));
    }
    // Full comparisons use even an underflowed value. The prefix functions
    // mirror glibc's ERANGE for inexact values below the normal cutoff.
    let digits = decimal_digits(text);
    if value == f64::MIN_POSITIVE {
        digits >= *MIN_NORMAL_CUTOFF
    } else {
        digits == decimal_digits(&format!("{value:.1074}"))
    }
}

enum Number {
    Integer(i64),
    Float(f64),
}

fn number(value: &str) -> Option<Number> {
    if let Some(value) = integer(value) {
        return Some(Number::Integer(value));
    }
    let prefix = FLOAT_PREFIX.find(leading_space(value))?.as_str();
    let (value, in_range) = parse_float_prefix(prefix);
    in_range.then_some(Number::Float(value))
}

// Overpass normalizes floating-point strings to fourteen significant digits.
fn format_number(value: f64) -> String {
    if !value.is_finite() {
        return format!(
            "{}{}",
            if value.is_sign_negative() { "-" } else { "" },
            if value.is_nan() { "nan" } else { "inf" }
        );
    }
    let scientific = format!("{value:.13e}");
    let (mantissa, exponent) = scientific.split_once('e').expect("scientific notation");
    let exponent: i32 = exponent.parse().expect("decimal exponent");
    if !(-4..14).contains(&exponent) {
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        return format!("{mantissa}e{exponent:+03}");
    }
    let precision = (13 - exponent) as usize;
    let decimal = format!("{value:.precision$}");
    if precision > 0 {
        return decimal.trim_end_matches('0').trim_end_matches('.').into();
    }
    decimal
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn comparison_tries_integers_then_doubles_then_text() {
        for (left, right, expected) in [
            ("9007199254740992", "9007199254740993", Ordering::Less),
            ("010", "8", Ordering::Equal),
            ("010", "10.0", Ordering::Equal),
            ("1.", "1", Ordering::Equal),
            ("  +0x10", "16", Ordering::Equal),
            ("0x1.8p1", "3", Ordering::Equal),
            (
                "777777777777777777777",
                "9223372036854775807",
                Ordering::Equal,
            ),
            ("  -2.5e1", "-25", Ordering::Equal),
            ("20 mph", "100", Ordering::Greater),
            ("100 ", "20", Ordering::Less),
            ("", "0", Ordering::Less),
        ] {
            assert_eq!(compare(left, right), Some(expected), "{left:?} {right:?}");
        }
        assert_eq!(compare("NaN", "0"), None);
        assert_eq!(compare("NaN", "NaN"), None);
        assert!(!Operator::Ge.evaluate("NaN", "0"));
        assert!(Operator::Ne.evaluate("NaN", "NaN"));
    }

    #[test]
    fn number_normalizes_a_numeric_prefix_and_truthiness_uses_whole_values() {
        for (source, expected) in [
            ("010", "8"),
            ("010 mph", "10"),
            ("  -10.25 mph", "-10.25"),
            ("5;10", "5"),
            ("1.234567890123456 km", "1.2345678901235"),
            ("1e-5", "1e-05"),
            ("0x1.8p+1 m", "3"),
        ] {
            let got = match number(source) {
                Some(Number::Integer(value)) => value.to_string(),
                Some(Number::Float(value)) => format_number(value),
                None => panic!("Expected a prefix in {source:?}"),
            };
            assert_eq!(got, expected);
        }
        for value in ["", "none", "1e9999", "1e-9999"] {
            assert!(number(value).is_none(), "{value:?}");
        }
        for value in ["", "0", "-0.000", " 0e9", "0x0p+5"] {
            assert!(!truthy(value), "{value:?}");
        }
        for value in ["no", "0 ", "0 mph", "NaN"] {
            assert!(truthy(value), "{value:?}");
        }
    }

    #[test]
    fn subnormal_prefixes_require_exact_conversion() {
        let smallest = f64::from_bits(1);
        let exact_decimal = format!("{smallest:.1074} m");
        for prefix in ["0x1p-1074 m", exact_decimal.as_str()] {
            assert!(matches!(number(prefix), Some(Number::Float(value)) if value == smallest));
        }
        for prefix in [
            "0x3p-1075",
            "0x1.0000000000001p-1074",
            "5e-324",
            "2.2250738585072012e-308",
            "0x0.fffffffffffff8p-1022",
            "0x0.fffffffffffff80001p-1022",
        ] {
            assert!(number(prefix).is_none(), "{prefix}");
            assert!(float(prefix).unwrap() > 0.0, "{prefix}");
        }
        for prefix in ["2.2250738585072013e-308", "0x0.fffffffffffffcp-1022"] {
            assert!(
                matches!(number(prefix), Some(Number::Float(value)) if value == f64::MIN_POSITIVE)
            );
        }
    }

    #[test]
    fn hex_float_rounds_directly_to_the_available_precision() {
        assert_eq!(float("0x1p-1075"), Some(0.0));
        assert_eq!(float("0x1.0000000000000001p-1075"), Some(f64::from_bits(1)));
        assert_eq!(float("0x2.fffffffffffffffp-1075"), Some(f64::from_bits(1)));
        assert_eq!(float("0x3p-1075"), Some(f64::from_bits(2)));
        assert_eq!(float("0x1.00000000000008p0"), Some(1.0));
        assert_eq!(float("0x1.00000000000008001p0"), Some(1.0f64.next_up()));
        assert_eq!(float("0x1.fffffffffffff8p1023"), Some(f64::INFINITY));
    }

    #[test]
    fn direct_plans_have_the_same_condition_limits() {
        let mut expr = json!({"op": "value", "value": "1"});
        for _ in 0..MAX_DEPTH {
            expr = json!({"op": "not", "arg": expr});
        }
        let expr = serde_json::from_str(&expr.to_string()).unwrap();
        assert!(validate(&[expr]).unwrap_err().contains("nesting limit"));
        let leaves: Vec<Condition> = (0..=MAX_NODES)
            .map(|_| Condition::Value { value: "1".into() })
            .collect();
        assert!(validate(&leaves).unwrap_err().contains("condition limit"));
    }
}
