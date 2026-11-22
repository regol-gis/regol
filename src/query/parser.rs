use super::{Literal, OrderingOp, Predicate, Query, Selector, Test, Types};
use crate::{Error, Result};
use regex::bytes::RegexBuilder;

impl Query {
    /// Parse a GOQL selector or comma-separated union of selectors.
    pub fn parse(text: &str) -> Result<Self> {
        Parser {
            text,
            p: 0,
            regexes: 0,
            predicates: 0,
        }
        .parse()
    }
}

impl std::str::FromStr for Query {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

struct Parser<'a> {
    text: &'a str,
    p: usize,
    regexes: usize,
    predicates: usize,
}
impl Parser<'_> {
    fn error(&self, message: impl Into<String>) -> Error {
        Error::Query {
            offset: self.p,
            message: message.into(),
        }
    }
    fn peek(&self) -> Option<char> {
        self.text[self.p..].chars().next()
    }
    fn pop(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.p += c.len_utf8();
        Some(c)
    }
    fn space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.pop();
        }
    }
    fn eat(&mut self, c: char) -> bool {
        self.space();
        if self.peek() == Some(c) {
            self.pop();
            true
        } else {
            false
        }
    }
    fn token(&mut self) -> Result<String> {
        self.space();
        if matches!(self.peek(), Some('\'' | '"')) {
            let quote = self.pop().ok_or_else(|| self.error("expected quote"))?;
            let mut token = String::new();
            loop {
                let c = self
                    .pop()
                    .ok_or_else(|| self.error("unterminated quoted string"))?;
                if c == quote {
                    break;
                }
                if c == '\\' {
                    let next = self
                        .pop()
                        .ok_or_else(|| self.error("unterminated escape"))?;
                    match next {
                        '\\' | '\'' | '"' => token.push(next),
                        'n' => token.push('\n'),
                        'r' => token.push('\r'),
                        't' => token.push('\t'),
                        _ => {
                            token.push('\\');
                            token.push(next);
                        }
                    }
                } else {
                    token.push(c);
                }
            }
            Ok(token)
        } else {
            let start = self.p;
            while self
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || "_:*.-+".contains(c))
            {
                self.pop();
            }
            if self.p == start {
                return Err(self.error("expected key or value"));
            }
            Ok(self.text[start..self.p].to_owned())
        }
    }
    fn literal(&mut self) -> Result<Literal> {
        self.space();
        let quoted = matches!(self.peek(), Some('\'' | '"'));
        let token = self.token()?;
        if !quoted && let Ok(number) = token.parse::<f64>() {
            if !number.is_finite() {
                return Err(self.error("expected finite number"));
            }
            return Ok(Literal::Number(number));
        }
        if token.trim_matches('*').contains('*') {
            return Err(
                self.error("wildcards are supported only at the beginning or end of a value")
            );
        }
        Ok(Literal::text(token))
    }
    fn parse(mut self) -> Result<Query> {
        if self.text.len() > 1_048_576 {
            return Err(self.error("query exceeds 1 MiB"));
        }
        let mut selectors = Vec::new();
        loop {
            self.space();
            let mut bits = 0;
            while let Some(c) = self.peek() {
                let b = match c {
                    'n' => 1,
                    'w' => 2,
                    'a' => 4,
                    'r' => 8,
                    '*' => 15,
                    _ => break,
                };
                bits |= b;
                self.pop();
            }
            if bits == 0 {
                return Err(self.error("expected feature type: n, w, a, r or *"));
            }
            if selectors.len() >= 256 {
                return Err(self.error("query exceeds 256 selectors"));
            }
            let mut selector = Selector::new(Types(bits));
            while self.eat('[') {
                self.predicates += 1;
                if self.predicates > 1024 {
                    return Err(self.error("query exceeds 1024 predicates"));
                }
                let negated = self.eat('!');
                let key = self.token()?;
                if self.eat(']') {
                    selector.predicates.push(if negated {
                        Predicate::falsy(key)
                    } else {
                        Predicate::truthy(key)
                    });
                    continue;
                }
                if negated {
                    return Err(self.error("unary ! cannot be combined with a comparison"));
                }
                self.space();
                let start = self.p;
                while self.peek().is_some_and(|c| "=!~<>".contains(c)) {
                    self.pop();
                }
                let operator = &self.text[start..self.p];
                let test = match operator {
                    "=" | "!=" => {
                        let mut values = vec![self.literal()?];
                        while self.eat(',') {
                            values.push(self.literal()?);
                        }
                        Test::Equal {
                            values,
                            negate: operator == "!=",
                        }
                    }
                    "<" | "<=" | ">" | ">=" => {
                        let value: f64 = self
                            .token()?
                            .parse()
                            .map_err(|_| self.error("expected finite number"))?;
                        if !value.is_finite() {
                            return Err(self.error("expected finite number"));
                        }
                        let op = match operator {
                            "<" => OrderingOp::Less,
                            "<=" => OrderingOp::LessEqual,
                            ">" => OrderingOp::Greater,
                            _ => OrderingOp::GreaterEqual,
                        };
                        Test::Numeric { op, value }
                    }
                    "~" | "!~" => {
                        self.regexes += 1;
                        if self.regexes > 32 {
                            return Err(self.error("query exceeds 32 regular expressions"));
                        }
                        let text = self.token()?;
                        let pattern = RegexBuilder::new(&format!("\\A(?:{text})\\z"))
                            .size_limit(262144)
                            .dfa_size_limit(262144)
                            .nest_limit(64)
                            .build()
                            .map_err(|e| {
                                self.error(format!(
                                    "unsupported or invalid regular expression: {e}"
                                ))
                            })?;
                        Test::Regex {
                            pattern,
                            negate: operator == "!~",
                        }
                    }
                    _ => return Err(self.error("unsupported comparison operator")),
                };
                if !self.eat(']') {
                    return Err(self.error("expected ]"));
                }
                selector.predicates.push(Predicate { key, test });
            }
            selectors.push(selector);
            if !self.eat(',') {
                break;
            }
        }
        self.space();
        if self.peek().is_some() {
            return Err(self.error("unexpected trailing input"));
        }
        Ok(Query::from_selectors(selectors))
    }
}
