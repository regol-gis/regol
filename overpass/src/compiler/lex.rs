use std::sync::LazyLock;

use regex::Regex;

use super::Result;

// Match the Overpass token stream without letting quoted punctuation or comments
// leak into the statement parser. Its whitespace and name syntax follow Live.
static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?x)
        [\x09-\x0D\x20\xA0\u1680\u2000-\u200A\u2028\u2029\u202F\u205F\u3000\uFEFF]+
        | //[^\n]* | /\*(?s:.*?)(?:\*/|$)
        | "(?:\\(?s:.)|[^"\\])*" | '(?:\\(?s:.)|[^'\\])*'
        | \{\{bbox\}\} | -> | == | != | !~ | <= | >= | && | \|\|
        | [+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?
        | [A-Za-z_][A-Za-z_0-9]* | (?s:.)
        "#,
    )
    .expect("Overpass token pattern")
});

static BARE_LITERAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z_0-9+-]+(?:\.[0-9]+)?$").expect("Overpass tag pattern"));

static DECIMAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[+-]?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$")
        .expect("Overpass number pattern")
});

pub(super) fn whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | ' '
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

pub(super) fn is_decimal(text: &str) -> bool {
    DECIMAL.is_match(text)
}

pub(super) fn is_word(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes
        .next()
        .is_some_and(|b| b == b'_' || b.is_ascii_alphabetic())
        && bytes.all(|b| b == b'_' || b.is_ascii_alphanumeric())
}

pub(super) fn is_quoted(text: &str) -> bool {
    text.starts_with(['"', '\''])
}

pub(super) fn diagnostic(line: usize, message: impl std::fmt::Display) -> String {
    format!("Line {line}: {message}")
}

struct Token<'text> {
    raw: &'text str,
    line: usize,
}

pub(super) struct Tokens<'text> {
    items: Vec<Token<'text>>,
    position: usize,
}

impl<'text> Tokens<'text> {
    pub(super) fn new(text: &'text str) -> Result<Self> {
        let mut items = Vec::new();
        let mut line = 1;
        for found in TOKEN.find_iter(text) {
            let raw = found.as_str();
            let start = line;
            line += raw.bytes().filter(|&byte| byte == b'\n').count();
            if raw.starts_with("/*") && !raw.ends_with("*/") {
                return Err(diagnostic(start, "unclosed comment."));
            }
            if raw.starts_with(whitespace) || raw.starts_with("//") || raw.starts_with("/*") {
                continue;
            }
            items.push(Token { raw, line: start });
        }
        items.push(Token { raw: "", line });
        Ok(Self { items, position: 0 })
    }

    pub(super) fn peek(&self) -> &'text str {
        self.items[self.position].raw
    }

    pub(super) fn ahead(&self, count: usize) -> Option<&'text str> {
        self.items.get(self.position + count).map(|token| token.raw)
    }

    pub(super) fn line(&self) -> usize {
        self.items[self.position].line
    }

    pub(super) fn error(&self, message: impl std::fmt::Display) -> String {
        diagnostic(self.line(), message)
    }

    pub(super) fn take(&mut self, raw: &str) -> bool {
        if self.peek() != raw {
            return false;
        }
        self.position += 1;
        true
    }

    pub(super) fn expect(&mut self, raw: &str) -> Result<()> {
        if self.take(raw) {
            Ok(())
        } else {
            Err(self.error(format_args!("Expected '{raw}'.")))
        }
    }

    pub(super) fn word(&mut self) -> Result<&'text str> {
        let raw = self.peek();
        if !is_word(raw) {
            return Err(self.error("Expected a name."));
        }
        self.position += 1;
        Ok(raw)
    }

    pub(super) fn literal(&mut self, regex: bool) -> Result<String> {
        let raw = self.peek();
        if is_quoted(raw) {
            if raw.len() < 2 {
                return Err(self.error("Unterminated quoted string."));
            }
            let line = self.line();
            self.position += 1;
            return unquote(&raw[1..raw.len() - 1], regex)
                .ok_or_else(|| diagnostic(line, "Unsupported string escape."));
        }
        if !BARE_LITERAL.is_match(raw) {
            return Err(self.error("Expected a tag key or value."));
        }
        self.position += 1;
        Ok(raw.to_owned())
    }

    pub(super) fn number(&mut self) -> Result<f64> {
        let raw = self.peek();
        let number = raw
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite())
            .ok_or_else(|| self.error("Expected a finite number."))?;
        self.position += 1;
        Ok(number)
    }
}

fn unquote(raw: &str, regex: bool) -> Option<String> {
    if !raw.contains('\\') {
        return Some(raw.to_owned());
    }
    let mut units = Vec::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        let c = if c == '\\' {
            match chars.next()? {
                'u' => {
                    let rest = chars.as_str().as_bytes();
                    if rest
                        .get(..4)
                        .is_some_and(|code| code.iter().all(u8::is_ascii_hexdigit))
                    {
                        units.push(u16::from_str_radix(&chars.as_str()[..4], 16).ok()?);
                        chars.by_ref().take(4).for_each(drop);
                        continue;
                    }
                    if !regex {
                        return None;
                    }
                    'u'
                }
                'n' => '\n',
                't' => '\t',
                escaped if regex => escaped,
                'r' => '\r',
                escaped @ ('"' | '\'' | '\\') => escaped,
                _ => return None,
            }
        } else {
            c
        };
        units.extend_from_slice(c.encode_utf16(&mut [0; 2]));
    }
    String::from_utf16(&units).ok()
}
